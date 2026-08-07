use super::*;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

/// Temporary directory that removes itself on drop, so cleanup also runs on
/// test failures.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "silo-config-test-{}-{name}",
            std::process::id()
        ));
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
    let config =
        Config::parse("[image]\ndockerfile = \"/tmp/Dockerfile\"").expect("config parses");
    assert_eq!(
        config.image.dockerfile.as_deref(),
        Some(Path::new("/tmp/Dockerfile"))
    );
}

#[test]
fn unknown_keys_are_ignored() {
    let config = Config::parse(
        "[image]\nfuture_key = 42\n[future]\nsome_setting = \"bar\"",
    )
    .expect("config with unknown keys parses");
    assert!(config.image.dockerfile.is_none());
}

#[test]
fn invalid_toml_reports_line_and_column() {
    let err = Config::parse("dockerfile =").expect_err("unterminated value is invalid");
    let msg = err.to_string();
    assert!(msg.contains("line 1"), "error points at the location: {msg}");
    assert!(msg.contains("column"), "error points at the location: {msg}");
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
    assert!(msg.contains("line 1"), "error points at the location: {msg}");
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
    let path = config_path_from(Some(OsStr::new("relative/xdg")), Some(OsStr::new("/home/user")))
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
    let path = run_dir_from(Some(OsStr::new("relative/xdg")), Some(OsStr::new("/home/user")))
        .expect("path resolves");
    assert_eq!(path, Path::new("/home/user/.config/silo/run"));
}

#[test]
fn run_dir_returns_none_without_home() {
    assert!(run_dir_from(None, None).is_none());
}
