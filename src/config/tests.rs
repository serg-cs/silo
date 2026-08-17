use super::*;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

/// Temporary directory that removes itself on drop, so cleanup also runs on
/// test failures.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("silo-config-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test dir creation succeeds");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn empty_config_uses_builtin_image() {
    let config = Config::parse("").expect("empty config parses");
    assert!(config.image.dockerfile.is_none());
}

#[test]
fn container_settings_default_to_safe_runtime_values() {
    let config = Config::parse("").expect("empty config parses");
    assert_eq!(config.container, Container::default());
    assert_eq!(config.container.cpus, None);
    assert_eq!(config.container.memory, None);
    assert!(!config.container.sudo);
}

#[test]
fn container_settings_are_configurable() {
    let config = Config::parse("[container]\ncpus = 8\nmemory = \"32G\"\nsudo = true\n")
        .expect("container resources parse");
    assert_eq!(config.container.cpus, Some(8));
    assert_eq!(config.container.memory.as_deref(), Some("32G"));
    assert!(config.container.sudo);
}

#[test]
fn container_resources_reject_impossible_values() {
    let cpu = Config::parse("[container]\ncpus = 0\n")
        .expect_err("zero CPUs cannot run a container")
        .to_string();
    assert!(cpu.contains("container.cpus"), "{cpu}");

    let memory = Config::parse("[container]\nmemory = \"  \"\n")
        .expect_err("empty memory cannot configure a container")
        .to_string();
    assert!(memory.contains("container.memory"), "{memory}");
}

#[test]
fn container_overrides_replace_only_supplied_values() {
    let mut config = Config::parse("[container]\ncpus = 8\nmemory = \"32G\"\n")
        .expect("container resources parse");

    config.apply_container_overrides(Some(4), None, false);
    assert_eq!(config.container.cpus, Some(4));
    assert_eq!(config.container.memory.as_deref(), Some("32G"));

    config.apply_container_overrides(None, Some("16G".to_string()), true);
    assert_eq!(config.container.cpus, Some(4));
    assert_eq!(config.container.memory.as_deref(), Some("16G"));
    assert!(config.container.sudo);
}

#[test]
fn shell_defaults_to_host_detection() {
    let config = Config::parse("").expect("empty config parses");
    assert_eq!(config.shell, None);
    assert_eq!(Config::default().shell, None);
}

#[test]
fn shell_accepts_every_builtin_choice() {
    for (name, expected) in [
        ("bash", Shell::Bash),
        ("zsh", Shell::Zsh),
        ("fish", Shell::Fish),
        ("nu", Shell::Nu),
    ] {
        let config = Config::parse(&format!("shell = \"{name}\"")).expect("supported shell parses");
        assert_eq!(config.shell, Some(expected));
    }
}

#[test]
fn shell_rejects_unknown_choices() {
    let err = Config::parse("shell = \"powershell\"").expect_err("unknown shell is invalid");
    let message = err.to_string();
    assert!(message.contains("unknown variant"), "{message}");
    for supported in ["bash", "zsh", "fish", "nu"] {
        assert!(message.contains(supported), "{message}");
    }
}

#[test]
fn image_section_sets_dockerfile() {
    let config = Config::parse("[image]\ndockerfile = \"/tmp/Dockerfile\"").expect("config parses");
    assert_eq!(
        config.image.dockerfile.as_deref(),
        Some(Path::new("/tmp/Dockerfile"))
    );
}

#[test]
fn read_only_defaults_to_git() {
    let config = Config::parse("").expect("empty config parses");
    assert_eq!(config.workspace.read_only, [PathBuf::from(".git")]);
    assert_eq!(
        config.workspace.read_only,
        Config::default().workspace.read_only
    );
}

#[test]
fn read_only_can_be_disabled() {
    let config = Config::parse("workspace.read_only = []").expect("config parses");
    assert!(config.workspace.read_only.is_empty());
}

#[test]
fn read_only_accepts_custom_and_nested_paths() {
    let config = Config::parse("workspace.read_only = [\"policy\", \"config/private\"]")
        .expect("config parses");
    assert_eq!(
        config.workspace.read_only,
        [PathBuf::from("policy"), PathBuf::from("config/private")]
    );
}

#[test]
fn read_only_rejects_unsafe_and_duplicate_paths() {
    for path in ["", "/etc", "../outside", "folder/../../outside", "bad:path"] {
        let error = Config::parse(&format!("workspace.read_only = [{path:?}]"))
            .expect_err("unsafe read-only path is invalid")
            .to_string();
        assert!(error.contains("`workspace.read_only`"), "{error}");
    }

    let duplicate = Config::parse("workspace.read_only = [\"config/../policy\", \"policy\"]")
        .expect_err("normalized duplicates are invalid")
        .to_string();
    assert!(duplicate.contains("duplicates"), "{duplicate}");
}

#[test]
fn read_only_normalization_keeps_paths_inside_the_project() {
    assert_eq!(
        normalize_read_only_path(Path::new("config/../policy")).expect("path normalizes"),
        Path::new("policy")
    );
    assert_eq!(
        normalize_read_only_path(Path::new(".")).expect("project root normalizes"),
        Path::new(".")
    );
}

#[test]
fn default_config_file_matches_builtin_defaults() {
    let from_file = Config::parse(DEFAULT_CONFIG).expect("default file parses");
    let defaults = Config::default();
    assert_eq!(from_file.shell, defaults.shell);
    assert_eq!(from_file.workspace.read_only, defaults.workspace.read_only);
    assert_eq!(from_file.image.dockerfile, defaults.image.dockerfile);
    assert_eq!(from_file.container, defaults.container);
    assert!(from_file.forward.is_empty());
    assert!(from_file.mounts.is_empty());
    assert!(from_file.quick.is_empty());
}

#[test]
fn forwards_default_to_enabled_and_render_like_mounts() {
    let config = Config::parse(
        "[forward]\npostgres = { port = 5432 }\nredis = { port = 6379, enabled = false }\n",
    )
    .expect("forwards parse");

    assert!(config.forward["postgres"].is_enabled());
    assert!(!config.forward["redis"].is_enabled());
    assert_eq!(config.forward["postgres"].port, 5432);
    assert_eq!(
        config.effective_toml().expect("forwards serialize"),
        "workspace.read_only = [\".git\"]\n\n[forward]\npostgres = { port = 5432, enabled = true }\nredis = { port = 6379, enabled = false }\n"
    );
}

#[test]
fn forward_validation_rejects_unsafe_names_and_privileged_ports() {
    let error = Config::parse("[forward]\n\"Postgres\" = { port = 5432 }\nhttp = { port = 80 }\n")
        .expect_err("invalid forwards fail")
        .to_string();

    assert!(error.contains("forward `Postgres`"), "{error}");
    assert!(error.contains("forward `http`"), "{error}");
    assert!(error.contains("between 1024 and 65535"), "{error}");
}

#[test]
fn project_can_add_and_overlay_forwards() {
    let dir = TestDir::new("project-forward-overlay");
    fs::write(
        dir.path().join(".silo.toml"),
        "[forward]\npostgres = { enabled = false }\nmetrics = { port = 19090 }\nredis = { port = 6379 }\n",
    )
    .expect("project config writes");
    let global =
        Config::parse("[forward]\npostgres = { port = 5432 }\nmetrics = { port = 9090 }\n")
            .expect("global forwards parse");

    let merged = Config::apply_project_file(global, dir.path()).expect("project merges forwards");
    assert_eq!(merged.forward["postgres"].port, 5432);
    assert!(!merged.forward["postgres"].is_enabled());
    assert_eq!(merged.forward["metrics"].port, 19090);
    assert!(merged.forward["metrics"].is_enabled());
    assert_eq!(merged.forward["redis"].port, 6379);
    assert!(merged.forward["redis"].is_enabled());
}

#[test]
fn project_forward_additions_require_a_port() {
    let dir = TestDir::new("project-forward-missing-port");

    fs::write(
        dir.path().join(".silo.toml"),
        "[forward]\nredis = { enabled = true }\n",
    )
    .expect("project config rewrites");
    let global =
        Config::parse("[forward]\npostgres = { port = 5432 }\n").expect("global forward parses");
    let error = format!(
        "{:#}",
        Config::apply_project_file(global, dir.path()).expect_err("incomplete forward fails")
    );
    assert!(error.contains("must define `port`"), "{error}");
}

#[test]
fn project_forward_ports_are_recognized() {
    let (_, unknown_keys) = deserialize_toml::<ProjectConfig>(
        "[forward]\npostgres = { enabled = true, port = 5432 }\n",
    )
    .expect("project forward parses");

    assert!(unknown_keys.is_empty());
}

#[test]
fn effective_toml_omits_every_empty_section() {
    let text = Config::default()
        .effective_toml()
        .expect("default config serializes");

    assert_eq!(text, "workspace.read_only = [\".git\"]\n");
}

#[test]
fn effective_toml_matches_starter_style_and_round_trips() {
    let config = Config::parse(
        "image.dockerfile = \"/tmp/Dockerfile\"\n\
         container.cpus = 4\n\
         container.memory = \"4G\"\n\
         container.sudo = true\n\
         shell = \"fish\"\n\
         workspace.read_only = [\"policy\"]\n\
         [binds.docs]\n\
         source = \"/tmp/docs\"\n\
         target = \"~/docs\"\n\
         access = \"read-only\"\n\
         [state.project.cargo]\n\
         target = \"~/.cargo\"\n\
         [state.user.codex]\n\
         target = \"~/.codex\"\n\
         [quick]\n\
         test = [\"cargo\", \"test\"]\n",
    )
    .expect("config parses");

    let text = config
        .effective_toml()
        .expect("effective config serializes");
    assert_eq!(
        text,
        "image.dockerfile = \"/tmp/Dockerfile\"\n\
         container.cpus = 4\n\
         container.memory = \"4G\"\n\
         container.sudo = true\n\
         shell = \"fish\"\n\
         workspace.read_only = [\"policy\"]\n\
         \n\
         [binds]\n\
         docs = { enabled = true, source = \"/tmp/docs\", target = \"~/docs\", access = \"read-only\" }\n\
         \n\
         [state.project]\n\
         cargo = { enabled = true, target = \"~/.cargo\" }\n\
         \n\
         [state.user]\n\
         codex = { enabled = true, target = \"~/.codex\" }\n\
         \n\
         [quick]\n\
         test = [\"cargo\", \"test\"]\n"
    );

    let reparsed = Config::parse(&text).expect("printed config parses again");
    assert_eq!(reparsed.workspace.read_only, [PathBuf::from("policy")]);
    assert_eq!(
        reparsed.image.dockerfile,
        Some(PathBuf::from("/tmp/Dockerfile"))
    );
    assert_eq!(reparsed.container.cpus, Some(4));
    assert_eq!(reparsed.container.memory.as_deref(), Some("4G"));
    assert!(reparsed.container.sudo);
    assert_eq!(reparsed.shell, Some(Shell::Fish));
    assert_eq!(reparsed.mounts.len(), 3);
    assert_eq!(reparsed.mounts["docs"].kind(), Some(MountKind::Host));
    assert_eq!(
        reparsed.mounts["cargo"].kind(),
        Some(MountKind::ProjectState)
    );
    assert_eq!(reparsed.mounts["codex"].kind(), Some(MountKind::UserState));
    assert_eq!(reparsed.quick["test"], ["cargo", "test"]);
}

#[test]
fn effective_toml_omits_unpopulated_mount_categories() {
    let config = Config::parse(
        "[state.user.zeta]\n\
         target = \"~/zeta\"\n\
         [state.user.codex]\n\
         target = \"~/.codex\"\n",
    )
    .expect("user state parses");

    let text = config.effective_toml().expect("config serializes");

    assert_eq!(
        text,
        "workspace.read_only = [\".git\"]\n\
         \n\
         [state.user]\n\
         codex = { enabled = true, target = \"~/.codex\" }\n\
         zeta = { enabled = true, target = \"~/zeta\" }\n"
    );
}

#[test]
fn effective_toml_preserves_escaped_quick_names() {
    let config = Config::parse(
        "[quick]\n\
         zeta = [\"zeta\"]\n\
         \"review.code\" = [\"codex\", \"review\"]\n",
    )
    .expect("quick commands parse");

    let text = config.effective_toml().expect("config serializes");

    assert_eq!(
        text,
        "workspace.read_only = [\".git\"]\n\
         \n\
         [quick]\n\
         \"review.code\" = [\"codex\", \"review\"]\n\
         zeta = [\"zeta\"]\n"
    );
    Config::parse(&text).expect("printed quick commands parse again");
}

#[test]
fn effective_toml_prints_the_merged_and_resolved_project_config() {
    let dir = TestDir::new("effective-merged");
    let global = dir.path().join("global/config.toml");
    fs::create_dir_all(global.parent().expect("global has parent"))
        .expect("global directory creation succeeds");
    fs::write(
        &global,
        "container.cpus = 8\n\
         [binds.docs]\n\
         source = \"docs\"\n\
         target = \"~/docs\"\n\
         access = \"read-only\"\n\
         [quick]\n\
         test = [\"cargo\", \"test\"]\n",
    )
    .expect("global config write succeeds");
    fs::write(
        dir.path().join(".silo.toml"),
        "container.memory = \"16G\"\n\
         [binds.docs]\n\
         access = \"read-write\"\n",
    )
    .expect("project config write succeeds");

    let global_config = Config::load_from(&global).expect("global config loads");
    let config = Config::apply_project_file(global_config, dir.path())
        .expect("project config overlays global config");
    let text = config
        .effective_toml()
        .expect("effective config serializes");
    let reparsed = Config::parse(&text).expect("effective config is valid TOML");

    assert_eq!(reparsed.container.cpus, Some(8));
    assert_eq!(reparsed.container.memory.as_deref(), Some("16G"));
    assert_eq!(
        reparsed.mounts["docs"].source.as_deref(),
        Some(dir.path().join("global/docs").as_path())
    );
    assert_eq!(
        reparsed.mounts["docs"].effective_access(),
        Permission::ReadWrite
    );
    assert_eq!(reparsed.quick["test"], ["cargo", "test"]);
}

#[test]
fn dotted_container_resources_keep_later_settings_at_the_root() {
    let config = Config::parse(
        "container.cpus = 8\n\
         container.memory = \"16G\"\n\
         shell = \"fish\"\n\
         workspace.read_only = []\n\
         [binds.notes]\n\
         source = \"~/notes\"\n\
         target = \"~/notes\"\n\
         access = \"read-only\"\n",
    )
    .expect("dotted resources and later root settings parse");

    assert_eq!(config.container.cpus, Some(8));
    assert_eq!(config.container.memory.as_deref(), Some("16G"));
    assert_eq!(config.shell, Some(Shell::Fish));
    assert!(config.workspace.read_only.is_empty());
    assert_eq!(config.mounts.len(), 1);
}

#[test]
fn unknown_keys_are_reported_and_ignored() {
    let (config, unknown_keys) = Config::parse_with_unknown_keys(
        "[image]\nfuture_key = 42\n[future]\nsome_setting = \"bar\"",
    )
    .expect("config with unknown keys parses");
    assert!(config.image.dockerfile.is_none());
    assert_eq!(unknown_keys, ["future", "image.future_key"]);
}

#[test]
fn unknown_key_warning_names_the_key_and_source_file() {
    let path = Path::new("/project/.silo.toml");
    assert_eq!(
        unknown_key_warning(path, "image.dockerfiel"),
        "warning: unknown config key `image.dockerfiel` in `/project/.silo.toml`; it will be ignored"
    );
}

#[test]
fn invalid_toml_reports_line_and_column() {
    let err = Config::parse("dockerfile =").expect_err("unterminated value is invalid");
    let msg = err.to_string();
    assert!(
        msg.contains("line 1"),
        "error points at the location: {msg}"
    );
    assert!(
        msg.contains("column"),
        "error points at the location: {msg}"
    );
}

#[test]
fn load_from_creates_default_file_on_first_use() {
    let dir = TestDir::new("first-use");
    let path = dir.path().join("config.toml");
    let config = Config::load_from(&path).expect("missing config loads defaults");
    assert!(config.image.dockerfile.is_none());
    assert!(path.exists(), "default config file is created");
    let written = fs::read_to_string(&path).expect("default config is readable");
    assert_eq!(written, DEFAULT_CONFIG, "template and file stay in sync");
}

#[test]
fn load_from_reads_existing_config() {
    let dir = TestDir::new("existing");
    let path = dir.path().join("config.toml");
    fs::write(&path, "[image]\ndockerfile = \"/tmp/Dockerfile\"").expect("write succeeds");
    let config = Config::load_from(&path).expect("config loads");
    assert_eq!(
        config.image.dockerfile.as_deref(),
        Some(Path::new("/tmp/Dockerfile"))
    );
}

#[test]
fn load_from_resolves_relative_host_sources_from_config_directory() {
    let dir = TestDir::new("relative-global-mount");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        "[binds.docs]\nsource = \"content\"\ntarget = \"/docs\"\naccess = \"read-only\"\n",
    )
    .expect("write succeeds");

    let config = Config::load_from(&path).expect("config loads");
    assert_eq!(
        config.mounts["docs"].source.as_deref(),
        Some(dir.path().join("content").as_path())
    );
}

#[test]
fn load_from_rejects_invalid_config_with_path_and_location() {
    let dir = TestDir::new("invalid");
    let path = dir.path().join("config.toml");
    fs::write(&path, "dockerfile =").expect("write succeeds");
    let err = Config::load_from(&path).expect_err("invalid config errors");
    // `{:#}` prints the full chain, including the TOML location from the source.
    let msg = format!("{err:#}");
    assert!(msg.contains(path.to_str().expect("path is UTF-8")));
    assert!(
        msg.contains("line 1"),
        "error points at the location: {msg}"
    );
}

#[test]
fn config_path_prefers_xdg_config_home() {
    let path = config_path_from(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/user")))
        .expect("path resolves");
    assert_eq!(path, Path::new("/xdg/silo/config.toml"));
}

#[test]
fn config_path_falls_back_to_home_config() {
    let path = config_path_from(None, Some(OsStr::new("/home/user"))).expect("path resolves");
    assert_eq!(path, Path::new("/home/user/.config/silo/config.toml"));
}

#[test]
fn config_path_ignores_empty_xdg_config_home() {
    let path = config_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/user")))
        .expect("path resolves");
    assert_eq!(path, Path::new("/home/user/.config/silo/config.toml"));
}

#[test]
fn config_path_ignores_relative_xdg_config_home() {
    let path = config_path_from(
        Some(OsStr::new("relative/xdg")),
        Some(OsStr::new("/home/user")),
    )
    .expect("path resolves");
    assert_eq!(path, Path::new("/home/user/.config/silo/config.toml"));
}

#[test]
fn config_path_returns_none_without_home() {
    assert!(config_path_from(None, None).is_none());
}

#[test]
fn active_config_paths_list_existing_sources_in_precedence_order() {
    let dir = TestDir::new("active-paths");
    let xdg = dir.path().join("xdg");
    let global = xdg.join("silo/config.toml");
    fs::create_dir_all(global.parent().expect("global has parent"))
        .expect("global directory creation succeeds");
    fs::write(&global, "").expect("global config creation succeeds");
    let project = dir.path().join("project");
    fs::create_dir_all(&project).expect("project directory creation succeeds");
    let project_config = project.join(".silo.toml");
    fs::write(&project_config, "").expect("project config creation succeeds");

    let paths = active_config_paths_from(&project, Some(xdg.as_os_str()), None);
    assert_eq!(
        paths,
        [
            ("global", global.clone()),
            ("project", project_config.clone())
        ]
    );
    assert_eq!(
        config_paths_text(&paths).expect("active paths format"),
        format!(
            "global\t{}\nproject\t{}\n",
            global.display(),
            project_config.display()
        )
    );
}

#[test]
fn active_config_paths_omit_missing_files_and_report_builtin_only_state() {
    let dir = TestDir::new("missing-active-paths");
    let xdg = dir.path().join("xdg");
    let paths = active_config_paths_from(dir.path(), Some(xdg.as_os_str()), None);
    assert!(paths.is_empty());
    let message = config_paths_text(&paths)
        .expect_err("no active files is reported")
        .to_string();
    assert!(message.contains("built-in defaults"), "{message}");
}

#[test]
fn edit_path_prefers_project_and_global_flag_overrides_it() {
    let dir = TestDir::new("edit-path");
    let xdg = dir.path().join("xdg");
    let project_config = dir.path().join(".silo.toml");
    fs::write(&project_config, "").expect("project config creation succeeds");
    let global = xdg.join("silo/config.toml");

    assert_eq!(
        edit_path_from(dir.path(), false, Some(xdg.as_os_str()), None)
            .expect("project edit path resolves"),
        project_config
    );
    assert_eq!(
        edit_path_from(dir.path(), true, Some(xdg.as_os_str()), None)
            .expect("global edit path resolves"),
        global
    );
}

#[test]
fn edit_path_falls_back_to_global_and_requires_a_home_location() {
    let dir = TestDir::new("edit-global-fallback");
    let home = dir.path().join("home");
    assert_eq!(
        edit_path_from(dir.path(), false, None, Some(home.as_os_str()))
            .expect("global fallback resolves"),
        home.join(".config/silo/config.toml")
    );
    let message = edit_path_from(dir.path(), false, None, None)
        .expect_err("missing home is reported")
        .to_string();
    assert!(message.contains("XDG_CONFIG_HOME or HOME"), "{message}");
}

#[test]
fn editor_selection_uses_visual_then_editor_then_vi() {
    assert_eq!(
        selected_editor(Some(OsStr::new("nvim")), Some(OsStr::new("nano"))),
        OsStr::new("nvim")
    );
    assert_eq!(
        selected_editor(Some(OsStr::new("")), Some(OsStr::new("nano"))),
        OsStr::new("nano")
    );
    assert_eq!(selected_editor(None, None), OsStr::new("vi"));
}

#[test]
fn editor_supports_arguments_and_keeps_the_config_path_one_argument() {
    let dir = TestDir::new("editor-arguments");
    let script = dir.path().join("fake editor.sh");
    let output = dir.path().join("editor-arguments.txt");
    let config = dir.path().join("config file.toml");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            output.display()
        ),
    )
    .expect("editor script creation succeeds");
    let editor = format!("/bin/sh '{}' --wait", script.display());

    run_editor(&config, None, Some(OsStr::new(&editor))).expect("fake editor succeeds");

    let arguments = fs::read_to_string(output).expect("editor arguments are recorded");
    assert_eq!(arguments, format!("--wait\n{}\n", config.display()));
}

#[test]
fn editor_failure_is_reported() {
    let message = run_editor(
        Path::new("/tmp/config.toml"),
        Some(OsStr::new("false")),
        None,
    )
    .expect_err("failed editor is reported")
    .to_string();
    assert!(message.contains("exited with status"), "{message}");
}

#[test]
fn config_without_read_only_uses_the_default() {
    let config =
        Config::parse("[image]\ndockerfile = \"/tmp/Dockerfile\"\n").expect("config parses");
    assert_eq!(
        config.workspace.read_only,
        [PathBuf::from(".git")],
        "missing key falls back to the default"
    );
    assert_eq!(
        config.image.dockerfile.as_deref(),
        Some(Path::new("/tmp/Dockerfile"))
    );
}

#[test]
fn quick_defaults_to_empty() {
    // A config written before `quick` existed has no quick commands.
    let config = Config::parse("[image]\n").expect("config parses");
    assert!(config.quick.is_empty());
}

#[test]
fn quick_parses_names_to_commands() {
    let config =
        Config::parse("[quick]\ncodex = [\"codex\"]\ncx = [\"codex\", \"--model\", \"compact\"]\n")
            .expect("config parses");
    assert_eq!(config.quick["codex"], ["codex"]);
    assert_eq!(config.quick["cx"], ["codex", "--model", "compact"]);
}

#[test]
fn quick_command_names_are_not_unknown_config_keys() {
    let (config, unknown_keys) = deserialize_toml::<Config>(
        "[quick]\ncodex = [\"codex\"]\ncx = [\"codex\", \"--model\", \"compact\"]\n",
    )
    .expect("config parses");
    assert!(unknown_keys.is_empty());
    assert_eq!(config.quick.len(), 2);
}

#[test]
fn quick_rejects_names_shadowed_by_builtins() {
    let config = Config::parse("[quick]\nrun = [\"bash\"]\n").expect("config parses");
    let msg = config
        .check_quick_names(&["run", "image", "help"])
        .expect_err("`run` collides with the built-in command")
        .to_string();
    assert!(msg.contains("`run`"), "names the offender: {msg}");
    assert!(msg.contains("shadowed"), "explains why: {msg}");
}

#[test]
fn quick_rejects_containers_after_it_becomes_a_builtin() {
    let config = Config::parse("[quick]\ncontainers = [\"bash\"]\n").expect("config parses");
    let msg = config
        .check_quick_names(&["run", "containers", "image", "help"])
        .expect_err("`containers` collides with the built-in command")
        .to_string();
    assert!(msg.contains("`containers`"), "names the offender: {msg}");
}

#[test]
fn quick_lists_every_shadowed_name() {
    let config =
        Config::parse("[quick]\nrun = [\"bash\"]\nimage = [\"bash\"]\ncodex = [\"codex\"]\n")
            .expect("config parses");
    let msg = config
        .check_quick_names(&["run", "image", "help"])
        .expect_err("two names collide")
        .to_string();
    assert!(msg.contains("`run`"), "{msg}");
    assert!(msg.contains("`image`"), "{msg}");
    assert!(!msg.contains("`codex`"), "non-colliding name stays: {msg}");
}

#[test]
fn quick_accepts_names_that_do_not_collide() {
    let config = Config::parse("[quick]\ncodex = [\"codex\"]\n").expect("config parses");
    config
        .check_quick_names(&["run", "image", "help"])
        .expect("no collision");
}

#[test]
fn quick_rejects_flag_shaped_names() {
    let config = Config::parse("[quick]\n\"-h\" = [\"codex\"]\n").expect("config parses");
    let msg = config
        .check_quick_names(&["run", "image", "help"])
        .expect_err("`-h` collides with the CLI's own options")
        .to_string();
    assert!(msg.contains("`-h`"), "{msg}");
    assert!(msg.contains("starts with `-`"), "{msg}");
}

#[test]
fn mounts_default_to_empty() {
    let config = Config::parse("[image]\n").expect("config parses");
    assert!(config.mounts.is_empty());
}

#[test]
fn binds_and_scoped_state_parse_with_clear_access() {
    let config = Config::parse(
        "[binds]\n\
         notes = { source = \"~/notes\", target = \"~/notes\", access = \"read-only\" }\n\
         output = { source = \"~/output\", target = \"~/output\", access = \"read-write\" }\n\
         [state.project]\n\
         cargo = { target = \"~/.cargo\" }\n\
         cargo-target = { target = \"./target\" }\n\
         [state.user]\n\
         codex = { target = \"~/.codex\" }\n",
    )
    .expect("named mounts parse");

    assert_eq!(config.mounts.len(), 5);
    assert_eq!(config.mounts["notes"].kind(), Some(MountKind::Host));
    assert_eq!(
        config.mounts["notes"].effective_target(Path::new("/home/silo/workspace")),
        Some(PathBuf::from("/home/silo/notes"))
    );
    assert_eq!(
        config.mounts["notes"].effective_access(),
        Permission::ReadOnly
    );
    assert_eq!(
        config.mounts["cargo"].effective_access(),
        Permission::ReadWrite
    );
    assert_eq!(
        config.mounts["cargo"].effective_target(Path::new("/home/silo/workspace")),
        Some(PathBuf::from("/home/silo/.cargo"))
    );
    assert_eq!(
        config.mounts["cargo-target"].effective_target(Path::new("/home/silo/workspace")),
        Some(PathBuf::from("/home/silo/workspace/target"))
    );
    assert_eq!(
        config.mounts["codex"].effective_access(),
        Permission::ReadWrite
    );
    assert_eq!(
        config.mounts["codex"].effective_target(Path::new("/home/silo/workspace")),
        Some(PathBuf::from("/home/silo/.codex"))
    );
    assert_eq!(
        config.mounts["output"].effective_access(),
        Permission::ReadWrite
    );
}

#[test]
fn omitted_project_mounts_inherit_global_entries() {
    let dir = TestDir::new("project-inherits-mounts");
    fs::write(dir.path().join(".silo.toml"), "workspace.read_only = []\n")
        .expect("project config writes");
    let global = Config::parse(
        "[binds.notes]\nsource = \"~/notes\"\ntarget = \"~/notes\"\naccess = \"read-only\"\n",
    )
    .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.mounts.len(), 1);
    assert_eq!(
        merged.mounts["notes"].effective_target(Path::new("/home/silo/workspace")),
        Some(PathBuf::from("/home/silo/notes"))
    );
    assert!(merged.workspace.read_only.is_empty());
}

#[test]
fn project_shell_overrides_global_shell() {
    let dir = TestDir::new("project-shell");
    fs::write(dir.path().join(".silo.toml"), "shell = \"fish\"\n").expect("project config writes");
    let global = Config::parse("shell = \"bash\"\n").expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.shell, Some(Shell::Fish));
}

#[test]
fn omitted_project_shell_inherits_global_shell() {
    let dir = TestDir::new("project-inherits-shell");
    fs::write(dir.path().join(".silo.toml"), "workspace.read_only = []\n")
        .expect("project config writes");
    let global = Config::parse("shell = \"nu\"\n").expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.shell, Some(Shell::Nu));
}

#[test]
fn project_container_settings_override_independently() {
    let dir = TestDir::new("project-container-resources");
    fs::write(
        dir.path().join(".silo.toml"),
        "[container]\nmemory = \"16G\"\nsudo = false\n",
    )
    .expect("project config writes");
    let global = Config::parse("[container]\ncpus = 6\nmemory = \"8G\"\nsudo = true\n")
        .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.container.cpus, Some(6));
    assert_eq!(merged.container.memory.as_deref(), Some("16G"));
    assert!(!merged.container.sudo);
}

#[test]
fn omitted_project_sudo_inherits_global_setting() {
    let dir = TestDir::new("project-inherits-sudo");
    fs::write(
        dir.path().join(".silo.toml"),
        "[container]\nmemory = \"16G\"\n",
    )
    .expect("project config writes");
    let global = Config::parse("container.sudo = true\n").expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert!(merged.container.sudo);
}

#[test]
fn project_schema_reports_unknown_nested_keys() {
    let (_, unknown_keys) = deserialize_toml::<ProjectConfig>(
        "workspace.read_only = [\".git\"]\n[image]\ndockerfiel = \"Dockerfile\"\n",
    )
    .expect("project config parses");
    assert_eq!(unknown_keys, ["image.dockerfiel"]);
}

#[test]
fn project_mount_overlay_changes_one_field_and_can_disable_another() {
    let dir = TestDir::new("project-overlays-mounts");
    fs::write(
        dir.path().join(".silo.toml"),
        "[binds.notes]\ntarget = \"/notes\"\n[state.user.codex]\nenabled = false\n",
    )
    .expect("project config writes");
    let global = Config::parse(
        "[binds.notes]\nsource = \"~/notes\"\ntarget = \"/old\"\naccess = \"read-only\"\n\
         [state.user.codex]\ntarget = \"~/.codex\"\n",
    )
    .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.mounts["notes"].kind(), Some(MountKind::Host));
    assert_eq!(
        merged.mounts["notes"].source.as_deref(),
        Some(Path::new("~/notes"))
    );
    assert_eq!(
        merged.mounts["notes"].target.as_deref(),
        Some(Path::new("/notes"))
    );
    assert!(!merged.mounts["codex"].is_enabled());
}

#[test]
fn project_mount_source_is_resolved_from_project_root() {
    let dir = TestDir::new("project-relative-mount");
    fs::write(
        dir.path().join(".silo.toml"),
        "[binds.cache]\nsource = \"project-cache\"\ntarget = \"/cache\"\naccess = \"read-only\"\n",
    )
    .expect("project config writes");

    let merged = Config::apply_project_file(Config::default(), dir.path()).expect("configs merge");

    assert_eq!(
        merged.mounts["cache"].source.as_deref(),
        Some(dir.path().join("project-cache").as_path())
    );
}

#[test]
fn changing_mount_intent_resets_host_specific_fields() {
    let dir = TestDir::new("project-changes-mount-kind");
    fs::write(
        dir.path().join(".silo.toml"),
        "[state.user.tools]\ntarget = \"~/tools\"\n",
    )
    .expect("project config writes");
    let global = Config::parse(
        "[binds.tools]\nsource = \"~/tools\"\ntarget = \"/tools\"\naccess = \"read-write\"\n",
    )
    .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");
    let mount = &merged.mounts["tools"];
    assert_eq!(mount.kind(), Some(MountKind::UserState));
    assert_eq!(mount.source, None);
    assert_eq!(mount.target.as_deref(), Some(Path::new("~/tools")));
    assert_eq!(mount.access, None);
    assert_eq!(mount.effective_access(), Permission::ReadWrite);
}

#[test]
fn project_quick_table_replaces_global_table() {
    let dir = TestDir::new("project-replaces-quick");
    fs::write(
        dir.path().join(".silo.toml"),
        "[quick]\nproject = [\"project-command\"]\n",
    )
    .expect("project config writes");
    let global =
        Config::parse("[quick]\nglobal = [\"global-command\"]\n").expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert!(!merged.quick.contains_key("global"));
    assert_eq!(merged.quick["project"], ["project-command"]);
}

#[test]
fn empty_project_quick_table_clears_global_table() {
    let dir = TestDir::new("project-clears-quick");
    fs::write(dir.path().join(".silo.toml"), "[quick]\n").expect("project config writes");
    let global =
        Config::parse("[quick]\nglobal = [\"global-command\"]\n").expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert!(merged.quick.is_empty());
}

#[test]
fn project_dockerfile_overrides_and_resolves_from_project_root() {
    let dir = TestDir::new("project-dockerfile");
    fs::write(
        dir.path().join(".silo.toml"),
        "[image]\ndockerfile = \"containers/Dockerfile\"\n",
    )
    .expect("project config writes");
    let global = Config::parse("[image]\ndockerfile = \"/global/Dockerfile\"\n")
        .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");
    let expected = dir.path().join("containers/Dockerfile");

    assert_eq!(merged.image.dockerfile.as_deref(), Some(expected.as_path()));
}

#[test]
fn empty_project_file_keeps_global_configuration() {
    let dir = TestDir::new("empty-project-config");
    fs::write(dir.path().join(".silo.toml"), "").expect("project config writes");
    let global = Config::parse(
        "workspace.read_only = [\"policy\"]\n[quick]\nglobal = [\"global-command\"]\n",
    )
    .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.workspace.read_only, [PathBuf::from("policy")]);
    assert_eq!(merged.quick["global"], ["global-command"]);
}

#[test]
fn project_read_only_list_replaces_the_global_list() {
    let dir = TestDir::new("project-read-only");
    fs::write(
        dir.path().join(".silo.toml"),
        "workspace.read_only = [\"policy\"]\n",
    )
    .expect("project config writes");
    let global = Config::parse("workspace.read_only = [\".git\", \".github\"]\n")
        .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.workspace.read_only, [PathBuf::from("policy")]);
}

#[test]
fn invalid_project_config_reports_its_path_and_location() {
    let dir = TestDir::new("invalid-project-config");
    let path = dir.path().join(".silo.toml");
    fs::write(&path, "mounts = {").expect("project config writes");

    let err = Config::apply_project_file(Config::default(), dir.path())
        .expect_err("invalid project config errors");
    let msg = format!("{err:#}");

    assert!(msg.contains(&path.display().to_string()), "{msg}");
    assert!(msg.contains("line 1"), "{msg}");
}

#[test]
fn disabled_mount_tombstone_needs_no_other_fields() {
    let config = Config::parse("[state.user.optional]\nenabled = false\n").expect("config parses");
    assert!(!config.mounts["optional"].is_enabled());
}

#[test]
fn enabled_mount_requires_category_fields_and_unique_name() {
    let config = Config::parse(
        "[binds.missing]\ntarget = \"/somewhere\"\n\
         [state.project.state]\n",
    )
    .expect_err("incomplete enabled mounts are invalid");
    let msg = config.to_string();
    assert!(
        msg.contains("bind or state entry `missing`: bind is missing `source`"),
        "{msg}"
    );
    assert!(
        msg.contains("bind or state entry `state`: enabled entry is missing `target`"),
        "{msg}"
    );

    let duplicate = Config::parse(
        "[binds.same]\nsource = \"~/same\"\ntarget = \"~/same\"\naccess = \"read-only\"\n\
         [state.user.same]\ntarget = \"~/same\"\n",
    )
    .expect_err("names must be unique across mount categories");
    assert!(
        duplicate
            .to_string()
            .contains("entry `same` is defined in more than one bind or state category")
    );
}

#[test]
fn direct_parse_rejects_relative_host_sources() {
    let config = Config::parse(
        "[binds.notes]\nsource = \"notes\"\ntarget = \"~/notes\"\naccess = \"read-only\"\n",
    )
    .expect_err("relative source is invalid");
    let msg = config.to_string();
    assert!(msg.contains("does not start with a bare `~`"), "{msg}");
}

#[test]
fn mount_rejects_named_user_tilde_paths() {
    let config = Config::parse(
        "[binds.notes]\nsource = \"~user/notes\"\ntarget = \"~/notes\"\naccess = \"read-only\"\n",
    )
    .expect_err("~user source is invalid");
    let msg = config.to_string();
    assert!(msg.contains("does not start with a bare `~`"), "{msg}");
}

#[test]
fn mounts_report_and_ignore_unknown_keys() {
    let (config, unknown_keys) = Config::parse_with_unknown_keys(
        "[binds.notes]\nsource = \"~/notes\"\ntarget = \"~/notes\"\naccess = \"read-only\"\nfuture_key = 42\n",
    )
    .expect("unknown key is ignored");
    assert_eq!(
        config.mounts["notes"].effective_access(),
        Permission::ReadOnly
    );
    assert_eq!(unknown_keys, ["binds.notes.future_key"]);
}

#[test]
fn mount_rejects_invalid_name_and_target() {
    let config = Config::parse(
        "[binds.\"bad name\"]\nsource = \"~/notes\"\ntarget = \"./../notes\"\naccess = \"read-only\"\n",
    )
    .expect_err("relative target is invalid");
    let msg = config.to_string();
    assert!(msg.contains("bind or state entry `bad name`"), "{msg}");
    assert!(msg.contains("must not escape its target root"), "{msg}");
}

#[test]
fn mount_rejects_unanchored_relative_target() {
    let err = Config::parse("[state.project.cache]\ntarget = \"cache\"\n")
        .expect_err("an unanchored relative target is ambiguous");
    assert!(
        err.to_string().contains(
            "path must start with `./` for the project, `~/` for the container home, or `/`"
        ),
        "{err}"
    );
}

#[test]
fn mount_rejects_empty_and_mount_syntax_paths() {
    let config = Config::parse(
        "[binds.empty]\nsource = \"\"\ntarget = \"/empty\"\naccess = \"read-only\"\n\
         [binds.comma]\nsource = \"~/notes\"\ntarget = \"/bad,target\"\naccess = \"read-only\"\n\
         [binds.equals]\nsource = \"~/bad=source\"\ntarget = \"/equals\"\naccess = \"read-only\"\n",
    )
    .expect_err("invalid paths are rejected");
    let msg = config.to_string();
    assert!(msg.contains("source path is empty"), "{msg}");
    assert!(msg.contains("target path contains `,` or `=`"), "{msg}");
    assert!(msg.contains("source path contains `,` or `=`"), "{msg}");
}
