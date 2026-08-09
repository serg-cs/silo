use super::*;
use crate::config::{Permission, Shared};
use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

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
fn custom_images_use_the_image_agnostic_lifecycle() {
    let mut config = Config::default();
    config.image.dockerfile = Some(PathBuf::from("/tmp/Dockerfile"));

    assert!(uses_isolated_lifecycle(&config, false));
}

#[test]
fn built_in_images_remain_shared_by_default() {
    assert!(!uses_isolated_lifecycle(&Config::default(), false));
    assert!(uses_isolated_lifecycle(&Config::default(), true));
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
        [
            "build",
            "--file",
            "/tmp/Dockerfile",
            "--tag",
            "silo:latest",
            "--pull",
            "/tmp/context"
        ]
    );
}

#[test]
fn run_command_starts_interactive_shell() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_run_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        "silo-123",
        &[],
    )
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
    let command = isolated_run_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        "silo-123",
        &[],
    )
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
    let command = isolated_run_command(
        true,
        Path::new("/home/user/src/silo"),
        &ids,
        &ConfigMounts::default(),
        "silo-123",
        &[],
    )
    .expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args[args
            .iter()
            .position(|arg| *arg == "-v")
            .expect("volume flag")
            + 1],
        "/home/user/src/silo:/home/silo/silo"
    );
    assert_eq!(
        args[args
            .iter()
            .position(|arg| *arg == "-w")
            .expect("workdir flag")
            + 1],
        "/home/silo/silo"
    );
}

#[test]
fn run_command_rejects_sharing_root() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let err = isolated_run_command(
        true,
        Path::new("/"),
        &ids,
        &ConfigMounts::default(),
        "silo-123",
        &[],
    )
    .expect_err("root has no name");
    assert!(err.to_string().contains("cannot share the root directory"));
}

#[test]
fn run_command_rejects_paths_with_colons() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let err = isolated_run_command(
        true,
        Path::new("/tmp/foo:bar"),
        &ids,
        &ConfigMounts::default(),
        "silo-123",
        &[],
    )
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
    let err = isolated_run_command(true, path, &ids, &ConfigMounts::default(), "silo-123", &[])
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

#[test]
fn system_not_started_matches_the_cli_hint() {
    let stderr = "Error: XPC connection error\n\
                  Ensure container system service has been started with \
                  `container system start`.";
    assert!(system_not_started(stderr), "{stderr}");
}

#[test]
fn system_not_started_matches_the_older_wording() {
    assert!(system_not_started(
        "error: container system start has not been run"
    ));
}

#[test]
fn system_not_started_ignores_other_failures() {
    assert!(!system_not_started(
        "error: container runtime is not running"
    ));
    assert!(!system_not_started("Error: image not found: silo:latest"));
    assert!(!system_not_started(""));
}

#[test]
fn run_captured_keeps_stderr_and_status() {
    let mut command = Command::new("sh");
    command.args(["-c", "echo to-stderr >&2; exit 3"]);
    let captured = run_captured(&mut command).expect("command runs");
    assert_eq!(captured.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(captured.stderr).expect("stderr is UTF-8"),
        "to-stderr\n"
    );
}

#[test]
fn trim_captured_keeps_the_tail() {
    let mut captured = vec![b'a'; 300 * 1024];
    captured.extend_from_slice(b"not-started-hint");
    trim_captured(&mut captured);
    assert_eq!(captured.len(), 256 * 1024);
    assert!(captured.ends_with(&b"not-started-hint"[..]));
    assert!(captured.iter().take(4).all(|&byte| byte == b'a'));
}

#[test]
fn trim_captured_leaves_small_output_untouched() {
    let mut captured = b"small".to_vec();
    trim_captured(&mut captured);
    assert_eq!(captured, b"small");
}

/// Appends one line to `path`; the fake probe/boot closures and the shell
/// commands in the orchestration tests write here to record event order.
fn append_log(path: &Path, line: &str) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("log opens");
    writeln!(file, "{line}").expect("log writes");
}

fn read_log(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

const NOT_STARTED_HINT: &str =
    "Ensure container system service has been started with `container system start`.";

#[test]
fn execute_build_boots_before_building_when_the_probe_says_not_started() {
    let dir = TestDir::new("build-boot-first");
    let log = dir.path().join("log");
    let log_path = log.display();
    let boots = Cell::new(0);
    let mut command = Command::new("sh");
    command.args(["-c", &format!("echo build >> '{log_path}'")]);
    let result = execute_build_with(
        &mut command,
        || Ok((false, NOT_STARTED_HINT.to_string())),
        || {
            boots.set(boots.get() + 1);
            append_log(&log, "boot");
            true
        },
    )
    .expect("build runs");
    assert_eq!(result, ExitCode::from(0));
    assert_eq!(boots.get(), 1);
    assert_eq!(read_log(&log), ["boot", "build"]);
}

#[test]
fn execute_build_does_not_boot_when_the_probe_says_the_system_is_up() {
    let dir = TestDir::new("build-no-boot");
    let log = dir.path().join("log");
    let log_path = log.display();
    let boots = Cell::new(0);
    let mut command = Command::new("sh");
    command.args(["-c", &format!("echo build >> '{log_path}'")]);
    let result = execute_build_with(
        &mut command,
        || Ok((false, "Error: image not found: silo:latest".to_string())),
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("build runs");
    assert_eq!(result, ExitCode::from(0));
    assert_eq!(boots.get(), 0);
    assert_eq!(read_log(&log), ["build"]);
}

#[test]
fn execute_build_boots_and_retries_once_when_the_probe_misses() {
    let dir = TestDir::new("build-retry");
    let log = dir.path().join("log");
    let marker = dir.path().join("built");
    let log_path = log.display();
    let marker_path = marker.display();
    let boots = Cell::new(0);
    // The probe thinks the system is up (the image is simply not found), so
    // no pre-build boot happens; the build then fails with the not-started
    // hint, the fallback boots, and the retried build succeeds.
    let mut command = Command::new("sh");
    command.args([
        "-c",
        &format!(
            "echo build >> '{log_path}'; \
         if [ ! -e '{marker_path}' ]; then \
           touch '{marker_path}'; \
           echo '{NOT_STARTED_HINT}' >&2; \
           exit 1; \
         fi"
        ),
    ]);
    let result = execute_build_with(
        &mut command,
        || Ok((false, "Error: image not found: silo:latest".to_string())),
        || {
            boots.set(boots.get() + 1);
            append_log(&log, "boot");
            true
        },
    )
    .expect("build runs");
    assert_eq!(result, ExitCode::from(0));
    assert_eq!(boots.get(), 1);
    assert_eq!(read_log(&log), ["build", "boot", "build"]);
}

#[test]
fn execute_build_passes_through_failures_unrelated_to_the_system() {
    let boots = Cell::new(0);
    let mut command = Command::new("sh");
    command.args(["-c", "echo 'Error: Dockerfile is missing' >&2; exit 2"]);
    let result = execute_build_with(
        &mut command,
        || Ok((false, "Error: image not found: silo:latest".to_string())),
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("build runs");
    assert_eq!(result, ExitCode::from(2));
    assert_eq!(boots.get(), 0);
}

#[test]
fn execute_build_passes_through_the_first_exit_code_when_the_boot_fails() {
    let boots = Cell::new(0);
    let mut command = Command::new("sh");
    command.args(["-c", &format!("echo '{NOT_STARTED_HINT}' >&2; exit 1")]);
    let result = execute_build_with(
        &mut command,
        || Ok((false, "Error: image not found: silo:latest".to_string())),
        || {
            boots.set(boots.get() + 1);
            false
        },
    )
    .expect("build runs");
    assert_eq!(result, ExitCode::from(1));
    assert_eq!(boots.get(), 1);
}

#[test]
fn inspect_image_boots_and_reprobes_when_the_system_is_not_started() {
    let probes = Cell::new(0);
    let boots = Cell::new(0);
    let (exists, _) = inspect_image_with(
        || {
            probes.set(probes.get() + 1);
            if probes.get() == 1 {
                Ok((false, NOT_STARTED_HINT.to_string()))
            } else {
                Ok((true, String::new()))
            }
        },
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("probe runs");
    assert!(exists);
    assert_eq!(probes.get(), 2);
    assert_eq!(boots.get(), 1);
}

#[test]
fn inspect_image_reports_the_original_stderr_when_the_boot_fails() {
    let boots = Cell::new(0);
    let (exists, stderr) = inspect_image_with(
        || Ok((false, NOT_STARTED_HINT.to_string())),
        || {
            boots.set(boots.get() + 1);
            false
        },
    )
    .expect("probe runs");
    assert!(!exists);
    assert_eq!(stderr, NOT_STARTED_HINT);
    assert_eq!(boots.get(), 1);
}

#[test]
fn inspect_image_does_not_boot_when_the_system_is_up() {
    let boots = Cell::new(0);
    let (exists, stderr) = inspect_image_with(
        || Ok((false, "Error: image not found: silo:latest".to_string())),
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("probe runs");
    assert!(!exists);
    assert_eq!(stderr, "Error: image not found: silo:latest");
    assert_eq!(boots.get(), 0);
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
        let path =
            std::env::temp_dir().join(format!("silo-container-test-{}-{name}", std::process::id()));
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
fn run_command_appends_the_passed_command() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_run_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        "silo-123",
        &[
            OsString::from("codex"),
            OsString::from("--model"),
            OsString::from("compact"),
        ],
    )
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
            "codex",
            "--model",
            "compact",
        ]
    );
}

/// Returns the `-v` volume specs of the command, in order.
fn volume_specs(command: &Command) -> Vec<String> {
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    args.windows(2)
        .filter(|pair| pair[0] == "-v")
        .map(|pair| pair[1].to_string())
        .collect()
}

/// The canonical form of a path, as `git_mount_host` emits it: the tests
/// must compare against the resolved path, since e.g. macOS `/var` is a
/// symlink to `/private/var` while `dir.path()` is the lexical path.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("path resolves")
}

#[test]
fn run_command_mounts_git_read_only() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_run_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts {
            git: Some(PathBuf::from("/tmp/project/.git")),
            shared: vec![],
        },
        "silo-123",
        &[],
    )
    .expect("command builds");
    assert_eq!(
        volume_specs(&command),
        [
            "/tmp/project:/home/silo/project",
            "/tmp/project/.git:/home/silo/project/.git:ro",
        ]
    );
}

#[test]
fn run_command_omits_git_mount_when_absent() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_run_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        "silo-123",
        &[],
    )
    .expect("command builds");
    assert_eq!(volume_specs(&command), ["/tmp/project:/home/silo/project"]);
}

#[test]
fn run_command_mounts_configured_shared() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let shared = vec![
        ResolvedShared {
            host: PathBuf::from("/home/user/.ssh"),
            dest: PathBuf::from("/home/silo/.ssh"),
            permission: Permission::ReadOnly,
        },
        ResolvedShared {
            host: PathBuf::from("/home/user/Downloads"),
            dest: PathBuf::from("/home/silo/Downloads"),
            permission: Permission::ReadWrite,
        },
    ];
    let command = isolated_run_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts { git: None, shared },
        "silo-123",
        &[],
    )
    .expect("command builds");
    assert_eq!(
        volume_specs(&command),
        [
            "/tmp/project:/home/silo/project",
            "/home/user/.ssh:/home/silo/.ssh:ro",
            "/home/user/Downloads:/home/silo/Downloads",
        ]
    );
}

#[test]
fn run_command_orders_git_then_shared_mounts() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let shared = vec![ResolvedShared {
        host: PathBuf::from("/home/user/data"),
        dest: PathBuf::from("/data"),
        permission: Permission::ReadWrite,
    }];
    let command = isolated_run_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts {
            git: Some(PathBuf::from("/tmp/project/.git")),
            shared,
        },
        "silo-123",
        &[],
    )
    .expect("command builds");
    assert_eq!(
        volume_specs(&command),
        [
            "/tmp/project:/home/silo/project",
            "/tmp/project/.git:/home/silo/project/.git:ro",
            "/home/user/data:/data",
        ]
    );
}

#[test]
fn mount_conflicts_reports_read_write_shared_over_git() {
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        true,
        &[ResolvedShared {
            host: PathBuf::from("/home/user/git"),
            dest: PathBuf::from("/home/silo/project/.git"),
            permission: Permission::ReadWrite,
        }],
    );
    assert_eq!(conflicts.len(), 1);
    let msg = &conflicts[0];
    assert!(msg.contains("read-write shared mount"), "{msg}");
    assert!(msg.contains("`.git`"), "{msg}");
}

#[test]
fn mount_conflicts_reports_read_write_shared_under_git() {
    // A read-write mount at `.git/objects` is a deeper, independent mount
    // and really is writable, so the `.git` protection is defeated.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        true,
        &[ResolvedShared {
            host: PathBuf::from("/home/user/git"),
            dest: PathBuf::from("/home/silo/project/.git/objects"),
            permission: Permission::ReadWrite,
        }],
    );
    assert_eq!(conflicts.len(), 1);
    assert!(
        conflicts[0].contains("read-write shared mount"),
        "{}",
        conflicts[0]
    );
    assert!(conflicts[0].contains(".git"), "{}", conflicts[0]);
}

#[test]
fn mount_conflicts_keeps_gitignore_silent() {
    // `.gitignore` is a sibling of `.git`, not a descendant: component
    // comparison must not flag it.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        true,
        &[ResolvedShared {
            host: PathBuf::from("/home/user/gitignore"),
            dest: PathBuf::from("/home/silo/project/.gitignore"),
            permission: Permission::ReadWrite,
        }],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn mount_conflicts_keeps_ancestor_shared_silent() {
    // A mount at an ancestor does not hide the child `.git` mount: the
    // read-only protection stays in place, so no warning.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        true,
        &[ResolvedShared {
            host: PathBuf::from("/home/user/project"),
            dest: PathBuf::from("/home/silo/project"),
            permission: Permission::ReadWrite,
        }],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn mount_conflicts_keeps_read_only_shared_over_git_silent() {
    // Read-only over read-only is harmless: the `.git` protection stays
    // intact either way.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        true,
        &[ResolvedShared {
            host: PathBuf::from("/home/user/git"),
            dest: PathBuf::from("/home/silo/project/.git"),
            permission: Permission::ReadOnly,
        }],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn mount_conflicts_is_silent_without_overlaps() {
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        true,
        &[ResolvedShared {
            host: PathBuf::from("/home/user/data"),
            dest: PathBuf::from("/data"),
            permission: Permission::ReadWrite,
        }],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn git_mount_host_returns_canonical_git_when_enabled() {
    let dir = TestDir::new("git-mount");
    fs::create_dir_all(dir.path().join(".git")).expect("mkdir succeeds");
    assert_eq!(
        git_mount_host(dir.path(), true),
        Some(canonical(&dir.path().join(".git")))
    );
}

#[test]
fn git_mount_host_disabled_returns_none() {
    let dir = TestDir::new("git-off");
    fs::create_dir_all(dir.path().join(".git")).expect("mkdir succeeds");
    assert!(git_mount_host(dir.path(), false).is_none());
}

#[test]
fn git_mount_host_returns_none_without_git() {
    let dir = TestDir::new("no-git");
    assert!(git_mount_host(dir.path(), true).is_none());
}

#[cfg(unix)]
#[test]
fn git_mount_host_skips_git_escaping_the_project() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("git-escape");
    let outside = std::env::temp_dir().join(format!("silo-test-outside-{}", std::process::id()));
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).expect("mkdir succeeds");
    symlink(&outside, dir.path().join(".git")).expect("symlink succeeds");
    assert!(
        git_mount_host(dir.path(), true).is_none(),
        "an escaping .git symlink must not become a mount"
    );
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn resolve_shared_returns_empty_without_shared_mounts() {
    let dir = TestDir::new("shared-empty");
    let resolved = resolve_shared(&[], Some(dir.path())).expect("no shared mounts resolve");
    assert!(resolved.is_empty());
}

#[cfg(unix)]
#[test]
fn resolve_shared_expands_tilde_and_canonicalizes_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("shared-tilde");
    let real = dir.path().join("real-agents");
    fs::create_dir_all(&real).expect("mkdir succeeds");
    symlink(&real, dir.path().join("agents")).expect("symlink succeeds");
    let shared = [Shared {
        source: PathBuf::from("~/agents"),
        target: PathBuf::from("/home/silo/.agents"),
        permission: Permission::ReadOnly,
    }];
    let resolved = resolve_shared(&shared, Some(dir.path())).expect("shared mount resolves");
    assert_eq!(
        resolved,
        vec![ResolvedShared {
            host: canonical(&real),
            dest: PathBuf::from("/home/silo/.agents"),
            permission: Permission::ReadOnly,
        }]
    );
}

#[test]
fn resolve_shared_keeps_absolute_sources() {
    let dir = TestDir::new("shared-abs");
    let source = dir.path().join("data");
    fs::create_dir_all(&source).expect("mkdir succeeds");
    let shared = [Shared {
        source: source.clone(),
        target: PathBuf::from("/data"),
        permission: Permission::ReadWrite,
    }];
    let resolved = resolve_shared(&shared, Some(dir.path())).expect("shared mount resolves");
    assert_eq!(
        resolved,
        vec![ResolvedShared {
            host: canonical(&source),
            dest: PathBuf::from("/data"),
            permission: Permission::ReadWrite,
        }]
    );
}

#[test]
fn resolve_shared_errors_on_missing_sources() {
    let dir = TestDir::new("shared-missing");
    let shared = [Shared {
        source: PathBuf::from("~/nope"),
        target: PathBuf::from("/home/silo/nope"),
        permission: Permission::ReadOnly,
    }];
    let err = resolve_shared(&shared, Some(dir.path())).expect_err("missing source errors");
    let msg = err.to_string();
    assert!(msg.contains("cannot resolve source"), "{msg}");
    assert!(msg.contains("`~/nope`"), "{msg}");
    assert!(msg.contains("/home/silo/nope"), "{msg}");
}

#[test]
fn resolve_shared_keeps_config_order() {
    let dir = TestDir::new("shared-order");
    let first = dir.path().join("a");
    let second = dir.path().join("b");
    fs::create_dir_all(&first).expect("mkdir succeeds");
    fs::create_dir_all(&second).expect("mkdir succeeds");
    let shared = [
        Shared {
            source: PathBuf::from("~/a"),
            target: PathBuf::from("/mnt/shared"),
            permission: Permission::ReadOnly,
        },
        Shared {
            source: PathBuf::from("~/b"),
            target: PathBuf::from("/mnt/shared"),
            permission: Permission::ReadWrite,
        },
    ];
    let resolved = resolve_shared(&shared, Some(dir.path())).expect("shared mounts resolve");
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].host, canonical(&first));
    assert_eq!(resolved[1].host, canonical(&second));
}

#[test]
fn expand_tilde_replaces_leading_tilde() {
    let home = Path::new("/home/user");
    assert_eq!(
        expand_tilde(Path::new("~/notes"), Some(home)),
        PathBuf::from("/home/user/notes")
    );
    assert_eq!(
        expand_tilde(Path::new("~"), Some(home)),
        PathBuf::from("/home/user")
    );
}

#[test]
fn expand_tilde_leaves_other_paths_untouched() {
    let home = Path::new("/home/user");
    assert_eq!(
        expand_tilde(Path::new("/abs/path"), Some(home)),
        PathBuf::from("/abs/path")
    );
    assert_eq!(
        expand_tilde(Path::new("~other/x"), Some(home)),
        PathBuf::from("~other/x")
    );
    assert_eq!(
        expand_tilde(Path::new("rel/path"), Some(home)),
        PathBuf::from("rel/path")
    );
}

#[test]
fn expand_tilde_without_home_keeps_tilde_paths() {
    assert_eq!(
        expand_tilde(Path::new("~/notes"), None),
        PathBuf::from("~/notes")
    );
}

#[test]
fn expand_tilde_with_empty_home_keeps_tilde_paths() {
    // An empty HOME must behave like an unset one: expanding against it
    // would turn `~/x` into a relative path resolved against the cwd.
    assert_eq!(
        expand_tilde(Path::new("~/notes"), Some(Path::new(""))),
        PathBuf::from("~/notes")
    );
}

#[test]
fn isolated_container_id_embeds_the_pid() {
    assert_eq!(
        isolated_container_id(),
        format!("silo-{}", std::process::id())
    );
}

#[test]
fn isolated_container_id_starts_with_the_prefix() {
    assert!(isolated_container_id().starts_with(CONTAINER_NAME_PREFIX));
}

#[test]
fn project_container_id_is_stable_and_path_specific() {
    let first = project_container_id(Path::new("/work/one/project"));
    let same = project_container_id(Path::new("/work/one/project"));
    let other = project_container_id(Path::new("/work/two/project"));
    assert_eq!(first, same);
    assert_ne!(first, other, "equal basenames must not collide");
    assert_eq!(
        first.len(),
        CONTAINER_NAME_PREFIX.len() + PROJECT_DIGEST_HEX_LEN
    );
    assert!(
        first
            .strip_prefix(CONTAINER_NAME_PREFIX)
            .expect("prefix exists")
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );
}

#[cfg(unix)]
#[test]
fn project_identity_canonicalizes_symlinked_working_directories() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("project-symlink");
    let project_dir = dir.path().join("real-project");
    let link = dir.path().join("project-link");
    fs::create_dir(&project_dir).expect("project directory creates");
    symlink(&project_dir, &link).expect("project symlink creates");

    let direct = Project::from_path(&project_dir).expect("direct project resolves");
    let linked = Project::from_path(&link).expect("linked project resolves");
    assert_eq!(direct, linked);
    assert_eq!(direct.root, canonical(&project_dir));
    assert_eq!(direct.workdir, PathBuf::from("/home/silo/real-project"));
}

fn test_project(root: &str) -> Project {
    let root = PathBuf::from(root);
    Project {
        workdir: Path::new(CONTAINER_HOME).join(root.file_name().expect("project has a name")),
        id: project_container_id(&root),
        root,
    }
}

#[test]
fn create_command_starts_a_detached_keeper() {
    let project = test_project("/tmp/project");
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = create_command(
        &project,
        &ids,
        &ConfigMounts::default(),
        Path::new("/tmp/project.cid"),
    )
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
            project.id.as_str(),
            "--cidfile",
            "/tmp/project.cid",
            "-d",
            "-v",
            "/tmp/project:/home/silo/project",
            "-w",
            "/home/silo/project",
            "--env",
            "SILO_UID=501",
            "--env",
            "SILO_GID=20",
            "silo:latest",
            "sleep",
            "infinity",
        ]
    );
}

#[test]
fn exec_command_attaches_as_silo_with_home() {
    let project = test_project("/tmp/project");
    let command = exec_command(
        true,
        &project,
        &[OsString::from("codex"), OsString::from("--compact")],
    );
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args,
        [
            "exec",
            "-i",
            "-t",
            "--user",
            "silo",
            "--workdir",
            "/home/silo/project",
            "--env",
            "HOME=/home/silo",
            project.id.as_str(),
            "codex",
            "--compact",
        ]
    );
}

#[test]
fn exec_command_uses_nu_and_omits_tty_without_a_terminal() {
    let project = test_project("/tmp/project");
    let command = exec_command(false, &project, &[]);
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(args[0..2], ["exec", "-i"]);
    assert!(!args.contains(&"-t"));
    assert_eq!(args.last(), Some(&DEFAULT_SESSION_COMMAND));
}

#[test]
fn inspect_parser_accepts_current_nested_state() {
    let json = br#"[{"id":"silo-test","status":{"state":"running","networks":[]}}]"#;
    assert_eq!(
        parse_container_state(json, "silo-test").expect("state parses"),
        ContainerState::Running
    );
}

#[test]
fn inspect_parser_accepts_legacy_flat_state() {
    let json = br#"[{"configuration":{"id":"silo-test"},"status":"stopped"}]"#;
    assert_eq!(
        parse_container_state(json, "silo-test").expect("state parses"),
        ContainerState::Stopped
    );
}

#[test]
fn inspect_parser_maps_transient_and_unknown_states() {
    let stopping = br#"[{"id":"silo-test","status":{"state":"stopping"}}]"#;
    let future = br#"[{"id":"silo-test","status":{"state":"paused"}}]"#;
    assert_eq!(
        parse_container_state(stopping, "silo-test").expect("state parses"),
        ContainerState::Stopping
    );
    assert_eq!(
        parse_container_state(future, "silo-test").expect("state parses"),
        ContainerState::Unknown
    );
}

#[test]
fn inspect_parser_rejects_missing_container_and_state() {
    let wrong = br#"[{"id":"other","status":{"state":"running"}}]"#;
    let missing = br#"[{"id":"silo-test","status":{}}]"#;
    assert!(parse_container_state(wrong, "silo-test").is_err());
    assert!(parse_container_state(missing, "silo-test").is_err());
}

#[test]
fn shared_marker_round_trips_project_and_creator() {
    let dir = TestDir::new("shared-marker");
    let project = test_project("/tmp/project");
    let marker = ContainerMarker::new(&project);
    write_marker(dir.path(), &marker).expect("marker writes");
    assert_eq!(
        read_marker(dir.path(), &project.id).expect("marker reads"),
        Some(marker)
    );
    remove_shared_state(dir.path(), &project.id);
    assert!(!marker_path(dir.path(), &project.id).exists());
}

#[test]
fn shared_marker_atomically_replaces_invalid_contents() {
    let dir = TestDir::new("shared-marker-replace");
    let project = test_project("/tmp/project");
    let marker = ContainerMarker::new(&project);
    fs::write(marker_path(dir.path(), &project.id), b"{partial")
        .expect("partial old marker writes");

    write_marker(dir.path(), &marker).expect("marker replacement succeeds");

    assert_eq!(
        read_marker(dir.path(), &project.id).expect("replacement reads"),
        Some(marker)
    );
    assert!(!marker_temp_path(dir.path(), &project.id).exists());
}

#[test]
fn failed_shared_marker_replacement_preserves_live_marker() {
    let dir = TestDir::new("shared-marker-preserve");
    let project = test_project("/tmp/project");
    let original = ContainerMarker::new(&project);
    write_marker(dir.path(), &original).expect("original marker writes");
    fs::create_dir(marker_temp_path(dir.path(), &project.id))
        .expect("directory blocks temporary marker creation");
    let mut replacement = original.clone();
    replacement.creator_pid += 1;

    write_marker(dir.path(), &replacement).expect_err("replacement cannot be staged");

    assert_eq!(
        read_marker(dir.path(), &project.id).expect("live marker remains readable"),
        Some(original)
    );
}

#[test]
fn shared_state_cleanup_removes_interrupted_marker_temporary() {
    let dir = TestDir::new("shared-marker-temp-cleanup");
    let project = test_project("/tmp/project");
    fs::write(marker_temp_path(dir.path(), &project.id), b"{partial")
        .expect("temporary marker writes");

    remove_shared_state(dir.path(), &project.id);

    assert!(!marker_temp_path(dir.path(), &project.id).exists());
}

#[test]
fn project_lock_excludes_a_second_ensure() {
    let dir = TestDir::new("project-lock");
    let path = dir.path().join("project.lock");
    let first = ProjectLock::acquire(&path, false)
        .expect("first lock succeeds")
        .expect("blocking lock is returned");
    assert!(
        ProjectLock::acquire(&path, true)
            .expect("nonblocking attempt succeeds")
            .is_none(),
        "second ensure must observe the busy lock"
    );
    drop(first);
    assert!(
        ProjectLock::acquire(&path, true)
            .expect("lock succeeds after release")
            .is_some()
    );
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

fn dead_owner_marker(dir: &Path, name: &str) -> ContainerMarker {
    let mut child = Command::new("true").spawn().expect("spawn succeeds");
    let creator_pid = child.id();
    child.wait().expect("wait succeeds");
    let project = test_project(&format!("/tmp/{name}"));
    let mut marker = ContainerMarker::new(&project);
    marker.creator_pid = creator_pid;
    write_marker(dir, &marker).expect("marker writes");
    marker
}

#[test]
fn shared_sweep_never_deletes_a_running_container() {
    let dir = TestDir::new("shared-sweep-running");
    let marker = dead_owner_marker(dir.path(), "running-project");
    let deleted = Cell::new(false);
    sweep_shared_in(
        dir.path(),
        None,
        |_| Ok(ContainerState::Running),
        |_| {
            deleted.set(true);
            Ok(())
        },
    );
    assert!(!deleted.get());
    assert!(marker_path(dir.path(), &marker.container_id).exists());
}

#[test]
fn shared_sweep_deletes_only_stopped_dead_owner_containers() {
    let dir = TestDir::new("shared-sweep-stopped");
    let marker = dead_owner_marker(dir.path(), "stopped-project");
    let mut deleted = Vec::new();
    sweep_shared_in(
        dir.path(),
        None,
        |_| Ok(ContainerState::Stopped),
        |id| {
            deleted.push(id.to_string());
            Ok(())
        },
    );
    assert_eq!(
        deleted.as_slice(),
        std::slice::from_ref(&marker.container_id)
    );
    assert!(!marker_path(dir.path(), &marker.container_id).exists());
}

#[test]
fn shared_sweep_clears_absent_markers_without_deleting() {
    let dir = TestDir::new("shared-sweep-absent");
    let marker = dead_owner_marker(dir.path(), "absent-project");
    sweep_shared_in(
        dir.path(),
        None,
        |_| Ok(ContainerState::Absent),
        |_| panic!("absent containers need no delete"),
    );
    assert!(!marker_path(dir.path(), &marker.container_id).exists());
}

#[test]
fn shared_sweep_keeps_live_owner_and_current_project_markers() {
    let dir = TestDir::new("shared-sweep-protected");
    let live_project = test_project("/tmp/live-project");
    write_marker(dir.path(), &ContainerMarker::new(&live_project)).expect("live marker writes");
    let dead = dead_owner_marker(dir.path(), "current-project");
    sweep_shared_in(
        dir.path(),
        Some(&dead.container_id),
        |_| panic!("protected markers are not inspected"),
        |_| panic!("protected markers are not deleted"),
    );
    assert!(marker_path(dir.path(), &live_project.id).exists());
    assert!(marker_path(dir.path(), &dead.container_id).exists());
}

#[test]
fn shared_sweep_retains_marker_when_delete_fails() {
    let dir = TestDir::new("shared-sweep-delete-failure");
    let marker = dead_owner_marker(dir.path(), "failed-project");
    sweep_shared_in(
        dir.path(),
        None,
        |_| Ok(ContainerState::Stopped),
        |_| Err(anyhow!("delete failed")),
    );
    assert!(marker_path(dir.path(), &marker.container_id).exists());
}

#[test]
fn legacy_sweep_removes_markers_of_dead_owners_only() {
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
    sweep_legacy_in(dir.path(), |id| {
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
fn legacy_sweep_keeps_markers_when_delete_fails() {
    let dir = TestDir::new("sweep-fail");
    let mut child = Command::new("true").spawn().expect("spawn succeeds");
    let dead_pid = child.id();
    child.wait().expect("wait succeeds");
    let dead = format!("silo-{dead_pid}");
    register_container_in(dir.path(), &dead).expect("register succeeds");

    sweep_legacy_in(dir.path(), |_| Err(anyhow!("delete failed")));

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
