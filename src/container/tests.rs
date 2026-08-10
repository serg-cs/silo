use super::*;
use crate::config::{Permission, Shared};
use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

fn args_without_labels(command: &Command) -> Vec<&str> {
    let mut arguments = command
        .get_args()
        .map(|argument| argument.to_str().expect("argument is UTF-8"));
    let mut filtered = Vec::new();
    while let Some(argument) = arguments.next() {
        if argument == "--label" {
            arguments.next().expect("label has a value");
        } else {
            filtered.push(argument);
        }
    }
    filtered
}

fn command_labels(command: &Command) -> HashMap<&str, &str> {
    let arguments: Vec<&str> = command
        .get_args()
        .map(|argument| argument.to_str().expect("argument is UTF-8"))
        .collect();
    arguments
        .windows(2)
        .filter(|pair| pair[0] == "--label")
        .map(|pair| pair[1].split_once('=').expect("label has key and value"))
        .collect()
}

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
    let args = args_without_labels(&command);
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
    let args = args_without_labels(&command);
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
    assert_eq!(
        command_labels(&command).get(LABEL_LIFECYCLE),
        Some(&LABEL_ISOLATED_VALUE)
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
    let args = args_without_labels(&command);
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
    let args = args_without_labels(&command);
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

#[test]
fn project_prefers_the_nearest_silo_marker() {
    let dir = TestDir::new("project-nearest-silo");
    let outer = dir.path().join("outer");
    let inner = outer.join("inner");
    let cwd = inner.join("src");
    fs::create_dir_all(&cwd).expect("nested project creates");
    fs::write(outer.join(PROJECT_MARKER), "").expect("outer marker creates");
    fs::write(inner.join(PROJECT_MARKER), "").expect("inner marker creates");

    let project = Project::from_path(&cwd).expect("project resolves");

    let expected_root = canonical(&inner);
    assert_eq!(project.root, expected_root);
    assert_eq!(project.workdir, PathBuf::from("/home/silo/inner"));
    assert_eq!(project.id, project_container_id(&expected_root));
}

#[test]
fn project_silo_marker_outranks_a_closer_git_directory() {
    let dir = TestDir::new("project-silo-before-git");
    let silo_root = dir.path().join("workspace");
    let git_root = silo_root.join("package");
    let cwd = git_root.join("src");
    fs::create_dir_all(cwd.as_path()).expect("nested project creates");
    fs::write(silo_root.join(PROJECT_MARKER), "").expect("silo marker creates");
    fs::create_dir(git_root.join(GIT_DIR)).expect("git directory creates");

    let project = Project::from_path(&cwd).expect("project resolves");

    assert_eq!(project.root, canonical(&silo_root));
    assert_eq!(project.workdir, PathBuf::from("/home/silo/workspace"));
}

#[test]
fn project_uses_the_nearest_git_directory_without_a_silo_marker() {
    let dir = TestDir::new("project-nearest-git");
    let outer = dir.path().join("outer");
    let inner = outer.join("inner");
    let cwd = inner.join("src");
    fs::create_dir_all(&cwd).expect("nested project creates");
    fs::create_dir(outer.join(GIT_DIR)).expect("outer git directory creates");
    fs::create_dir(inner.join(GIT_DIR)).expect("inner git directory creates");

    let project = Project::from_path(&cwd).expect("project resolves");

    assert_eq!(project.root, canonical(&inner));
    assert_eq!(project.workdir, PathBuf::from("/home/silo/inner"));
}

#[test]
fn project_without_markers_uses_the_exact_directory() {
    // Use a synthetic absolute path so markers in the test runner's own
    // temporary-directory ancestors cannot affect this pure fallback check.
    let cwd = Path::new("/silo-test-unmarked-project/src");

    assert_eq!(discover_project_root(cwd), cwd);
}

#[test]
fn project_ignores_markers_with_the_wrong_entry_type() {
    let dir = TestDir::new("project-wrong-marker-types");
    let candidate = dir.path().join("candidate");
    let cwd = candidate.join("src");
    fs::create_dir_all(&cwd).expect("working directory creates");
    fs::write(dir.path().join(PROJECT_MARKER), "").expect("outer marker creates");
    fs::create_dir(candidate.join(PROJECT_MARKER)).expect("marker directory creates");
    fs::write(candidate.join(GIT_DIR), "gitdir: elsewhere").expect("git file creates");

    let project = Project::from_path(&cwd).expect("project resolves");

    assert_eq!(project.root, canonical(dir.path()));
}

#[test]
fn discovered_project_root_drives_shared_and_isolated_mounts() {
    let dir = TestDir::new("project-lifecycle-root");
    let root = dir.path().join("workspace");
    let cwd = root.join("package").join("src");
    fs::create_dir_all(&cwd).expect("nested project creates");
    fs::write(root.join(PROJECT_MARKER), "").expect("project marker creates");
    let project = Project::from_path(&cwd).expect("project resolves");
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let mounts = ConfigMounts::default();
    let expected = format!("{}:/home/silo/workspace", project.root.display());

    let isolated = isolated_run_command(false, &project.root, &ids, &mounts, "silo-123", &[])
        .expect("isolated command builds");
    let shared = create_command(&project, &ids, &mounts, Path::new("/tmp/project.cid"))
        .expect("shared command builds");

    assert_eq!(volume_specs(&isolated), std::slice::from_ref(&expected));
    assert_eq!(volume_specs(&shared), [expected]);
    assert_eq!(
        project_digest(&project.root),
        project_digest(&canonical(&root))
    );
}

#[cfg(unix)]
#[test]
fn project_identity_canonicalizes_symlinked_working_directories() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("project-symlink");
    let project_dir = dir.path().join("real-project");
    let nested = project_dir.join("src");
    let link = dir.path().join("project-link");
    fs::create_dir_all(&nested).expect("project directory creates");
    fs::write(project_dir.join(PROJECT_MARKER), "").expect("project marker creates");
    symlink(&nested, &link).expect("project symlink creates");

    let direct = Project::from_path(&nested).expect("direct project resolves");
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
fn create_command_starts_the_detached_supervisor() {
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
    let args = args_without_labels(&command);
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
            "/usr/local/bin/silo-supervisor",
        ]
    );
    let labels = command_labels(&command);
    assert_eq!(labels.get(LABEL_OWNER), Some(&LABEL_OWNER_VALUE));
    assert_eq!(labels.get(LABEL_SCHEMA), Some(&LABEL_SCHEMA_VALUE));
    assert_eq!(labels.get(LABEL_LIFECYCLE), Some(&LABEL_SHARED_VALUE));
    assert_eq!(labels.get(LABEL_PROJECT).map(|value| value.len()), Some(64));
    assert_eq!(labels.get(LABEL_PROJECT_ROOT), Some(&"/tmp/project"));
    assert_eq!(labels.get(LABEL_SPEC).map(|value| value.len()), Some(64));
}

#[test]
fn exec_command_attaches_as_silo_with_home() {
    let project = test_project("/tmp/project");
    let command = exec_command(
        true,
        &project,
        "abc123",
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
            "/usr/local/bin/silo-session",
            "abc123",
            "codex",
            "--compact",
        ]
    );
}

#[test]
fn exec_command_uses_nu_and_omits_tty_without_a_terminal() {
    let project = test_project("/tmp/project");
    let command = exec_command(false, &project, "abc123", &[]);
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(args[0..2], ["exec", "-i"]);
    assert!(!args.contains(&"-t"));
    assert_eq!(args.last(), Some(&DEFAULT_SESSION_COMMAND));
}

#[test]
fn guest_readiness_probe_targets_the_explicit_marker() {
    let project = test_project("/tmp/project");
    let command = guest_ready_command(&project);
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("argument is UTF-8"))
        .collect();

    assert_eq!(
        args,
        [
            "exec",
            "--user",
            "silo",
            project.id.as_str(),
            "test",
            "-e",
            "/run/silo/ready",
        ]
    );
}

#[test]
fn session_reservation_is_submitted_before_the_user_command() {
    let project = test_project("/tmp/project");
    let reserve = session_reserve_command(&project, "abc123");
    let reserve_args: Vec<&str> = reserve
        .get_args()
        .map(|arg| arg.to_str().expect("argument is UTF-8"))
        .collect();
    assert_eq!(
        reserve_args,
        [
            "exec",
            "--user",
            "silo",
            project.id.as_str(),
            "/usr/local/bin/silo-reserve",
            "abc123",
        ]
    );

    let session = exec_command(false, &project, "abc123", &[OsString::from("true")]);
    let session_args: Vec<&str> = session
        .get_args()
        .map(|arg| arg.to_str().expect("argument is UTF-8"))
        .collect();
    let wrapper = session_args
        .iter()
        .position(|arg| *arg == SESSION_WRAPPER_COMMAND)
        .expect("session wrapper is present");
    assert_eq!(session_args[wrapper + 1..], ["abc123", "true"]);
}

#[test]
fn reservation_tokens_are_opaque_lowercase_digests() {
    let token = session_reservation_token(&test_project("/tmp/project"));
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(token, token.to_lowercase());
}

// These scripts are part of the Linux guest image and deliberately depend on
// util-linux `flock`. Running them directly is therefore a Linux integration
// test; macOS does not provide the guest's locking command.
#[cfg(target_os = "linux")]
#[test]
fn guest_reservation_keeps_pid_one_alive_during_session_handoff() {
    let dir = TestDir::new("guest-handoff");
    let runtime = dir.path().join("runtime");
    let reservations = runtime.join("reservations");
    fs::create_dir_all(&reservations).expect("runtime directories create");
    fs::create_dir(runtime.join("sessions")).expect("session directory creates");
    let supervisor_path = dir.path().join("supervisor");
    let reserve_path = dir.path().join("reserve");
    let session_path = dir.path().join("session");
    fs::write(&supervisor_path, SUPERVISOR).expect("supervisor writes");
    fs::write(&reserve_path, SESSION_RESERVER).expect("reserver writes");
    fs::write(&session_path, SESSION_WRAPPER).expect("wrapper writes");

    let mut supervisor = Command::new("sh")
        .arg(&supervisor_path)
        .env("SILO_RUNTIME_DIR", &runtime)
        .spawn()
        .expect("supervisor starts");
    run_guest_script(&reserve_path, &runtime, &["aa11"]).expect("first session reserves");
    let mut first = Command::new("sh")
        .arg(&session_path)
        .args(["aa11", "sh", "-c", "sleep 0.5"])
        .env("SILO_RUNTIME_DIR", &runtime)
        .spawn()
        .expect("first session starts");
    wait_for_path(&runtime.join("armed"));

    run_guest_script(&reserve_path, &runtime, &["bb22"]).expect("handoff reserves");
    assert!(first.wait().expect("first session waits").success());
    thread::sleep(Duration::from_millis(200));
    assert!(
        supervisor.try_wait().expect("supervisor polls").is_none(),
        "the pending reservation must prevent PID 1 from exiting"
    );

    run_guest_script(&session_path, &runtime, &["bb22", "true"]).expect("second session runs");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if supervisor.try_wait().expect("supervisor polls").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor did not stop after idle"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn run_guest_script(path: &Path, runtime: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("sh")
        .arg(path)
        .args(arguments)
        .env("SILO_RUNTIME_DIR", runtime)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "guest script failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(target_os = "linux")]
fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} was not created",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn inspect_parser_accepts_current_nested_state() {
    let json = br#"[{
        "id":"silo-test",
        "configuration":{"image":{"reference":"silo:latest"}},
        "status":{"state":"running","networks":[]}
    }]"#;
    let inspection = parse_container_inspection(json, "silo-test").expect("state parses");
    assert_eq!(inspection.state, ContainerState::Running);
    assert_eq!(inspection.image.as_deref(), Some("silo:latest"));
}

#[test]
fn inspect_parser_accepts_legacy_flat_state() {
    let json = br#"[{"configuration":{"id":"silo-test"},"status":"stopped"}]"#;
    assert_eq!(
        parse_container_inspection(json, "silo-test")
            .expect("state parses")
            .state,
        ContainerState::Stopped,
    );
}

#[test]
fn inspect_parser_maps_transient_and_unknown_states() {
    let stopping = br#"[{"id":"silo-test","status":{"state":"stopping"}}]"#;
    let future = br#"[{"id":"silo-test","status":{"state":"paused"}}]"#;
    assert_eq!(
        parse_container_inspection(stopping, "silo-test")
            .expect("state parses")
            .state,
        ContainerState::Stopping,
    );
    assert_eq!(
        parse_container_inspection(future, "silo-test")
            .expect("state parses")
            .state,
        ContainerState::Unknown,
    );
}

#[test]
fn inspect_parser_rejects_missing_container_and_state() {
    let wrong = br#"[{"id":"other","status":{"state":"running"}}]"#;
    let missing = br#"[{"id":"silo-test","status":{}}]"#;
    assert!(parse_container_inspection(wrong, "silo-test").is_err());
    assert!(parse_container_inspection(missing, "silo-test").is_err());
}

#[test]
fn inspect_parser_reads_runtime_labels() {
    let json = br#"[{
        "id":"silo-test",
        "configuration":{"labels":{
            "dev.silo.owner":"silo",
            "dev.silo.schema":"1",
            "dev.silo.project":"project-digest",
            "dev.silo.lifecycle":"shared",
            "dev.silo.spec":"spec-digest"
        }},
        "status":{"state":"running"}
    }]"#;
    let inspection = parse_container_inspection(json, "silo-test").expect("inspection parses");

    assert_eq!(inspection.state, ContainerState::Running);
    assert_eq!(
        inspection.labels.get(LABEL_OWNER).map(String::as_str),
        Some(LABEL_OWNER_VALUE)
    );
    assert_eq!(
        inspection.labels.get(LABEL_SPEC).map(String::as_str),
        Some("spec-digest")
    );
}

#[test]
fn inspect_parser_accepts_label_array_shape() {
    let json = br#"[{
        "configuration":{
            "id":"silo-test",
            "labels":[{"key":"dev.silo.owner","value":"silo"}]
        },
        "status":"stopped"
    }]"#;
    let inspection = parse_container_inspection(json, "silo-test").expect("inspection parses");

    assert_eq!(inspection.state, ContainerState::Stopped);
    assert_eq!(
        inspection.labels.get(LABEL_OWNER).map(String::as_str),
        Some(LABEL_OWNER_VALUE)
    );
}

fn shared_identity(project: &Project) -> ContainerIdentity {
    container_identity(
        project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    )
}

fn shared_inspection(project: &Project, identity: &ContainerIdentity) -> ContainerInspection {
    ContainerInspection {
        state: ContainerState::Running,
        labels: HashMap::from([
            (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
            (LABEL_SCHEMA.to_string(), LABEL_SCHEMA_VALUE.to_string()),
            (LABEL_PROJECT.to_string(), project_digest(&project.root)),
            (
                LABEL_PROJECT_ROOT.to_string(),
                project.root.display().to_string(),
            ),
            (LABEL_LIFECYCLE.to_string(), LABEL_SHARED_VALUE.to_string()),
            (LABEL_SPEC.to_string(), identity.spec.clone()),
        ]),
        image: Some(IMAGE_TAG.to_string()),
    }
}

#[test]
fn inspect_identity_accepts_an_exact_shared_specification() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let inspection = shared_inspection(&project, &identity);

    validate_shared_container(&inspection, &project, &identity)
        .expect("matching inspect labels are accepted");
}

#[test]
fn inspect_identity_refuses_foreign_containers_including_buildkit() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let foreign = ContainerInspection {
        state: ContainerState::Running,
        labels: HashMap::new(),
        image: None,
    };

    let error = validate_shared_container(&foreign, &project, &identity)
        .expect_err("an unlabeled runtime container must never be adopted");

    assert!(error.to_string().contains(LABEL_OWNER));
}

#[test]
fn inspect_identity_requires_explicit_stop_for_spec_drift() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &identity);
    inspection
        .labels
        .insert(LABEL_SPEC.to_string(), "different".to_string());

    let error = validate_shared_container(&inspection, &project, &identity)
        .expect_err("specification drift must be refused");

    assert!(error.to_string().contains("silo containers delete"));
}

#[test]
fn isolated_orphan_discovery_reads_runtime_ids_without_matching_buildkit() {
    let json = br#"[
        {"id":"silo-123","status":{"state":"running"}},
        {"configuration":{"id":"silo-456"},"status":{"state":"stopped"}},
        {"id":"buildkit","status":{"state":"running"}}
    ]"#;
    let ids = parse_container_ids(json).expect("container list parses");

    assert_eq!(ids, ["silo-123", "silo-456", "buildkit"]);
    assert_eq!(isolated_owner_pid(&ids[0]), Some(123));
    assert_eq!(isolated_owner_pid(&ids[1]), Some(456));
    assert_eq!(isolated_owner_pid(&ids[2]), None);
}

#[test]
fn isolated_orphan_cleanup_requires_complete_runtime_ownership_labels() {
    let project = test_project("/tmp/project");
    let identity = container_identity(
        &project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        LABEL_ISOLATED_VALUE,
        &[],
    );
    let mut inspection = ContainerInspection {
        state: ContainerState::Running,
        labels: HashMap::from([
            (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
            (LABEL_SCHEMA.to_string(), LABEL_SCHEMA_VALUE.to_string()),
            (
                LABEL_LIFECYCLE.to_string(),
                LABEL_ISOLATED_VALUE.to_string(),
            ),
            (LABEL_PROJECT.to_string(), identity.project),
            (LABEL_PROJECT_ROOT.to_string(), identity.project_root),
            (LABEL_SPEC.to_string(), identity.spec),
        ]),
        image: Some(IMAGE_TAG.to_string()),
    };
    assert!(is_owned_isolated(&inspection));

    inspection.labels.remove(LABEL_SPEC);
    assert!(!is_owned_isolated(&inspection));
    inspection.labels.clear();
    assert!(
        !is_owned_isolated(&inspection),
        "BuildKit must remain foreign"
    );
}

#[test]
fn owner_liveness_distinguishes_current_and_exited_processes() {
    let current = libc::pid_t::try_from(std::process::id()).expect("PID fits");
    assert!(owner_alive(current));

    let mut child = Command::new("true").spawn().expect("child starts");
    let child_pid = libc::pid_t::try_from(child.id()).expect("PID fits");
    child.wait().expect("child waits");
    assert!(!owner_alive(child_pid));
}

#[test]
fn specification_digest_is_deterministic_and_creation_sensitive() {
    let project = test_project("/tmp/project");
    let base = shared_identity(&project);
    let mut mounts = ConfigMounts::default();
    mounts.shared.push(ResolvedShared {
        host: PathBuf::from("/tmp/shared"),
        dest: PathBuf::from("/home/silo/shared"),
        permission: Permission::ReadOnly,
    });
    let changed = container_identity(
        &project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &mounts,
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );

    assert_eq!(base, shared_identity(&project));
    assert_ne!(base.spec, changed.spec);
    assert_eq!(base.project.len(), 64);
}

#[test]
fn embedded_image_contains_guest_lifecycle_programs() {
    assert!(DOCKERFILE.contains("COPY silo-supervisor.sh"));
    assert!(DOCKERFILE.contains("COPY silo-session.sh"));
    assert!(DOCKERFILE.contains("COPY silo-reserve.sh"));
    assert!(DOCKERFILE.contains("COPY silo-status.sh"));
    assert!(DOCKERFILE.contains("COPY silo-stop-guard.sh"));
    assert!(DOCKERFILE.contains("util-linux"));
    assert!(SUPERVISOR.contains("flock --exclusive --nonblock"));
    assert!(SUPERVISOR.contains("-ge 100"));
    assert!(SESSION_WRAPPER.contains("flock --shared"));
    assert!(SESSION_WRAPPER.contains("reservation_file"));
    assert!(SESSION_RESERVER.contains("flock --shared"));
    assert!(STATUS_HELPER.contains("count=$((count + 1))"));
    assert!(STOP_GUARD.contains("flock --exclusive --nonblock"));
    assert!(SESSION_WRAPPER.contains("exec \"$@\""));
}

#[test]
fn inventory_metadata_requires_a_matching_project_path_digest() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &identity);

    let metadata = silo_metadata(&inspection).expect("current labels are inventory-safe");
    assert_eq!(metadata.0, ContainerLifecycle::Shared);
    assert_eq!(metadata.1, project.root);
    assert_eq!(metadata.2, identity.spec);

    inspection
        .labels
        .insert(LABEL_PROJECT_ROOT.to_string(), "/tmp/different".to_string());
    assert!(silo_metadata(&inspection).is_none());
}

fn inventory_item(id: &str, project: &str, lifecycle: ContainerLifecycle) -> ContainerInfo {
    ContainerInfo {
        id: id.to_string(),
        lifecycle,
        state: ContainerState::Stopped,
        sessions: Some(0),
        project: PathBuf::from(project),
        spec: "a".repeat(64),
        image: IMAGE_TAG.to_string(),
    }
}

#[test]
fn selectors_support_ids_prefixes_paths_and_unique_project_names() {
    let items = [
        inventory_item(
            "silo-111111111111111111111111",
            "/work/alpha",
            ContainerLifecycle::Shared,
        ),
        inventory_item(
            "silo-222222222222222222222222",
            "/work/beta",
            ContainerLifecycle::Isolated,
        ),
    ];
    assert_eq!(
        select_container(&items, "silo-1111")
            .expect("unique prefix")
            .id,
        items[0].id
    );
    assert_eq!(
        select_container(&items, "/work/beta")
            .expect("exact path")
            .id,
        items[1].id
    );
    assert_eq!(
        select_container(&items, "alpha")
            .expect("unique project name")
            .id,
        items[0].id
    );
}

#[test]
fn selectors_report_ambiguous_project_matches() {
    let items = [
        inventory_item("silo-111", "/one/project", ContainerLifecycle::Shared),
        inventory_item("silo-222", "/two/project", ContainerLifecycle::Shared),
    ];
    let error = select_container(&items, "project").expect_err("name is ambiguous");
    let message = error.to_string();
    assert!(message.contains("silo-111"), "{message}");
    assert!(message.contains("silo-222"), "{message}");
}

#[test]
fn exact_project_name_precedes_an_unrelated_id_prefix() {
    let items = [
        inventory_item(
            "silo-123abcdef",
            "/work/unrelated",
            ContainerLifecycle::Shared,
        ),
        inventory_item(
            "silo-999abcdef",
            "/work/silo-123",
            ContainerLifecycle::Shared,
        ),
    ];
    let selected = select_container(&items, "silo-123").expect("project name takes precedence");
    assert_eq!(selected.id, "silo-999abcdef");
}

#[test]
fn selected_ownership_rejects_a_replacement_with_different_labels() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let inspection = shared_inspection(&project, &identity);
    let selected = ContainerInfo {
        id: project.id.clone(),
        lifecycle: ContainerLifecycle::Shared,
        state: ContainerState::Stopped,
        sessions: Some(0),
        project: project.root.clone(),
        spec: identity.spec.clone(),
        image: IMAGE_TAG.to_string(),
    };
    validate_selected_ownership(&selected, &inspection).expect("selected identity matches");

    let mut replacement = inspection;
    replacement
        .labels
        .insert(LABEL_SPEC.to_string(), "b".repeat(64));
    let error = validate_selected_ownership(&selected, &replacement)
        .expect_err("replacement must not be managed");
    assert!(error.to_string().contains("no longer matches"));
}

#[test]
fn normal_stop_policy_refuses_active_and_unknown_sessions() {
    let mut item = inventory_item("silo-111", "/work/project", ContainerLifecycle::Shared);
    item.state = ContainerState::Running;
    item.sessions = Some(2);
    let active = require_inactive_sessions(&item.id, item.sessions)
        .expect_err("active sessions are protected");
    assert!(active.to_string().contains("2 active sessions"));
    assert!(active.to_string().contains("--force"));

    item.sessions = None;
    let unknown = require_inactive_sessions(&item.id, item.sessions)
        .expect_err("unknown sessions are protected");
    assert!(unknown.to_string().contains("unknown session state"));

    item.sessions = Some(0);
    require_inactive_sessions(&item.id, item.sessions).expect("an idle container can stop");
}

#[test]
fn container_table_shows_the_agreed_snapshot_fields() {
    let item = inventory_item(
        "silo-1234567890abcdef12345678",
        "/work/project",
        ContainerLifecycle::Shared,
    );
    let table = render_container_table(&[item]);
    for expected in [
        "CONTAINER",
        "TYPE",
        "STATE",
        "SESSIONS",
        "PROJECT",
        "IMAGE",
        "silo-1234567890a",
        "silo:latest",
    ] {
        assert!(table.contains(expected), "missing {expected} in:\n{table}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn guest_status_counts_live_leases_and_stop_guard_refuses_them() {
    let dir = TestDir::new("guest-status");
    let runtime = dir.path().join("runtime");
    fs::create_dir_all(runtime.join("reservations")).expect("reservations create");
    fs::create_dir(runtime.join("sessions")).expect("sessions create");
    let reserve = dir.path().join("reserve");
    let session = dir.path().join("session");
    let status = dir.path().join("status");
    let guard = dir.path().join("guard");
    fs::write(&reserve, SESSION_RESERVER).expect("reserve writes");
    fs::write(&session, SESSION_WRAPPER).expect("session writes");
    fs::write(&status, STATUS_HELPER).expect("status writes");
    fs::write(&guard, STOP_GUARD).expect("guard writes");

    run_guest_script(&reserve, &runtime, &["aa11"]).expect("session reserves");
    let mut active = Command::new("sh")
        .arg(&session)
        .args(["aa11", "sleep", "0.4"])
        .env("SILO_RUNTIME_DIR", &runtime)
        .spawn()
        .expect("session starts");
    wait_for_path(&runtime.join("sessions/aa11"));

    let count = Command::new("sh")
        .arg(&status)
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("status runs");
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
    let blocked = Command::new("sh")
        .arg(&guard)
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("guard runs");
    assert_eq!(blocked.status.code(), Some(75));

    assert!(active.wait().expect("session waits").success());
    let ready = Command::new("sh")
        .arg(&guard)
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("guard runs after idle");
    assert!(ready.status.success());
    assert_eq!(String::from_utf8_lossy(&ready.stdout).trim(), "ready");
    let count = Command::new("sh")
        .arg(&status)
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("status prunes stale marker");
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "0");
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
