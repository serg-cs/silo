use super::*;
use std::fs;
use std::path::Path;

#[test]
fn dockerfile_embeds_ubuntu_latest() {
    assert!(
        DOCKERFILE
            .lines()
            .any(|line| line.trim() == "FROM ubuntu:latest")
    );
}

#[test]
fn image_tag_is_silo_latest() {
    assert_eq!(IMAGE_TAG, "silo:latest");
}

#[test]
fn build_command_targets_embedded_dockerfile() {
    let command = build_command(Path::new("/tmp/Dockerfile"), Path::new("/tmp/context"));
    let program = command.get_program().to_str().expect("program is UTF-8");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(program, CONTAINER_BIN);
    assert_eq!(
        args,
        ["build", "--file", "/tmp/Dockerfile", "--tag", "silo:latest", "--pull", "/tmp/context"]
    );
}

#[test]
fn run_command_starts_interactive_shell() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = run_command(true, Path::new("/tmp/project"), &ids, &RunFiles::default(), "silo-123")
        .expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args,
        [
            "run",
            "--name",
            "silo-123",
            "--rm",
            "-i",
            "-t",
            "-v",
            "/tmp/project:/home/silo/project",
            "-w",
            "/home/silo/project",
            "--env",
            "SILO_UID=501",
            "--env",
            "SILO_GID=20",
            "silo:latest",
        ]
    );
}

#[test]
fn run_command_omits_pty_when_not_interactive() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = run_command(false, Path::new("/tmp/project"), &ids, &RunFiles::default(), "silo-123")
        .expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args,
        [
            "run",
            "--name",
            "silo-123",
            "--rm",
            "-i",
            "-v",
            "/tmp/project:/home/silo/project",
            "-w",
            "/home/silo/project",
            "--env",
            "SILO_UID=501",
            "--env",
            "SILO_GID=20",
            "silo:latest",
        ]
    );
}

#[test]
fn run_command_places_shared_dir_in_container_home() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = run_command(true, Path::new("/home/user/src/silo"), &ids, &RunFiles::default(), "silo-123")
        .expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args[
            args.iter().position(|arg| *arg == "-v").expect("volume flag") + 1
        ],
        "/home/user/src/silo:/home/silo/silo"
    );
    assert_eq!(
        args[
            args.iter().position(|arg| *arg == "-w").expect("workdir flag") + 1
        ],
        "/home/silo/silo"
    );
}

#[test]
fn run_command_rejects_sharing_root() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let err = run_command(true, Path::new("/"), &ids, &RunFiles::default(), "silo-123")
        .expect_err("root has no name");
    assert!(err.to_string().contains("cannot share the root directory"));
}

#[test]
fn run_command_rejects_paths_with_colons() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let err = run_command(true, Path::new("/tmp/foo:bar"), &ids, &RunFiles::default(), "silo-123")
        .expect_err("colon is a separator");
    assert!(err.to_string().contains("cannot share"));
    assert!(err.to_string().contains("without `:`"));
}

#[cfg(unix)]
#[test]
fn run_command_rejects_non_utf8_paths() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let path = Path::new(OsStr::from_bytes(b"/tmp/bad\xFFdir"));
    let err = run_command(true, path, &ids, &RunFiles::default(), "silo-123")
        .expect_err("name is not valid UTF-8");
    assert!(err.to_string().contains("cannot share"));
    assert!(err.to_string().contains("valid UTF-8"));
}

#[cfg(unix)]
#[test]
fn host_ids_reports_numeric_ids() {
    let ids = host_ids().expect("host ids resolve");
    assert!(ids.uid.chars().all(|c| c.is_ascii_digit()));
    assert!(ids.gid.chars().all(|c| c.is_ascii_digit()));
}

#[test]
fn inspect_error_reports_missing_image() {
    let err = inspect_error("Error: image not found: silo:latest");
    assert!(err.to_string().contains("not built yet"));
}

#[test]
fn inspect_error_reports_probe_failures() {
    let err = inspect_error("error: container runtime is not running");
    assert!(!err.to_string().contains("not built yet"));
    assert!(err.to_string().contains("runtime is not running"));
}

#[cfg(unix)]
#[test]
fn exit_code_reports_signals_as_128_plus_signal() {
    use std::os::unix::process::ExitStatusExt;
    let status = ExitStatus::from_raw(0x0009); // killed by signal 9
    assert_eq!(status.code(), None);
    assert_eq!(status.signal(), Some(9));
    assert_eq!(exit_code(status), ExitCode::from(137));
}

#[test]
fn build_dir_removes_itself_on_drop() {
    let build_dir = BuildDir::create().expect("build dir creation succeeds");
    let path = build_dir.path().to_path_buf();
    assert!(path.exists());
    drop(build_dir);
    assert!(!path.exists());
}

#[test]
fn validate_dockerfile_rejects_empty_paths() {
    let err = validate_dockerfile(Path::new("")).expect_err("empty path is invalid");
    assert!(err.to_string().contains("empty"));
}

#[test]
fn validate_dockerfile_rejects_missing_paths() {
    let path = std::env::temp_dir().join(format!("silo-test-missing-{}", std::process::id()));
    let _ = fs::remove_file(&path);
    let err = validate_dockerfile(&path).expect_err("missing path is invalid");
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn validate_dockerfile_rejects_directories() {
    let path = std::env::temp_dir().join(format!("silo-test-dir-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("test dir creation succeeds");
    let err = validate_dockerfile(&path).expect_err("directory is not a file");
    assert!(err.to_string().contains("is not a file"));
    let _ = fs::remove_dir_all(&path);
}

#[test]
fn validate_dockerfile_accepts_regular_files() {
    let path = std::env::temp_dir().join(format!("silo-test-file-{}", std::process::id()));
    let _ = fs::remove_file(&path);
    fs::write(&path, "FROM scratch").expect("write succeeds");
    validate_dockerfile(&path).expect("regular file is valid");
    let _ = fs::remove_file(&path);
}

#[test]
fn dockerfile_context_uses_the_dockerfile_directory() {
    assert_eq!(
        dockerfile_context(Path::new("/home/user/project/Dockerfile")),
        Path::new("/home/user/project")
    );
}

#[test]
fn dockerfile_context_resolves_relative_parents() {
    assert_eq!(
        dockerfile_context(Path::new("images/dev/Dockerfile")),
        Path::new("images/dev")
    );
}

#[test]
fn dockerfile_context_falls_back_to_current_directory() {
    assert_eq!(dockerfile_context(Path::new("Dockerfile")), Path::new("."));
}

/// Temporary directory that removes itself on drop, so cleanup also runs on
/// test failures.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "silo-container-test-{}-{name}",
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
fn run_command_injects_run_files() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let run_files = RunFiles {
        env_file: Some(PathBuf::from("/home/user/.config/silo/run/env")),
        mounts: vec![
            RunMount {
                host: PathBuf::from("/home/user/.agents"),
                dest: PathBuf::from("/home/silo/.agents"),
            },
            RunMount {
                host: PathBuf::from("/home/user/.config/opencode"),
                dest: PathBuf::from("/home/silo/.config/opencode"),
            },
        ],
    };
    let command = run_command(true, Path::new("/tmp/project"), &ids, &run_files, "silo-123")
        .expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args,
        [
            "run",
            "--name",
            "silo-123",
            "--rm",
            "-i",
            "-t",
            "-v",
            "/tmp/project:/home/silo/project",
            "-w",
            "/home/silo/project",
            "-v",
            "/home/user/.agents:/home/silo/.agents:ro",
            "-v",
            "/home/user/.config/opencode:/home/silo/.config/opencode:ro",
            "--env-file",
            "/home/user/.config/silo/run/env",
            "--env",
            "SILO_UID=501",
            "--env",
            "SILO_GID=20",
            "silo:latest",
        ]
    );
}

#[test]
fn discover_returns_empty_for_missing_run_dir() {
    let dir = TestDir::new("missing");
    let run_files = RunFiles::discover(&dir.path().join("run")).expect("missing dir is empty");
    assert_eq!(run_files, RunFiles::default());
}

#[test]
fn discover_skips_env_and_collects_mounts() {
    let dir = TestDir::new("discover");
    let run_dir = dir.path().join("run");
    fs::create_dir_all(run_dir.join(".config/opencode")).expect("dir creation succeeds");
    fs::create_dir_all(run_dir.join(".agents")).expect("dir creation succeeds");
    fs::write(run_dir.join(".gitconfig"), "[user]\n").expect("write succeeds");
    fs::write(run_dir.join("README"), "hello\n").expect("write succeeds");
    fs::write(run_dir.join("env"), "OPENAI_API_KEY=x\n").expect("write succeeds");

    let run_files = RunFiles::discover(&run_dir).expect("discover succeeds");
    assert_eq!(run_files.env_file, Some(run_dir.join("env")));
    assert_eq!(
        run_files.mounts,
        vec![
            RunMount {
                host: fs::canonicalize(run_dir.join(".agents")).expect("host resolves"),
                dest: PathBuf::from("/home/silo/.agents"),
            },
            RunMount {
                host: fs::canonicalize(run_dir.join(".config")).expect("host resolves"),
                dest: PathBuf::from("/home/silo/.config"),
            },
            RunMount {
                host: fs::canonicalize(run_dir.join(".gitconfig")).expect("host resolves"),
                dest: PathBuf::from("/home/silo/.gitconfig"),
            },
            RunMount {
                host: fs::canonicalize(run_dir.join("README")).expect("host resolves"),
                dest: PathBuf::from("/home/silo/README"),
            },
        ]
    );
}

#[test]
fn discover_rejects_directory_named_env() {
    let dir = TestDir::new("env-dir");
    let run_dir = dir.path().join("run");
    fs::create_dir_all(run_dir.join("env")).expect("dir creation succeeds");

    let err = RunFiles::discover(&run_dir).expect_err("env must be a file");
    assert!(err.to_string().contains("`env`"));
    assert!(err.to_string().contains("is not a file"));
}

#[test]
fn discover_rejects_names_with_colons() {
    let dir = TestDir::new("colon");
    let run_dir = dir.path().join("run");
    fs::create_dir_all(&run_dir).expect("dir creation succeeds");
    fs::write(run_dir.join("foo:bar"), "x").expect("write succeeds");

    let err = RunFiles::discover(&run_dir).expect_err("colon breaks the volume spec");
    assert!(err.to_string().contains("cannot mount"));
    assert!(err.to_string().contains("without `:`"));
}

#[cfg(unix)]
#[test]
fn discover_rejects_non_utf8_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = TestDir::new("non-utf8");
    let run_dir = dir.path().join("run");
    fs::create_dir_all(&run_dir).expect("dir creation succeeds");
    fs::write(run_dir.join(OsStr::from_bytes(b".agents\xFF")), "x").expect("write succeeds");

    let err = RunFiles::discover(&run_dir).expect_err("name is not valid UTF-8");
    assert!(err.to_string().contains("cannot mount"));
    assert!(err.to_string().contains("valid UTF-8"));
}

#[cfg(unix)]
#[test]
fn discover_resolves_symlinked_entries() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("symlink");
    let run_dir = dir.path().join("run");
    let target = dir.path().join("real-agents");
    fs::create_dir_all(&run_dir).expect("dir creation succeeds");
    fs::create_dir_all(&target).expect("dir creation succeeds");
    fs::write(target.join("agent.toml"), "x").expect("write succeeds");
    symlink(&target, run_dir.join(".agents")).expect("symlink creation succeeds");

    let run_files = RunFiles::discover(&run_dir).expect("discover succeeds");
    assert_eq!(
        run_files.mounts,
        vec![RunMount {
            host: fs::canonicalize(&target).expect("target canonicalizes"),
            dest: PathBuf::from("/home/silo/.agents"),
        }]
    );
}

#[cfg(unix)]
#[test]
fn discover_rejects_broken_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("broken-link");
    let run_dir = dir.path().join("run");
    fs::create_dir_all(&run_dir).expect("dir creation succeeds");
    symlink(dir.path().join("missing"), run_dir.join(".agents"))
        .expect("symlink creation succeeds");

    let err = RunFiles::discover(&run_dir).expect_err("broken symlink errors");
    assert!(err.to_string().contains(".agents"));
    assert!(err.to_string().contains("cannot resolve"));
}

#[test]
fn discover_rejects_existing_non_directory() {
    let dir = TestDir::new("not-a-dir");
    fs::write(dir.path().join("run"), "x").expect("write succeeds");

    let err = RunFiles::discover(&dir.path().join("run")).expect_err("file is not a directory");
    assert!(err.to_string().contains("is not a directory"));
}

#[test]
fn container_id_embeds_the_pid() {
    assert_eq!(container_id(), format!("silo-{}", std::process::id()));
}

#[test]
fn container_id_starts_with_the_prefix() {
    assert!(container_id().starts_with(CONTAINER_NAME_PREFIX));
}

#[test]
fn state_dir_follows_the_config_dir() {
    assert_eq!(
        container_state_dir_from(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/user"))),
        Some(PathBuf::from("/xdg/silo/containers"))
    );
    assert_eq!(
        container_state_dir_from(None, Some(OsStr::new("/home/user"))),
        Some(PathBuf::from("/home/user/.config/silo/containers"))
    );
    assert_eq!(container_state_dir_from(None, None), None);
}

#[test]
fn register_and_unregister_markers() {
    let dir = TestDir::new("markers");
    register_container_in(dir.path(), "silo-123").expect("register succeeds");
    assert!(dir.path().join("silo-123").is_file());
    unregister_container_in(dir.path(), "silo-123");
    assert!(!dir.path().join("silo-123").exists());
}

#[test]
fn owner_alive_detects_live_processes() {
    let pid = libc::pid_t::try_from(std::process::id()).expect("pid fits in pid_t");
    assert!(owner_alive(pid));
}

#[test]
fn owner_alive_detects_dead_processes() {
    let mut child = Command::new("true").spawn().expect("spawn succeeds");
    let pid = libc::pid_t::try_from(child.id()).expect("pid fits in pid_t");
    child.wait().expect("wait succeeds");
    assert!(!owner_alive(pid));
}

#[test]
fn sweep_removes_markers_of_dead_owners_only() {
    let dir = TestDir::new("sweep");
    // Live owner (this process): its container must be left alone.
    let live = format!("silo-{}", std::process::id());
    register_container_in(dir.path(), &live).expect("register succeeds");
    // Dead owner: its container must be swept.
    let mut child = Command::new("true").spawn().expect("spawn succeeds");
    let dead_pid = child.id();
    child.wait().expect("wait succeeds");
    let dead = format!("silo-{dead_pid}");
    register_container_in(dir.path(), &dead).expect("register succeeds");
    // Foreign marker: must be ignored.
    fs::write(dir.path().join("other"), "x").expect("write succeeds");
    // Malformed silo marker: must be ignored.
    fs::write(dir.path().join("silo-not-a-pid"), "x").expect("write succeeds");

    let mut deleted = Vec::new();
    sweep_stale_in(dir.path(), |id| {
        deleted.push(id.to_string());
        Ok(())
    });

    assert_eq!(deleted, vec![dead.clone()]);
    assert!(dir.path().join(&live).exists());
    assert!(!dir.path().join(&dead).exists());
    assert!(dir.path().join("other").exists());
    assert!(dir.path().join("silo-not-a-pid").exists());
}

#[test]
fn sweep_keeps_markers_when_delete_fails() {
    let dir = TestDir::new("sweep-fail");
    let mut child = Command::new("true").spawn().expect("spawn succeeds");
    let dead_pid = child.id();
    child.wait().expect("wait succeeds");
    let dead = format!("silo-{dead_pid}");
    register_container_in(dir.path(), &dead).expect("register succeeds");

    sweep_stale_in(dir.path(), |_| Err(anyhow!("delete failed")));

    assert!(dir.path().join(&dead).exists());
}

#[test]
fn delete_succeeded_accepts_success() {
    let status = Command::new("true").status().expect("true runs");
    assert!(delete_succeeded(status, ""));
}

#[test]
fn delete_succeeded_accepts_not_found() {
    let status = Command::new("false").status().expect("false runs");
    let stderr = "error: container with ID silo-123 not found";
    assert!(delete_succeeded(status, stderr));
}

#[test]
fn delete_succeeded_rejects_other_failures() {
    let status = Command::new("false").status().expect("false runs");
    let stderr = "error: container silo-123 is running and can not be deleted";
    assert!(!delete_succeeded(status, stderr));
}
