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
    assert_eq!(from_file.read_only_git, Config::default().read_only_git);
}

#[test]
fn unknown_keys_are_ignored() {
    let config = Config::parse("[image]\nfuture_key = 42\n[future]\nsome_setting = \"bar\"")
        .expect("config with unknown keys parses");
    assert!(config.image.dockerfile.is_none());
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
fn run_dir_prefers_xdg_config_home() {
    let path = run_dir_from(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/user")))
        .expect("path resolves");
    assert_eq!(path, Path::new("/xdg/silo/run"));
}

#[test]
fn run_dir_falls_back_to_home_config() {
    let path = run_dir_from(None, Some(OsStr::new("/home/user"))).expect("path resolves");
    assert_eq!(path, Path::new("/home/user/.config/silo/run"));
}

#[test]
fn run_dir_ignores_relative_xdg_config_home() {
    let path = run_dir_from(
        Some(OsStr::new("relative/xdg")),
        Some(OsStr::new("/home/user")),
    )
    .expect("path resolves");
    assert_eq!(path, Path::new("/home/user/.config/silo/run"));
}

#[test]
fn run_dir_returns_none_without_home() {
    assert!(run_dir_from(None, None).is_none());
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
fn shared_ignores_unknown_keys() {
    // A config written for a future silo still works: unknown keys in a
    // shared entry are ignored.
    let config = Config::parse(
        "[[shared]]\n\
         source = \"~/notes\"\n\
         target = \"/home/silo/notes\"\n\
         future_key = 42\n",
    )
    .expect("unknown key is ignored");
    assert_eq!(config.shared[0].permission, Permission::ReadOnly);
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
