use super::*;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use config::{Config as ConfigLoader, File, FileFormat};

impl Config {
    pub(crate) fn parse(text: &str) -> Result<Self> {
        parse_layers("", text)
    }
}

fn parse_layers(global: &str, project: &str) -> Result<Config> {
    let builder = ConfigLoader::builder()
        .add_source(File::from_str(global, FileFormat::Toml))
        .add_source(File::from_str(project, FileFormat::Toml));
    let mut config = builder.build()?.try_deserialize::<Config>()?;
    config.resolve_paths(Path::new("/project"));
    Ok(config)
}

#[test]
fn config_path_follows_xdg_and_home_precedence() {
    for (xdg, home, expected) in [
        (
            Some("/xdg"),
            Some("/home/user"),
            Some("/xdg/silo/config.toml"),
        ),
        (
            None,
            Some("/home/user"),
            Some("/home/user/.config/silo/config.toml"),
        ),
        (
            Some(""),
            Some("/home/user"),
            Some("/home/user/.config/silo/config.toml"),
        ),
        (
            Some("relative/xdg"),
            Some("/home/user"),
            Some("/home/user/.config/silo/config.toml"),
        ),
        (None, Some(""), None),
        (None, Some("relative/home"), None),
        (None, None, None),
    ] {
        let path = config_path_from(xdg.map(OsStr::new), home.map(OsStr::new));
        assert_eq!(path.as_deref(), expected.map(Path::new));
    }
}

#[test]
fn project_scalars_override_and_omitted_values_inherit() {
    let config = parse_layers(
        "container.cpus = 6\ncontainer.memory = \"8G\"\ncontainer.sudo = true\nshell = \"bash\"\n",
        "container.memory = \"16G\"\ncontainer.sudo = false\nshell = \"fish\"\n",
    )
    .expect("configuration layers merge");

    assert_eq!(config.container.cpus, Some(6));
    assert_eq!(config.container.memory.as_deref(), Some("16G"));
    assert!(!config.container.sudo);
    assert_eq!(config.shell, Some(Shell::Fish));
}

#[test]
fn project_arrays_replace_global_arrays() {
    let config = parse_layers(
        "workspace.read_only = [\".git\", \".github\"]\n",
        "workspace.read_only = [\"policy\"]\n",
    )
    .expect("configuration layers merge");

    assert_eq!(config.workspace.read_only, [PathBuf::from("policy")]);
}

#[test]
fn named_tables_merge_recursively() {
    let config = parse_layers(
        "[quick]\nbuild = [\"cargo\", \"build\"]\n",
        "[quick]\nbuild = [\"just\", \"build\"]\ntest = [\"cargo\", \"test\"]\n",
    )
    .expect("configuration layers merge");

    assert_eq!(config.quick["build"], ["just", "build"]);
    assert_eq!(config.quick["test"], ["cargo", "test"]);
}

#[test]
fn project_host_ports_replace_global_host_ports() {
    let config = parse_layers("host_ports = [5432, 6379]\n", "host_ports = [3000, 8080]\n")
        .expect("configuration layers merge");

    assert_eq!(config.host_ports, BTreeSet::from([3000, 8080]));
}

#[test]
fn project_env_vars_replace_the_global_allowlist() {
    let config = parse_layers(
        "env_vars = [\"GLOBAL_TOKEN\", \"AWS_PROFILE\"]\n",
        "env_vars = [\"PROJECT_TOKEN\", \"PROJECT_TOKEN\"]\n",
    )
    .expect("environment allowlists merge");

    assert_eq!(
        config.env_vars,
        BTreeSet::from(["PROJECT_TOKEN".to_string()])
    );
}

#[test]
fn merged_relative_paths_resolve_from_the_project_root() {
    let base = Path::new("/project");
    let config = parse_layers(
        "image.dockerfile = \"containers/Dockerfile\"\n",
        "[binds.cache]\nsource = \"project-cache\"\ntarget = \"/cache\"\naccess = \"read-only\"\n",
    )
    .expect("configuration layers merge");

    assert_eq!(
        config.image.dockerfile,
        Some(base.join("containers/Dockerfile"))
    );
    assert_eq!(config.binds["cache"].source, base.join("project-cache"));
}

#[test]
fn path_resolution_preserves_values_for_semantic_validation() {
    let config = parse_layers(
        "image.dockerfile = \"\"\n",
        "[binds.home]\nsource = \"~/cache\"\ntarget = \"/cache\"\naccess = \"read-only\"\n\
         [binds.invalid-home]\nsource = \"~user/cache\"\ntarget = \"/invalid\"\naccess = \"read-only\"\n",
    )
    .expect("configuration layers merge");

    assert_eq!(config.image.dockerfile.as_deref(), Some(Path::new("")));
    assert_eq!(config.binds["home"].source, Path::new("~/cache"));
    assert_eq!(
        config.binds["invalid-home"].source,
        Path::new("~user/cache")
    );
}

#[test]
fn unknown_keys_are_rejected() {
    let error = Config::parse("future_key = 42\n")
        .expect_err("unknown keys are invalid")
        .to_string();
    assert!(error.contains("unknown field `future_key`"), "{error}");
}

#[test]
fn empty_config_uses_defaults() {
    assert_eq!(
        Config::parse("").expect("empty config parses"),
        Config::default()
    );
}

#[test]
fn shell_accepts_every_builtin_choice() {
    for (name, expected) in [
        ("bash", Shell::Bash),
        ("zsh", Shell::Zsh),
        ("fish", Shell::Fish),
        ("nu", Shell::Nu),
    ] {
        let config = Config::parse(&format!("shell = {name:?}")).expect("supported shell parses");
        assert_eq!(config.shell, Some(expected));
    }
}

#[test]
fn shell_rejects_unknown_choices() {
    let error = Config::parse("shell = \"powershell\"").expect_err("unknown shell is invalid");
    let message = error.to_string();
    assert!(message.contains("powershell"), "{message}");
    assert!(message.contains("shell"), "{message}");
}

#[test]
fn read_only_can_be_disabled() {
    let config = Config::parse("workspace.read_only = []").expect("config parses");
    assert!(config.workspace.read_only.is_empty());
}

#[test]
fn default_config_file_matches_builtin_defaults() {
    let from_file = Config::parse(DEFAULT_CONFIG).expect("default file parses");
    assert_eq!(from_file, Config::default());
}

#[test]
fn mount_entries_require_category_fields() {
    let state = Config::parse("[state.user.optional]\n")
        .expect_err("state target is required")
        .to_string();
    assert!(state.contains("state.user.optional.target"), "{state}");

    let bind = Config::parse("[binds.missing]\ntarget = \"/somewhere\"\n")
        .expect_err("bind source is required")
        .to_string();
    assert!(bind.contains("binds.missing.source"), "{bind}");
}

#[test]
fn mount_entries_reject_unknown_fields() {
    let error = Config::parse(
        "[binds.notes]\nsource = \"~/notes\"\ntarget = \"~/notes\"\naccess = \"read-only\"\nfuture_key = 42\n",
    )
    .expect_err("unknown mount field fails")
    .to_string();

    assert!(error.contains("unknown field `future_key`"), "{error}");
}

#[test]
fn host_ports_render_as_a_sorted_unique_allowlist() {
    let config = Config::parse("host_ports = [8080, 5432, 8080]\n").expect("host ports parse");

    assert_eq!(config.host_ports, BTreeSet::from([5432, 8080]));
    assert_eq!(
        toml::to_string_pretty(&config).expect("host ports serialize"),
        "host_ports = [\n    5432,\n    8080,\n]\n\n[workspace]\nread_only = [\".git\"]\n"
    );
}

#[test]
fn env_vars_render_as_a_sorted_unique_allowlist() {
    let config =
        Config::parse("env_vars = [\"OPENAI_API_KEY\", \"AWS_PROFILE\", \"OPENAI_API_KEY\"]\n")
            .expect("environment allowlist parses");

    assert_eq!(
        config.env_vars,
        BTreeSet::from(["AWS_PROFILE".to_string(), "OPENAI_API_KEY".to_string(),])
    );
    let text = toml::to_string_pretty(&config).expect("environment allowlist serializes");
    assert!(
        text.contains("env_vars = [\n    \"AWS_PROFILE\",\n    \"OPENAI_API_KEY\",\n]"),
        "{text}"
    );
}

#[test]
fn host_ports_reject_values_outside_the_schema_range() {
    let error = Config::parse("host_ports = [65536]\n")
        .expect_err("host ports must fit in an unsigned 16-bit integer")
        .to_string();

    assert!(error.contains("65536"), "{error}");
}

#[test]
fn removed_forward_table_is_rejected() {
    let error = Config::parse("[forward]\npostgres = { port = 5432 }\n")
        .expect_err("removed forwarding schema is invalid")
        .to_string();

    assert!(error.contains("unknown field `forward`"), "{error}");
}

#[test]
fn default_config_serializes_without_empty_tables() {
    let text = toml::to_string_pretty(&Config::default()).expect("default config serializes");

    assert_eq!(text, "[workspace]\nread_only = [\".git\"]\n");
}

#[test]
fn effective_config_round_trips_through_toml() {
    let config = Config::parse(
        "image.dockerfile = \"/tmp/Dockerfile\"\n\
         container.cpus = 4\n\
         container.memory = \"4G\"\n\
         container.sudo = true\n\
         shell = \"fish\"\n\
         env_vars = [\"OPENAI_API_KEY\", \"AWS_PROFILE\"]\n\
         workspace.read_only = [\"policy\"]\n\
         host_ports = [5432, 8080]\n\
         [binds.docs]\n\
         source = \"/tmp/docs\"\n\
         target = \"~/docs\"\n\
         access = \"read-only\"\n\
         [binds.output]\n\
         source = \"/tmp/output\"\n\
         target = \"./output\"\n\
         access = \"read-write\"\n\
         [state.project.cargo]\n\
         target = \"~/.cargo\"\n\
         [state.user.codex]\n\
         target = \"~/.codex\"\n\
         [quick]\n\
         test = [\"cargo\", \"test\"]\n",
    )
    .expect("config parses");

    let text = toml::to_string_pretty(&config).expect("effective config serializes");
    let reparsed = Config::parse(&text).expect("printed config parses again");

    assert_eq!(reparsed, config);
}

#[test]
fn serialization_preserves_escaped_quick_names() {
    let config = Config::parse(
        "[quick]\n\
         zeta = [\"zeta\"]\n\
         \"review.code\" = [\"codex\", \"review\"]\n",
    )
    .expect("quick commands parse");

    let text = toml::to_string_pretty(&config).expect("config serializes");

    assert!(text.contains("\"review.code\" = ["), "{text}");
    Config::parse(&text).expect("serialized quick commands parse again");
}
