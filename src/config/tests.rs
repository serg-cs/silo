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
fn read_only_git_is_enabled_by_default() {
    let config = Config::parse("").expect("empty config parses");
    assert!(config.read_only_git);
    assert!(Config::default().read_only_git);
}

#[test]
fn read_only_git_can_be_disabled() {
    let config = Config::parse("read_only_git = false").expect("config parses");
    assert!(!config.read_only_git);
}

#[test]
fn read_only_git_can_be_enabled_explicitly() {
    let config = Config::parse("read_only_git = true").expect("config parses");
    assert!(config.read_only_git);
}

#[test]
fn default_config_file_matches_builtin_defaults() {
    let from_file = Config::parse(DEFAULT_CONFIG).expect("default file parses");
    let defaults = Config::default();
    assert_eq!(from_file.shell, defaults.shell);
    assert_eq!(from_file.read_only_git, defaults.read_only_git);
    assert_eq!(from_file.image.dockerfile, defaults.image.dockerfile);
    assert!(from_file.shared.is_empty());
    assert!(from_file.quick.is_empty());
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
fn old_config_without_the_key_still_protects() {
    // A config written before `read_only_git` existed: only `[image]`.
    let config =
        Config::parse("[image]\ndockerfile = \"/tmp/Dockerfile\"\n").expect("config parses");
    assert!(
        config.read_only_git,
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
fn shared_default_to_empty() {
    // A config written before `shared` existed has no shared mounts.
    let config = Config::parse("[image]\n").expect("config parses");
    assert!(config.shared.is_empty());
}

#[test]
fn shared_parse_source_target_and_permission() {
    let config = Config::parse(
        "[[shared]]\n\
         source = \"~/notes\"\n\
         target = \"/home/silo/notes\"\n\
         permission = \"read-write\"\n\
         [[shared]]\n\
         source = \"~/.ssh\"\n\
         target = \"/home/silo/.ssh\"\n\
         permission = \"read-only\"\n",
    )
    .expect("config parses");
    assert_eq!(config.shared.len(), 2);
    assert_eq!(config.shared[0].source, PathBuf::from("~/notes"));
    assert_eq!(config.shared[0].target, PathBuf::from("/home/silo/notes"));
    assert_eq!(config.shared[0].permission, Permission::ReadWrite);
    assert_eq!(config.shared[1].source, PathBuf::from("~/.ssh"));
    assert_eq!(config.shared[1].target, PathBuf::from("/home/silo/.ssh"));
    assert_eq!(config.shared[1].permission, Permission::ReadOnly);
}

#[test]
fn shared_parse_compact_array() {
    let config = Config::parse(
        "shared = [\n\
         { source = \"~/notes\", target = \"/home/silo/notes\", permission = \"read-write\" },\n\
         { source = \"~/.ssh\", target = \"/home/silo/.ssh\" },\n\
         ]\n",
    )
    .expect("compact shared array parses");
    assert_eq!(config.shared.len(), 2);
    assert_eq!(config.shared[0].source, PathBuf::from("~/notes"));
    assert_eq!(config.shared[0].permission, Permission::ReadWrite);
    assert_eq!(config.shared[1].target, PathBuf::from("/home/silo/.ssh"));
    assert_eq!(config.shared[1].permission, Permission::ReadOnly);
}

#[test]
fn project_omitted_shared_inherits_global_list() {
    let dir = TestDir::new("project-inherits-shared");
    fs::write(dir.path().join(".silo.toml"), "read_only_git = false\n")
        .expect("project config writes");
    let global =
        Config::parse("shared = [{ source = \"~/notes\", target = \"/home/silo/notes\" }]\n")
            .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.shared.len(), 1);
    assert_eq!(merged.shared[0].target, PathBuf::from("/home/silo/notes"));
    assert!(!merged.read_only_git);
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
    fs::write(dir.path().join(".silo.toml"), "read_only_git = false\n")
        .expect("project config writes");
    let global = Config::parse("shell = \"nu\"\n").expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.shell, Some(Shell::Nu));
}

#[test]
fn project_schema_reports_unknown_nested_keys() {
    let (_, unknown_keys) = deserialize_toml::<ProjectConfig>(
        "read_only_git = true\n[image]\ndockerfiel = \"Dockerfile\"\n",
    )
    .expect("project config parses");
    assert_eq!(unknown_keys, ["image.dockerfiel"]);
}

#[test]
fn project_empty_shared_clears_global_list() {
    let dir = TestDir::new("project-clears-shared");
    fs::write(dir.path().join(".silo.toml"), "shared = []\n").expect("project config writes");
    let global =
        Config::parse("shared = [{ source = \"~/notes\", target = \"/home/silo/notes\" }]\n")
            .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert!(merged.shared.is_empty());
}

#[test]
fn project_shared_replaces_global_and_resolves_relative_sources() {
    let dir = TestDir::new("project-replaces-shared");
    fs::write(
        dir.path().join(".silo.toml"),
        "shared = [{ source = \"project-cache\", target = \"/cache\" }]\n",
    )
    .expect("project config writes");
    let global =
        Config::parse("shared = [{ source = \"~/notes\", target = \"/home/silo/notes\" }]\n")
            .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert_eq!(merged.shared.len(), 1);
    assert_eq!(merged.shared[0].source, dir.path().join("project-cache"));
    assert_eq!(merged.shared[0].target, PathBuf::from("/cache"));
}

#[test]
fn project_shared_still_rejects_named_user_tilde_sources() {
    let dir = TestDir::new("project-rejects-user-tilde");
    fs::write(
        dir.path().join(".silo.toml"),
        "shared = [{ source = \"~user/notes\", target = \"/notes\" }]\n",
    )
    .expect("project config writes");

    let err = Config::apply_project_file(Config::default(), dir.path())
        .expect_err("named-user tilde remains invalid");
    let msg = format!("{err:#}");

    assert!(msg.contains("`~user` paths are not supported"), "{msg}");
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
    let global = Config::parse("read_only_git = false\n[quick]\nglobal = [\"global-command\"]\n")
        .expect("global config parses");

    let merged = Config::apply_project_file(global, dir.path()).expect("configs merge");

    assert!(!merged.read_only_git);
    assert_eq!(merged.quick["global"], ["global-command"]);
}

#[test]
fn invalid_project_config_reports_its_path_and_location() {
    let dir = TestDir::new("invalid-project-config");
    let path = dir.path().join(".silo.toml");
    fs::write(&path, "shared = [").expect("project config writes");

    let err = Config::apply_project_file(Config::default(), dir.path())
        .expect_err("invalid project config errors");
    let msg = format!("{err:#}");

    assert!(msg.contains(&path.display().to_string()), "{msg}");
    assert!(msg.contains("line 1"), "{msg}");
}

#[test]
fn shared_permission_defaults_to_read_only() {
    let config = Config::parse("[[shared]]\nsource = \"~/notes\"\ntarget = \"/home/silo/notes\"\n")
        .expect("config parses");
    assert_eq!(config.shared[0].permission, Permission::ReadOnly);
}

#[test]
fn shared_rejects_unknown_permissions() {
    let config = Config::parse(
        "[[shared]]\n\
         source = \"~/notes\"\n\
         target = \"/home/silo/notes\"\n\
         permission = \"write\"\n",
    )
    .expect_err("unknown permission value is invalid");
    let msg = config.to_string();
    assert!(msg.contains("unknown variant"), "{msg}");
    assert!(msg.contains("read-only"), "{msg}");
    assert!(msg.contains("read-write"), "{msg}");
}

#[test]
fn shared_rejects_boolean_permissions() {
    let config = Config::parse(
        "[[shared]]\n\
         source = \"~/notes\"\n\
         target = \"/home/silo/notes\"\n\
         permission = false\n",
    )
    .expect_err("a boolean is not a permission");
    // The error points at the offending line; the exact wording of the type
    // error belongs to the TOML parser and is not asserted here.
    assert!(config.to_string().contains("line 4"));
}

#[test]
fn shared_reject_relative_sources() {
    let config = Config::parse("[[shared]]\nsource = \"notes\"\ntarget = \"/home/silo/notes\"\n")
        .expect_err("relative source is invalid");
    let msg = config.to_string();
    assert!(msg.contains("does not start with a bare `~`"), "{msg}");
    assert!(msg.contains("`~/notes`"), "{msg}");
}

#[test]
fn shared_rejects_user_tilde_paths() {
    // `~user/x` starts with `~` as text but not as a path component;
    // only a bare `~` is expanded, so such sources must not be mounted
    // against the working directory.
    let config = Config::parse(
        "[[shared]]\n\
         source = \"~user/notes\"\n\
         target = \"/home/silo/notes\"\n",
    )
    .expect_err("~user source is invalid");
    let msg = config.to_string();
    assert!(msg.contains("does not start with a bare `~`"), "{msg}");
    assert!(msg.contains("`~user` paths are not supported"), "{msg}");
}

#[test]
fn shared_accepts_tilde_sources() {
    let config = Config::parse("[[shared]]\nsource = \"~/notes\"\ntarget = \"/home/silo/notes\"\n")
        .expect("tilde source is valid");
    assert_eq!(config.shared[0].source, PathBuf::from("~/notes"));
}

#[test]
fn shared_reports_and_ignores_unknown_keys() {
    let (config, unknown_keys) = Config::parse_with_unknown_keys(
        "[[shared]]\n\
         source = \"~/notes\"\n\
         target = \"/home/silo/notes\"\n\
         future_key = 42\n",
    )
    .expect("unknown key is ignored");
    assert_eq!(config.shared[0].permission, Permission::ReadOnly);
    assert_eq!(unknown_keys, ["shared.0.future_key"]);
}

#[test]
fn shared_reject_relative_targets() {
    let config = Config::parse("[[shared]]\nsource = \"~/notes\"\ntarget = \"notes\"\n")
        .expect_err("relative target is invalid");
    let msg = config.to_string();
    assert!(msg.contains("shared entry 1"), "{msg}");
    assert!(msg.contains("target path is not absolute"), "{msg}");
}

#[test]
fn shared_reject_empty_source() {
    let config = Config::parse("[[shared]]\nsource = \"\"\ntarget = \"/home/silo/notes\"\n")
        .expect_err("empty source is invalid");
    let msg = config.to_string();
    assert!(msg.contains("source path is empty"), "{msg}");
}

#[test]
fn shared_reject_empty_target() {
    let config = Config::parse("[[shared]]\nsource = \"~/notes\"\ntarget = \"\"\n")
        .expect_err("empty target is invalid");
    assert!(config.to_string().contains("target path is empty"));
}

#[test]
fn shared_reject_colon_paths() {
    let config =
        Config::parse("[[shared]]\nsource = \"/tmp/a:b\"\ntarget = \"/home/silo/notes\"\n")
            .expect_err("colon breaks the volume spec");
    let msg = config.to_string();
    assert!(msg.contains("source path contains `:`"), "{msg}");
}

#[test]
fn shared_report_every_invalid_entry() {
    let config = Config::parse(
        "[[shared]]\nsource = \"~/a\"\ntarget = \"relative\"\n\
         [[shared]]\nsource = \"~/b\"\ntarget = \"/tmp/c:d\"\n",
    )
    .expect_err("both shared entries are invalid");
    let msg = config.to_string();
    assert!(msg.contains("shared entry 1"), "{msg}");
    assert!(msg.contains("shared entry 2"), "{msg}");
    assert!(msg.contains("not absolute"), "{msg}");
    assert!(msg.contains("target path contains `:`"), "{msg}");
}
