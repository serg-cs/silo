use super::*;
use crate::config::{Mount, MountKind, Permission, Shell};
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

const TEST_IMAGE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn resolved_host(name: &str, host: &str, dest: &str, access: Permission) -> ResolvedMount {
    ResolvedMount {
        name: name.to_string(),
        source: ResolvedMountSource::Host(PathBuf::from(host)),
        dest: PathBuf::from(dest),
        access,
    }
}

fn read_only_path(host: &str, relative: &str) -> ReadOnlyProjectPath {
    ReadOnlyProjectPath {
        host: PathBuf::from(host),
        relative: PathBuf::from(relative),
    }
}

fn test_managed_mount(scope: StateScope, name: &str, project: Option<&Path>) -> ManagedMount {
    managed_mount_at_root(
        scope,
        name,
        project,
        Path::new("/home/user/.local/state/silo/state"),
    )
}

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
fn forwarded_shared_preflight_preserves_image_fallback() {
    let image_checked = Cell::new(false);
    let system_checked = Cell::new(false);

    runtime_preflight_with(
        false,
        true,
        || {
            image_checked.set(true);
            Err(anyhow!("image inspection is unavailable"))
        },
        || {
            system_checked.set(true);
            Ok(())
        },
    )
    .expect("shared forwarding only requires the container system");

    assert!(!image_checked.get());
    assert!(system_checked.get());
}

#[test]
fn configured_shell_precedes_the_host_shell() {
    assert_eq!(
        resolve_shell(Some(Shell::Fish), Some(OsStr::new("/bin/bash"))),
        Shell::Fish
    );
}

#[test]
fn supported_host_shells_resolve_by_executable_name() {
    for (path, expected) in [
        ("/bin/bash", Shell::Bash),
        ("/home/user/bin/zsh", Shell::Zsh),
        ("fish", Shell::Fish),
        ("/opt/homebrew/bin/nu", Shell::Nu),
    ] {
        assert_eq!(resolve_shell(None, Some(OsStr::new(path))), expected);
    }
}

#[test]
fn missing_or_unsupported_host_shell_falls_back_to_zsh() {
    assert_eq!(resolve_shell(None, None), Shell::Zsh);
    assert_eq!(
        resolve_shell(None, Some(OsStr::new("/bin/tcsh"))),
        Shell::Zsh
    );
    assert_eq!(resolve_shell(None, Some(OsStr::new(""))), Shell::Zsh);
}

#[cfg(unix)]
#[test]
fn non_utf8_host_shell_falls_back_to_zsh() {
    use std::os::unix::ffi::OsStrExt;

    assert_eq!(
        resolve_shell(None, Some(OsStr::from_bytes(b"/bin/bad\xFFshell"))),
        Shell::Zsh
    );
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
            "--no-cache",
            "/tmp/context"
        ]
    );
}

#[test]
fn build_maintenance_commands_target_global_apple_storage() {
    assert_eq!(
        args_without_labels(&builder_delete_command()),
        ["builder", "delete", "--force"]
    );
    assert_eq!(
        args_without_labels(&image_prune_command()),
        ["image", "prune"]
    );
}

#[test]
fn build_lifecycle_deletes_before_and_cleans_after_success() {
    let events = RefCell::new(Vec::new());
    let result = run_build_lifecycle(
        || {
            events.borrow_mut().push("delete-before");
            Ok(())
        },
        || {
            events.borrow_mut().push("build");
            Ok(ExitCode::SUCCESS)
        },
        || {
            events.borrow_mut().push("cleanup-after");
            Ok(())
        },
    )
    .expect("the lifecycle succeeds");

    assert_eq!(result, ExitCode::SUCCESS);
    assert_eq!(
        events.into_inner(),
        ["delete-before", "build", "cleanup-after"]
    );
}

#[test]
fn build_lifecycle_aborts_when_stale_builder_cannot_be_deleted() {
    let built = Cell::new(false);
    let cleaned = Cell::new(false);
    let error = run_build_lifecycle(
        || Err(anyhow!("builder is busy")),
        || {
            built.set(true);
            Ok(ExitCode::SUCCESS)
        },
        || {
            cleaned.set(true);
            Ok(())
        },
    )
    .expect_err("pre-build deletion is required");

    assert!(error.to_string().contains("builder is busy"));
    assert!(!built.get());
    assert!(!cleaned.get());
}

#[test]
fn failed_build_keeps_its_status_when_cleanup_also_fails() {
    let result = run_build_lifecycle(
        || Ok(()),
        || Ok(ExitCode::from(7)),
        || Err(anyhow!("prune failed")),
    )
    .expect("the build status is preserved");

    assert_eq!(result, ExitCode::from(7));
}

#[test]
fn successful_build_reports_cleanup_failure() {
    let error = run_build_lifecycle(
        || Ok(()),
        || Ok(ExitCode::SUCCESS),
        || Err(anyhow!("prune failed")),
    )
    .expect_err("cleanup is part of successful completion");

    assert!(error.to_string().contains("prune failed"));
}

#[test]
fn build_cleanup_attempts_builder_and_image_reclamation() {
    let pruned = Cell::new(false);
    let error = cleanup_build_storage_with(
        || Err(anyhow!("delete failed")),
        || {
            pruned.set(true);
            Err(anyhow!("prune failed"))
        },
    )
    .expect_err("both maintenance failures are reported");

    assert!(pruned.get());
    assert!(error.to_string().contains("delete failed"));
    assert!(error.to_string().contains("prune failed"));
}

#[test]
fn run_command_starts_interactive_shell() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_create_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        Some(Shell::Zsh),
    )
    .expect("command builds");
    let args = args_without_labels(&command);
    assert_eq!(
        args,
        [
            "create",
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
            "--env",
            "SHELL=/home/linuxbrew/.linuxbrew/bin/zsh",
            "silo:latest",
            "/home/linuxbrew/.linuxbrew/bin/zsh",
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
    let command = isolated_create_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        Some(Shell::Zsh),
    )
    .expect("command builds");
    let args = args_without_labels(&command);
    assert_eq!(
        args,
        [
            "create",
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
            "--env",
            "SHELL=/home/linuxbrew/.linuxbrew/bin/zsh",
            "silo:latest",
            "/home/linuxbrew/.linuxbrew/bin/zsh",
        ]
    );
}

#[test]
fn run_command_applies_configured_resources() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let resources = Container {
        cpus: Some(8),
        memory: Some("32G".to_string()),
        sudo: false,
    };
    let command = isolated_create_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &resources,
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");
    let args = args_without_labels(&command);

    assert_eq!(&args[5..9], ["--cpus", "8", "--memory", "32G"],);
}

#[test]
fn run_command_grants_only_configured_sudo_access() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let enabled = isolated_create_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &Container {
            sudo: true,
            ..Container::default()
        },
        "silo-123",
        &[],
        Some(Shell::Zsh),
    )
    .expect("sudo command builds");
    let disabled = isolated_create_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        Some(Shell::Zsh),
    )
    .expect("default command builds");

    assert!(args_without_labels(&enabled).contains(&"SILO_SUDO=1"));
    assert!(!args_without_labels(&disabled).contains(&"SILO_SUDO=1"));
}

#[test]
fn custom_images_ignore_configured_sudo_access() {
    let configured = Container {
        sudo: true,
        ..Container::default()
    };

    assert!(effective_container_settings(&configured, false).sudo);
    assert!(!effective_container_settings(&configured, true).sudo);
}

#[test]
fn custom_image_run_keeps_its_default_command_and_environment() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_create_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");
    let args = args_without_labels(&command);

    assert_eq!(args.last(), Some(&IMAGE_TAG));
    assert!(!args.iter().any(|argument| argument.starts_with("SHELL=")));
}

#[test]
fn run_command_places_shared_dir_in_container_home() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_create_command(
        true,
        Path::new("/home/user/src/silo"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args[args.iter().position(|arg| *arg == "-v").expect("bind flag") + 1],
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
    let err = isolated_create_command(
        true,
        Path::new("/"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        None,
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
    let err = isolated_create_command(
        true,
        Path::new("/tmp/foo:bar"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect_err("colon is a separator");
    assert!(err.to_string().contains("cannot share"));
    assert!(err.to_string().contains("without `:`"));
}

#[test]
fn mount_argument_rejects_equals_and_commas() {
    for path in ["/tmp/with=equals", "/tmp/with,comma"] {
        let message = mount_argument_path(Path::new(path))
            .expect_err("ambiguous mount syntax is rejected")
            .to_string();
        assert!(message.contains("without `,` or `=`"), "{message}");
    }
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
    let err = isolated_create_command(
        true,
        path,
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
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
fn image_digest_parser_accepts_current_inspect_schema() {
    let output =
        format!(r#"[{{"configuration":{{"descriptor":{{"digest":"{TEST_IMAGE_DIGEST}"}}}}}}]"#);

    assert_eq!(
        parse_image_digest(output.as_bytes()).expect("current schema parses"),
        TEST_IMAGE_DIGEST
    );
}

#[test]
fn image_digest_parser_rejects_missing_or_malformed_output() {
    let missing =
        parse_image_digest(br#"[{"configuration":{}}]"#).expect_err("missing digest is rejected");
    let malformed = parse_image_digest(b"not json").expect_err("malformed JSON is rejected");

    assert!(missing.to_string().contains("OCI image digest"));
    assert!(malformed.to_string().contains("as JSON"));
}

#[test]
fn system_not_started_matches_the_cli_hint() {
    let stderr = "Error: XPC connection error\n\
                  Ensure container system service has been started with \
                  `container system start`.";
    assert!(system_not_started(stderr), "{stderr}");
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
        || Ok((None, NOT_STARTED_HINT.to_string())),
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
        || Ok((None, "Error: image not found: silo:latest".to_string())),
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
        || Ok((None, "Error: image not found: silo:latest".to_string())),
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
        || Ok((None, "Error: image not found: silo:latest".to_string())),
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
        || Ok((None, "Error: image not found: silo:latest".to_string())),
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
    let (digest, _) = inspect_image_with(
        || {
            probes.set(probes.get() + 1);
            if probes.get() == 1 {
                Ok((None, NOT_STARTED_HINT.to_string()))
            } else {
                Ok((Some(TEST_IMAGE_DIGEST.to_string()), String::new()))
            }
        },
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("probe runs");
    assert_eq!(digest.as_deref(), Some(TEST_IMAGE_DIGEST));
    assert_eq!(probes.get(), 2);
    assert_eq!(boots.get(), 1);
}

#[test]
fn inspect_image_reports_the_original_stderr_when_the_boot_fails() {
    let boots = Cell::new(0);
    let (digest, stderr) = inspect_image_with(
        || Ok((None, NOT_STARTED_HINT.to_string())),
        || {
            boots.set(boots.get() + 1);
            false
        },
    )
    .expect("probe runs");
    assert!(digest.is_none());
    assert_eq!(stderr, NOT_STARTED_HINT);
    assert_eq!(boots.get(), 1);
}

#[test]
fn inspect_image_does_not_boot_when_the_system_is_up() {
    let boots = Cell::new(0);
    let (digest, stderr) = inspect_image_with(
        || Ok((None, "Error: image not found: silo:latest".to_string())),
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("probe runs");
    assert!(digest.is_none());
    assert_eq!(stderr, "Error: image not found: silo:latest");
    assert_eq!(boots.get(), 0);
}

#[test]
fn system_preflight_allows_a_missing_image_tag() {
    let boots = Cell::new(0);

    ensure_container_system_with(
        || Ok("Error: image not found: silo:latest".to_string()),
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("a running system does not require the image tag");

    assert_eq!(boots.get(), 0);
}

#[test]
fn system_preflight_reports_a_failed_boot() {
    let error = ensure_container_system_with(|| Ok(NOT_STARTED_HINT.to_string()), || false)
        .expect_err("an unavailable system must start");

    assert!(
        error
            .to_string()
            .contains("could not start Apple container system")
    );
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
fn image_config_validation_checks_only_configured_dockerfiles() {
    validate_image_config(&Config::default()).expect("built-in image config is valid");

    let mut config = Config::default();
    config.image.dockerfile = Some(PathBuf::new());
    let message = validate_image_config(&config)
        .expect_err("configured empty Dockerfile is invalid")
        .to_string();
    assert!(message.contains("empty"), "{message}");
}

#[test]
fn effective_config_validation_rejects_custom_image_home_mounts() {
    let dir = TestDir::new("validate-custom-image-mounts");
    let dockerfile = dir.path().join("Dockerfile");
    fs::write(&dockerfile, "FROM scratch").expect("Dockerfile creation succeeds");
    let mut config = Config::default();
    config.image.dockerfile = Some(dockerfile);
    config.mounts.insert(
        "docs".into(),
        Mount {
            kind: Some(MountKind::Host),
            source: Some(dir.path().to_path_buf()),
            target: Some(PathBuf::from("~/docs")),
            ..Mount::default()
        },
    );

    let message = validate_effective_config(&config)
        .expect_err("custom image has no defined home for an enabled bind")
        .to_string();
    assert!(message.contains("bind `docs`"), "{message}");
    assert!(
        message.contains("custom images have no Silo-defined home"),
        "{message}"
    );

    config.mounts.get_mut("docs").expect("mount exists").enabled = Some(false);
    validate_effective_config(&config).expect("disabled home-relative mount is ignored");

    {
        let mount = config.mounts.get_mut("docs").expect("mount exists");
        mount.enabled = Some(true);
        mount.target = Some(PathBuf::from("./docs"));
    }
    validate_effective_config(&config).expect("project-relative host target is supported");

    let mount = config.mounts.get_mut("docs").expect("mount exists");
    mount.kind = Some(MountKind::UserState);
    mount.target = Some(PathBuf::from("~/.cache"));
    validate_effective_config(&config).expect("managed state is skipped for custom images");
}

#[test]
fn validate_dockerfile_rejects_missing_paths() {
    let path = std::env::temp_dir().join(format!("silo-test-missing-{}", std::process::id()));
    let _ = fs::remove_file(&path);
    let err = validate_dockerfile(&path).expect_err("missing path is invalid");
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn build_image_validates_custom_dockerfile_before_runtime_access() {
    let dir = TestDir::new("missing-build-dockerfile");
    let mut config = Config::default();
    config.image.dockerfile = Some(dir.path().join("Dockerfile"));

    let err = build_image(&config).expect_err("missing Dockerfile prevents maintenance");

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
    let command = isolated_create_command(
        false,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[
            OsString::from("codex"),
            OsString::from("--model"),
            OsString::from("compact"),
        ],
        Some(Shell::Fish),
    )
    .expect("command builds");
    let args = args_without_labels(&command);
    assert_eq!(
        args,
        [
            "create",
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
            "--env",
            "SHELL=/home/linuxbrew/.linuxbrew/bin/fish",
            "silo:latest",
            "codex",
            "--model",
            "compact",
        ]
    );
}

/// Returns the `-v` bind specifications of the command, in order.
fn bind_specs(command: &Command) -> Vec<String> {
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    args.windows(2)
        .filter(|pair| pair[0] == "-v")
        .map(|pair| pair[1].to_string())
        .collect()
}

fn mount_specs(command: &Command) -> Vec<String> {
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    args.windows(2)
        .filter(|pair| pair[0] == "--mount")
        .map(|pair| pair[1].to_string())
        .collect()
}

/// Canonicalizes expected host paths because e.g. macOS `/var` resolves to
/// `/private/var` before Silo builds a mount.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("path resolves")
}

#[test]
fn run_command_mounts_project_paths_read_only() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_create_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts {
            read_only: vec![
                read_only_path("/tmp/project/.git", ".git"),
                read_only_path("/tmp/project/.jj", ".jj"),
            ],
            named: vec![],
            network: None,
            guest_forwarding: None,
        },
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");
    assert_eq!(
        bind_specs(&command),
        [
            "/tmp/project:/home/silo/project",
            "/tmp/project/.git:/home/silo/project/.git:ro",
            "/tmp/project/.jj:/home/silo/project/.jj:ro",
        ]
    );
}

#[test]
fn run_command_omits_read_only_mounts_when_absent() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let command = isolated_create_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");
    assert_eq!(bind_specs(&command), ["/tmp/project:/home/silo/project"]);
}

#[test]
fn run_command_can_mount_the_project_root_read_only() {
    let command = isolated_create_command(
        false,
        Path::new("/tmp/project"),
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts {
            read_only: vec![read_only_path("/tmp/project", ".")],
            ..ConfigMounts::default()
        },
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");

    assert_eq!(
        bind_specs(&command),
        [
            "/tmp/project:/home/silo/project",
            "/tmp/project:/home/silo/project:ro"
        ]
    );
}

#[test]
fn run_command_mounts_configured_named_mounts() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let named = vec![
        resolved_host(
            "ssh",
            "/home/user/.ssh",
            "/home/silo/.ssh",
            Permission::ReadOnly,
        ),
        resolved_host(
            "downloads",
            "/home/user/Downloads",
            "/home/silo/Downloads",
            Permission::ReadWrite,
        ),
    ];
    let command = isolated_create_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts {
            read_only: vec![],
            named,
            network: None,
            guest_forwarding: None,
        },
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");
    assert_eq!(bind_specs(&command), ["/tmp/project:/home/silo/project"]);
    assert_eq!(
        mount_specs(&command),
        [
            "type=bind,source=/home/user/.ssh,target=/home/silo/.ssh,readonly",
            "type=bind,source=/home/user/Downloads,target=/home/silo/Downloads",
        ]
    );
}

#[test]
fn run_command_orders_read_only_then_named_mounts() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let named = vec![resolved_host(
        "data",
        "/home/user/data",
        "/data",
        Permission::ReadWrite,
    )];
    let command = isolated_create_command(
        true,
        Path::new("/tmp/project"),
        &ids,
        &ConfigMounts {
            read_only: vec![read_only_path("/tmp/project/.git", ".git")],
            named,
            network: None,
            guest_forwarding: None,
        },
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("command builds");
    assert_eq!(
        bind_specs(&command),
        [
            "/tmp/project:/home/silo/project",
            "/tmp/project/.git:/home/silo/project/.git:ro",
        ]
    );
    assert_eq!(
        mount_specs(&command),
        ["type=bind,source=/home/user/data,target=/data"]
    );
}

#[test]
fn shared_create_bind_mounts_all_managed_storage() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let project = Path::new("/tmp/project");
    let writable = test_managed_mount(StateScope::Project, "cargo", Some(project));
    let readonly = test_managed_mount(StateScope::User, "codex", None);
    let mounts = ConfigMounts {
        read_only: vec![],
        named: vec![
            ResolvedMount {
                name: "cargo".into(),
                source: ResolvedMountSource::Managed(writable.clone()),
                dest: PathBuf::from("/home/silo/.cargo"),
                access: Permission::ReadWrite,
            },
            ResolvedMount {
                name: "codex".into(),
                source: ResolvedMountSource::Managed(readonly.clone()),
                dest: PathBuf::from("/home/silo/.codex"),
                access: Permission::ReadOnly,
            },
        ],
        network: None,
        guest_forwarding: None,
    };
    let command = create_command(
        &test_project("/tmp/project"),
        TEST_IMAGE_DIGEST,
        &ids,
        &mounts,
        &Container::default(),
        Path::new("/tmp/cidfile"),
    )
    .expect("command builds");

    assert_eq!(
        mount_specs(&command),
        [
            format!(
                "type=bind,source={},target=/home/silo/.cargo",
                writable.path.display()
            ),
            format!(
                "type=bind,source={},target=/home/silo/.codex,readonly",
                readonly.path.display()
            ),
        ]
    );
    assert!(
        !command
            .get_args()
            .any(|arg| arg.to_string_lossy().starts_with("SILO_STATE_TARGETS="))
    );
}

#[test]
fn built_in_isolated_mount_filter_keeps_all_managed_mounts() {
    let project = Path::new("/tmp/project");
    let mounts = vec![
        ResolvedMount {
            name: "cargo".into(),
            source: ResolvedMountSource::Managed(test_managed_mount(
                StateScope::Project,
                "cargo",
                Some(project),
            )),
            dest: PathBuf::from("/home/silo/.cargo"),
            access: Permission::ReadWrite,
        },
        ResolvedMount {
            name: "codex".into(),
            source: ResolvedMountSource::Managed(test_managed_mount(
                StateScope::User,
                "codex",
                None,
            )),
            dest: PathBuf::from("/home/silo/.codex"),
            access: Permission::ReadWrite,
        },
        resolved_host("docs", "/home/user/docs", "/docs", Permission::ReadOnly),
    ];

    assert_eq!(
        mounts
            .iter()
            .map(|mount| mount.name.as_str())
            .collect::<Vec<_>>(),
        ["cargo", "codex", "docs"]
    );
    assert!(needs_mount_lock(&mounts));

    let command = isolated_create_command(
        false,
        project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts {
            read_only: vec![],
            named: mounts,
            network: None,
            guest_forwarding: None,
        },
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("built-in isolated command accepts managed state");
    assert!(
        mount_specs(&command)
            .iter()
            .any(|spec| spec.contains("/state/project/")
                && spec.ends_with("target=/home/silo/.cargo"))
    );
}

#[test]
fn custom_image_mount_filter_skips_all_managed_mounts() {
    let dir = TestDir::new("custom-image-mount-filter");
    let mounts = BTreeMap::from([
        (
            "cargo".into(),
            Mount {
                kind: Some(MountKind::ProjectState),
                target: Some(PathBuf::from("~/.cargo")),
                ..Mount::default()
            },
        ),
        (
            "codex".into(),
            Mount {
                kind: Some(MountKind::UserState),
                target: Some(PathBuf::from("~/.codex")),
                ..Mount::default()
            },
        ),
        (
            "tools".into(),
            Mount {
                kind: Some(MountKind::UserState),
                target: Some(PathBuf::from("/tools")),
                ..Mount::default()
            },
        ),
        (
            "docs".into(),
            Mount {
                kind: Some(MountKind::Host),
                source: Some(dir.path().to_path_buf()),
                target: Some(PathBuf::from("/docs")),
                ..Mount::default()
            },
        ),
    ]);

    let ResolvedIsolatedMounts {
        named: mounts,
        skipped,
    } = resolve_isolated_named_mounts(&mounts, dir.path(), None, None, true)
        .expect("custom-image mounts resolve without a managed state root");
    assert_eq!(
        skipped,
        [
            ("project", "cargo".into()),
            ("user", "codex".into()),
            ("user", "tools".into()),
        ]
    );
    assert_eq!(
        mounts
            .iter()
            .map(|mount| mount.name.as_str())
            .collect::<Vec<_>>(),
        ["docs"]
    );
    assert!(!needs_mount_lock(&mounts));
}

#[test]
fn custom_image_mount_filter_rejects_home_relative_host_targets() {
    let dir = TestDir::new("custom-image-home-target");
    let mounts = BTreeMap::from([(
        "docs".into(),
        Mount {
            kind: Some(MountKind::Host),
            source: Some(dir.path().to_path_buf()),
            target: Some(PathBuf::from("~/docs")),
            ..Mount::default()
        },
    )]);

    let message = resolve_isolated_named_mounts(&mounts, dir.path(), None, None, true)
        .err()
        .expect("custom images have no defined home for a home-relative target")
        .to_string();

    assert!(message.contains("bind `docs`"), "{message}");
    assert!(
        message.contains("custom images have no Silo-defined home"),
        "{message}"
    );
    assert!(message.contains("project-relative `./...`"), "{message}");
}

#[test]
fn custom_image_mount_filter_keeps_project_relative_host_targets() {
    let dir = TestDir::new("custom-image-project-target");
    let mounts = BTreeMap::from([(
        "docs".into(),
        Mount {
            kind: Some(MountKind::Host),
            source: Some(dir.path().to_path_buf()),
            target: Some(PathBuf::from("./docs")),
            ..Mount::default()
        },
    )]);

    let resolved = resolve_isolated_named_mounts(&mounts, dir.path(), None, None, true)
        .expect("the project location is defined for custom images");

    assert_eq!(resolved.named.len(), 1);
    assert_eq!(
        resolved.named[0].dest,
        Path::new(CONTAINER_HOME)
            .join(dir.path().file_name().expect("test project has a name"))
            .join("docs")
    );
}

#[test]
fn isolated_start_attaches_input_and_output() {
    let command = isolated_start_command("silo-123");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["start", "--attach", "--interactive", "silo-123"]
    );
}

#[test]
fn mount_conflicts_reports_read_write_shared_over_git() {
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        &[read_only_path("/project/.git", ".git")],
        &[resolved_host(
            "git-state",
            "/home/user/git",
            "/home/silo/project/.git",
            Permission::ReadWrite,
        )],
    );
    assert_eq!(conflicts.len(), 1);
    let msg = &conflicts[0];
    assert!(msg.contains("read-write entry"), "{msg}");
    assert!(msg.contains("`.git`"), "{msg}");
}

#[test]
fn mount_conflicts_reports_read_write_shared_under_git() {
    // A read-write entry at `.git/objects` is a deeper, independent mount
    // and really is writable, so the `.git` protection is defeated.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        &[read_only_path("/project/.git", ".git")],
        &[resolved_host(
            "git-objects",
            "/home/user/git",
            "/home/silo/project/.git/objects",
            Permission::ReadWrite,
        )],
    );
    assert_eq!(conflicts.len(), 1);
    assert!(
        conflicts[0].contains("read-write entry"),
        "{}",
        conflicts[0]
    );
    assert!(conflicts[0].contains(".git"), "{}", conflicts[0]);
}

#[test]
fn mount_conflicts_reports_custom_protected_directories() {
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        &[read_only_path("/project/policy", "policy")],
        &[resolved_host(
            "policy",
            "/home/user/policy",
            "/home/silo/project/policy",
            Permission::ReadWrite,
        )],
    );

    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("`policy`"), "{}", conflicts[0]);
}

#[test]
fn mount_conflicts_keeps_gitignore_silent() {
    // `.gitignore` is a sibling of `.git`, not a descendant: component
    // comparison must not flag it.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        &[read_only_path("/project/.git", ".git")],
        &[resolved_host(
            "gitignore",
            "/home/user/gitignore",
            "/home/silo/project/.gitignore",
            Permission::ReadWrite,
        )],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn mount_conflicts_keeps_ancestor_shared_silent() {
    // A mount at an ancestor does not hide the child `.git` mount: the
    // read-only protection stays in place, so no warning.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        &[read_only_path("/project/.git", ".git")],
        &[resolved_host(
            "project",
            "/home/user/project",
            "/home/silo/project",
            Permission::ReadWrite,
        )],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn mount_conflicts_keeps_read_only_shared_over_git_silent() {
    // Read-only over read-only is harmless: the `.git` protection stays
    // intact either way.
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        &[read_only_path("/project/.git", ".git")],
        &[resolved_host(
            "git",
            "/home/user/git",
            "/home/silo/project/.git",
            Permission::ReadOnly,
        )],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn mount_conflicts_is_silent_without_overlaps() {
    let conflicts = mount_conflicts(
        Path::new("/home/silo/project"),
        &[read_only_path("/project/.git", ".git")],
        &[resolved_host(
            "data",
            "/home/user/data",
            "/data",
            Permission::ReadWrite,
        )],
    );
    assert!(conflicts.is_empty());
}

#[test]
fn mount_conflicts_reports_duplicate_target_and_deterministic_winner() {
    let mounts = [
        resolved_host("alpha", "/a", "/target", Permission::ReadOnly),
        resolved_host("beta", "/b", "/target", Permission::ReadWrite),
    ];
    let conflicts = mount_conflicts(Path::new("/home/silo/project"), &[], &mounts);
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("`alpha`"), "{}", conflicts[0]);
    assert!(
        conflicts[0].contains("`beta` is applied later"),
        "{}",
        conflicts[0]
    );
    assert_eq!(
        effective_mounts(&mounts)
            .map(|mount| mount.name.as_str())
            .collect::<Vec<_>>(),
        ["beta"]
    );
}

#[test]
fn read_only_paths_resolve_directories_in_order() {
    let dir = TestDir::new("read-only-paths");
    fs::create_dir_all(dir.path().join(".git")).expect("mkdir succeeds");
    fs::create_dir_all(dir.path().join("config/policy")).expect("nested mkdir succeeds");
    assert_eq!(
        resolve_read_only_paths(
            dir.path(),
            &[PathBuf::from(".git"), PathBuf::from("config/policy")]
        )
        .expect("directories resolve"),
        [
            ReadOnlyProjectPath {
                host: canonical(&dir.path().join(".git")),
                relative: PathBuf::from(".git"),
            },
            ReadOnlyProjectPath {
                host: canonical(&dir.path().join("config/policy")),
                relative: PathBuf::from("config/policy"),
            }
        ]
    );
}

#[test]
fn read_only_paths_reject_regular_files() {
    let dir = TestDir::new("read-only-file");
    fs::write(dir.path().join("AGENTS.md"), "instructions").expect("file write succeeds");

    let error = resolve_read_only_paths(dir.path(), &[PathBuf::from("AGENTS.md")])
        .expect_err("Apple container cannot bind-mount a regular file")
        .to_string();

    assert!(error.contains("`AGENTS.md`"), "{error}");
    assert!(error.contains("must be a directory"), "{error}");
    assert!(error.contains("Apple container"), "{error}");
}

#[test]
fn read_only_paths_omit_missing_entries() {
    let dir = TestDir::new("read-only-missing");
    assert!(
        resolve_read_only_paths(dir.path(), &[PathBuf::from(".git"), PathBuf::from(".jj")])
            .expect("missing paths are omitted")
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn read_only_paths_allow_internal_symlinks_and_skip_escaping_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("read-only-symlinks");
    fs::create_dir(dir.path().join("policy")).expect("directory creates");
    symlink("policy", dir.path().join("protected")).expect("internal symlink succeeds");
    let outside = std::env::temp_dir().join(format!("silo-test-outside-{}", std::process::id()));
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).expect("mkdir succeeds");
    symlink(&outside, dir.path().join(".git")).expect("symlink succeeds");
    assert_eq!(
        resolve_read_only_paths(
            dir.path(),
            &[PathBuf::from("protected"), PathBuf::from(".git")]
        )
        .expect("safe directories resolve"),
        [ReadOnlyProjectPath {
            host: canonical(&dir.path().join("policy")),
            relative: PathBuf::from("protected"),
        }],
        "an escaping symlink must not become a mount"
    );
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn resolve_named_returns_empty_without_mounts() {
    let dir = TestDir::new("mount-empty");
    let resolved = resolve_named_mounts(&BTreeMap::new(), dir.path(), Some(dir.path()), None)
        .expect("no named mounts resolve");
    assert!(resolved.is_empty());
}

#[cfg(unix)]
#[test]
fn resolve_named_expands_tilde_and_canonicalizes_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new("mount-tilde");
    let real = dir.path().join("real-agents");
    fs::create_dir_all(&real).expect("mkdir succeeds");
    symlink(&real, dir.path().join("agents")).expect("symlink succeeds");
    let mounts = BTreeMap::from([(
        "agents".to_string(),
        Mount {
            kind: Some(MountKind::Host),
            source: Some(PathBuf::from("~/agents")),
            target: Some(PathBuf::from("/home/silo/.agents")),
            ..Mount::default()
        },
    )]);
    let resolved =
        resolve_named_mounts(&mounts, dir.path(), Some(dir.path()), None).expect("mount resolves");
    assert_eq!(
        resolved,
        vec![resolved_host(
            "agents",
            canonical(&real).to_str().expect("UTF-8"),
            "/home/silo/.agents",
            Permission::ReadOnly,
        )]
    );
}

#[test]
fn resolve_named_builds_stable_host_backed_managed_mounts() {
    let mounts = BTreeMap::from([
        (
            "cargo".to_string(),
            Mount {
                kind: Some(MountKind::ProjectState),
                target: Some(PathBuf::from("~/.cargo")),
                ..Mount::default()
            },
        ),
        (
            "codex".to_string(),
            Mount {
                kind: Some(MountKind::UserState),
                target: Some(PathBuf::from("~/.codex")),
                ..Mount::default()
            },
        ),
        (
            "cargo-target".to_string(),
            Mount {
                kind: Some(MountKind::ProjectState),
                target: Some(PathBuf::from("./target")),
                ..Mount::default()
            },
        ),
    ]);
    let project = Path::new("/tmp/project");
    let home = Path::new("/home/user");
    let resolved =
        resolve_named_mounts(&mounts, project, Some(home), None).expect("state resolves");
    let cargo = resolved.iter().find(|mount| mount.name == "cargo").unwrap();
    let codex = resolved.iter().find(|mount| mount.name == "codex").unwrap();
    let target = resolved
        .iter()
        .find(|mount| mount.name == "cargo-target")
        .unwrap();
    let ResolvedMountSource::Managed(cargo_mount) = &cargo.source else {
        panic!("project state becomes a managed mount");
    };
    let ResolvedMountSource::Managed(codex_mount) = &codex.source else {
        panic!("user state becomes a managed mount");
    };
    assert_eq!(cargo_mount.scope, StateScope::Project);
    assert_eq!(cargo_mount.project.as_deref(), Some(project));
    assert_eq!(codex_mount.scope, StateScope::User);
    assert_eq!(codex_mount.project, None);
    assert_eq!(target.dest, PathBuf::from("/home/silo/project/target"));
    assert_eq!(
        cargo_mount.path,
        PathBuf::from(format!(
            "/home/user/.local/state/silo/state/project/{}/entries/cargo",
            project_digest(project)
        ))
    );
    assert_eq!(
        codex_mount.path,
        PathBuf::from("/home/user/.local/state/silo/state/user/codex")
    );
    assert_eq!(cargo.access, Permission::ReadWrite);
    assert_eq!(codex.access, Permission::ReadWrite);
    assert_eq!(
        test_managed_mount(StateScope::User, "codex", None).id,
        codex_mount.id
    );
}

#[test]
fn resolve_named_errors_on_missing_host_source() {
    let dir = TestDir::new("mount-missing");
    let mounts = BTreeMap::from([(
        "nope".to_string(),
        Mount {
            kind: Some(MountKind::Host),
            source: Some(PathBuf::from("~/nope")),
            target: Some(PathBuf::from("/home/silo/nope")),
            ..Mount::default()
        },
    )]);
    let err = resolve_named_mounts(&mounts, dir.path(), Some(dir.path()), None)
        .expect_err("missing source errors");
    let msg = err.to_string();
    assert!(msg.contains("cannot resolve source"), "{msg}");
    assert!(msg.contains("bind `nope`"), "{msg}");
}

#[test]
fn resolve_named_rejects_regular_file_host_sources() {
    let dir = TestDir::new("mount-file");
    let file = dir.path().join("settings.toml");
    fs::write(&file, "setting = true").expect("file writes");
    let mounts = BTreeMap::from([(
        "settings".to_string(),
        Mount {
            kind: Some(MountKind::Host),
            source: Some(file),
            target: Some(PathBuf::from("/home/silo/settings.toml")),
            ..Mount::default()
        },
    )]);

    let message = resolve_named_mounts(&mounts, dir.path(), Some(dir.path()), None)
        .expect_err("regular files are not supported bind sources")
        .to_string();
    assert!(message.contains("bind `settings`"), "{message}");
    assert!(message.contains("must be a directory"), "{message}");
}

#[test]
fn resolve_named_orders_parents_before_children_then_names() {
    let mounts = BTreeMap::from([
        (
            "later".to_string(),
            Mount {
                kind: Some(MountKind::UserState),
                target: Some(PathBuf::from("/same")),
                ..Mount::default()
            },
        ),
        (
            "earlier".to_string(),
            Mount {
                kind: Some(MountKind::UserState),
                target: Some(PathBuf::from("/same")),
                ..Mount::default()
            },
        ),
        (
            "child".to_string(),
            Mount {
                kind: Some(MountKind::ProjectState),
                target: Some(PathBuf::from("/same/child")),
                ..Mount::default()
            },
        ),
    ]);
    let resolved = resolve_named_mounts(
        &mounts,
        Path::new("/tmp/project"),
        Some(Path::new("/home/user")),
        None,
    )
    .expect("mounts resolve");
    assert_eq!(
        resolved
            .iter()
            .map(|mount| mount.name.as_str())
            .collect::<Vec<_>>(),
        ["earlier", "later", "child"]
    );
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
fn managed_mount_root_prefers_absolute_xdg_and_falls_back_to_home() {
    assert_eq!(
        managed_mount_root(Some(Path::new("/var/state")), Some(Path::new("/home/user")))
            .expect("absolute XDG state home works"),
        PathBuf::from("/var/state/silo/state")
    );
    assert_eq!(
        managed_mount_root(Some(Path::new("relative")), Some(Path::new("/home/user")))
            .expect("relative XDG state home is ignored"),
        PathBuf::from("/home/user/.local/state/silo/state")
    );
    assert!(managed_mount_root(None, Some(Path::new("relative"))).is_err());
    assert!(managed_mount_root(Some(Path::new("relative")), Some(Path::new("relative"))).is_err());
    assert!(managed_mount_root(None, None).is_err());
}

#[test]
fn mount_inventory_reports_an_unavailable_state_root() {
    for (xdg_state_home, home) in [
        (None, None),
        (Some(Path::new("")), Some(Path::new(""))),
        (
            Some(Path::new("relative-state")),
            Some(Path::new("relative-home")),
        ),
    ] {
        let message = mount_inventory_for_env(xdg_state_home, home)
            .err()
            .expect("an unavailable managed-state root must be reported")
            .to_string();
        assert!(
            message.contains("XDG_STATE_HOME or HOME to be absolute"),
            "{message}"
        );
    }
}

#[cfg(unix)]
#[test]
fn managed_mounts_are_created_private_and_reject_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = TestDir::new("managed-mount");
    let mount = managed_mount_at_root(StateScope::User, "codex", None, dir.path());
    ensure_managed_mount(&mount).expect("managed directory is created");
    for path in [
        dir.path().to_path_buf(),
        dir.path().join("user"),
        mount.path.clone(),
    ] {
        assert_eq!(
            fs::metadata(path)
                .expect("metadata exists")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    let target = dir.path().join("target");
    fs::create_dir(&target).expect("target exists");
    let link = managed_mount_at_root(StateScope::User, "linked", None, dir.path());
    symlink(&target, &link.path).expect("symlink exists");
    assert!(ensure_managed_mount(&link).is_err());
}

#[cfg(unix)]
#[test]
fn project_mount_metadata_round_trips_raw_paths_and_drives_inventory() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new("project-mount-metadata");
    let project = Path::new(OsStr::from_bytes(b"/work/non-utf8-\xFF"));
    let mount = managed_mount_at_root(StateScope::Project, "cargo", Some(project), dir.path());
    ensure_managed_mount(&mount).expect("project state is created");

    let project_dir = mount.path.parent().unwrap().parent().unwrap();
    let metadata = project_dir.join(PROJECT_ROOT_METADATA);
    assert_eq!(
        fs::read(&metadata).expect("metadata reads"),
        project.as_os_str().as_bytes()
    );
    for path in [
        dir.path(),
        &dir.path().join("project"),
        project_dir,
        mount.path.parent().unwrap(),
        &mount.path,
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{} is private",
            path.display()
        );
    }
    assert_eq!(
        fs::metadata(&metadata).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let inventory = mount_inventory_at(dir.path());
    assert!(inventory.warnings.is_empty(), "{:?}", inventory.warnings);
    assert_eq!(inventory.items, [MountInfo(mount)]);
}

#[test]
fn project_mount_inventory_rejects_metadata_digest_mismatches() {
    let dir = TestDir::new("project-mount-bad-metadata");
    let project = Path::new("/work/project");
    let mount = managed_mount_at_root(StateScope::Project, "cargo", Some(project), dir.path());
    ensure_managed_mount(&mount).expect("project state is created");
    let project_dir = mount.path.parent().unwrap().parent().unwrap();
    fs::write(
        project_dir.join(PROJECT_ROOT_METADATA),
        "/different/project",
    )
    .expect("metadata is corrupted");
    assert!(ensure_managed_mount(&mount).is_err());

    let inventory = mount_inventory_at(dir.path());
    assert!(inventory.items.is_empty());
    assert!(
        inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("does not match its digest")),
        "{:?}",
        inventory.warnings
    );
}

#[test]
fn final_project_state_cleanup_removes_metadata_directory() {
    let dir = TestDir::new("project-state-cleanup");
    let project = Path::new("/work/project");
    let mount = managed_mount_at_root(StateScope::Project, "cargo", Some(project), dir.path());
    ensure_managed_mount(&mount).expect("project state is created");
    let project_dir = mount.path.parent().unwrap().parent().unwrap().to_path_buf();
    fs::remove_dir_all(&mount.path).expect("mount data is deleted");
    prune_empty_project_state_directory(&mount.path).expect("empty project metadata is pruned");
    assert!(!project_dir.exists());
}

#[test]
fn managed_mount_lock_serializes_competing_operations() {
    let dir = TestDir::new("managed-mount-lock");
    let first = MountLock::acquire_at(dir.path()).expect("first lock succeeds");
    let root = dir.path().to_path_buf();
    let (sender, receiver) = std::sync::mpsc::channel();
    let contender = std::thread::spawn(move || {
        let second = MountLock::acquire_at(&root).expect("second lock succeeds");
        sender.send(()).expect("notification sends");
        drop(second);
    });

    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_err(),
        "the competing operation must wait while creation or deletion owns the lock"
    );
    drop(first);
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("the competing operation resumes after unlock");
    contender.join().expect("contender exits");
}

#[test]
fn build_lock_serializes_competing_image_builds() {
    let dir = TestDir::new("image-build-lock");
    let first = BuildLock::acquire_at(dir.path()).expect("first lock succeeds");
    let root = dir.path().to_path_buf();
    let (sender, receiver) = std::sync::mpsc::channel();
    let contender = std::thread::spawn(move || {
        let second = BuildLock::acquire_at(&root).expect("second lock succeeds");
        sender.send(()).expect("notification sends");
        drop(second);
    });

    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_err(),
        "the competing Silo build must wait for global builder ownership"
    );
    drop(first);
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("the competing build resumes after unlock");
    contender.join().expect("contender exits");
}

#[test]
fn build_lock_location_is_stable_for_the_current_user() {
    assert_eq!(
        global_build_lock_root(),
        Path::new(BUILD_LOCK_PARENT).join(format!("silo-build-{}", unsafe { libc::geteuid() }))
    );
}

#[test]
fn shared_build_lock_directory_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new("private-build-lock");
    let root = dir.path().join("lock");
    ensure_owned_private_directory(&root, "test lock root").expect("lock root is created");

    assert_eq!(
        fs::metadata(root).unwrap().permissions().mode() & 0o777,
        0o700
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
fn project_silo_marker_outranks_closer_vcs_directories() {
    let dir = TestDir::new("project-silo-before-git");
    let silo_root = dir.path().join("workspace");
    let git_root = silo_root.join("package");
    let cwd = git_root.join("src");
    fs::create_dir_all(cwd.as_path()).expect("nested project creates");
    fs::write(silo_root.join(PROJECT_MARKER), "").expect("silo marker creates");
    fs::create_dir(git_root.join(GIT_DIR)).expect("git directory creates");
    fs::create_dir(git_root.join(JJ_DIR)).expect("jj directory creates");

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
fn project_uses_the_nearest_jj_directory_without_a_silo_marker() {
    let dir = TestDir::new("project-nearest-jj");
    let workspace = dir.path().join("workspace");
    let cwd = workspace.join("src");
    fs::create_dir_all(&cwd).expect("nested project creates");
    fs::create_dir(workspace.join(JJ_DIR)).expect("jj directory creates");

    let project = Project::from_path(&cwd).expect("project resolves");

    assert_eq!(project.root, canonical(&workspace));
    assert_eq!(project.workdir, PathBuf::from("/home/silo/workspace"));
}

#[test]
fn project_uses_the_nearest_vcs_directory_regardless_of_kind() {
    let dir = TestDir::new("project-nearest-vcs");
    let git_root = dir.path().join("git-root");
    let jj_root = git_root.join("jj-root");
    let cwd = jj_root.join("src");
    fs::create_dir_all(&cwd).expect("nested project creates");
    fs::create_dir(git_root.join(GIT_DIR)).expect("git directory creates");
    fs::create_dir(jj_root.join(JJ_DIR)).expect("jj directory creates");

    assert_eq!(discover_project_root(&cwd), jj_root);
}

#[test]
fn project_without_markers_uses_the_exact_directory() {
    // Use a synthetic absolute path so markers in the test runner's own
    // temporary-directory ancestors cannot affect this pure fallback check.
    let cwd = Path::new("/silo-test-unmarked-project/src");

    assert_eq!(discover_project_root(cwd), cwd);
}

#[test]
fn config_project_root_discovery_does_not_validate_container_mounts() {
    let root = project_root_from_path(Path::new("/"))
        .expect("filesystem root is valid for config discovery");
    assert_eq!(root, Path::new("/"));

    let message = Project::from_path(Path::new("/"))
        .expect_err("filesystem root is still invalid for a container project")
        .to_string();
    assert!(
        message.contains("cannot share the root directory"),
        "{message}"
    );
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
    fs::write(candidate.join(JJ_DIR), "not a workspace").expect("jj file creates");

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

    let isolated = isolated_create_command(
        false,
        &project.root,
        &ids,
        &mounts,
        &Container::default(),
        "silo-123",
        &[],
        None,
    )
    .expect("isolated command builds");
    let shared = create_command(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &mounts,
        &Container::default(),
        Path::new("/tmp/project.cid"),
    )
    .expect("shared command builds");

    assert_eq!(bind_specs(&isolated), std::slice::from_ref(&expected));
    assert_eq!(bind_specs(&shared), [expected]);
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
        TEST_IMAGE_DIGEST,
        &ids,
        &ConfigMounts::default(),
        &Container::default(),
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
            "--rm",
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
fn shared_create_command_applies_configured_resources() {
    let project = test_project("/tmp/project");
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let resources = Container {
        cpus: Some(6),
        memory: Some("12G".to_string()),
        sudo: false,
    };
    let command = create_command(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &ConfigMounts::default(),
        &resources,
        Path::new("/tmp/project.cid"),
    )
    .expect("command builds");
    let args = args_without_labels(&command);

    assert_eq!(&args[7..11], ["--cpus", "6", "--memory", "12G"],);
}

#[test]
fn shared_create_command_applies_configured_sudo_access() {
    let project = test_project("/tmp/project");
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let resources = Container {
        sudo: true,
        ..Container::default()
    };
    let command = create_command(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &ConfigMounts::default(),
        &resources,
        Path::new("/tmp/project.cid"),
    )
    .expect("command builds");

    assert!(args_without_labels(&command).contains(&"SILO_SUDO=1"));
}

#[test]
fn forward_network_and_environment_reach_container_commands() {
    let project = test_project("/tmp/project");
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let mounts = ConfigMounts {
        network: Some("silo-net-test".to_string()),
        guest_forwarding: Some(forward::GuestForwarding::new(
            "192.168.64.1".parse().expect("gateway parses"),
            BTreeMap::from([(5432, 15432)]),
        )),
        ..ConfigMounts::default()
    };
    let environment = [("SILO_POSTGRES_PORT".to_string(), "5432".to_string())];

    let isolated = isolated_create_command_with_forward(
        false,
        &project.root,
        &ids,
        &mounts,
        &Container::default(),
        "silo-isolated",
        &[],
        Some(Shell::Zsh),
        &environment,
    )
    .expect("isolated command builds");
    let isolated_args = args_without_labels(&isolated);
    assert!(
        isolated_args
            .windows(2)
            .any(|pair| pair == ["--network", "silo-net-test"])
    );
    assert!(isolated_args.contains(&"SILO_POSTGRES_PORT=5432"));
    assert!(
        !isolated_args
            .iter()
            .any(|arg| arg.starts_with("SILO_POSTGRES_HOST="))
    );
    assert!(isolated_args.ends_with(&[
        IMAGE_TAG,
        FORWARD_WRAPPER_COMMAND,
        "192.168.64.1",
        "5432:15432",
        "--",
        ZSH_PATH,
    ]));

    let shared = create_command(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &mounts,
        &Container::default(),
        Path::new("/tmp/project.cid"),
    )
    .expect("shared command builds");
    let shared_args = args_without_labels(&shared);
    assert!(
        shared_args
            .windows(2)
            .any(|pair| pair == ["--network", "silo-net-test"])
    );
    assert!(shared_args.ends_with(&[
        IMAGE_TAG,
        FORWARD_WRAPPER_COMMAND,
        "192.168.64.1",
        "5432:15432",
        "--",
        SHARED_INIT_COMMAND,
    ]));

    let exec = exec_command_with_forward(false, &project, "abc123", &[], Shell::Zsh, &environment);
    let exec_args: Vec<_> = exec
        .get_args()
        .map(|arg| arg.to_str().expect("argument is UTF-8"))
        .collect();
    assert!(exec_args.contains(&"SILO_POSTGRES_PORT=5432"));
    assert!(
        !exec_args
            .iter()
            .any(|arg| arg.starts_with("SILO_POSTGRES_HOST="))
    );
}

#[test]
fn custom_image_forwarding_keeps_gateway_environment_without_guest_wrapper() {
    let project = test_project("/tmp/project");
    let environment = [
        ("SILO_POSTGRES_HOST".to_string(), "192.168.64.1".to_string()),
        ("SILO_POSTGRES_PORT".to_string(), "15432".to_string()),
    ];
    let command = isolated_create_command_with_forward(
        false,
        &project.root,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts {
            network: Some("silo-net-test".to_string()),
            ..ConfigMounts::default()
        },
        &Container::default(),
        "silo-isolated",
        &[],
        None,
        &environment,
    )
    .expect("custom image command builds");
    let args = args_without_labels(&command);

    assert!(args.contains(&"SILO_POSTGRES_HOST=192.168.64.1"));
    assert!(args.contains(&"SILO_POSTGRES_PORT=15432"));
    assert!(!args.contains(&FORWARD_WRAPPER_COMMAND));
}

#[test]
fn exec_command_attaches_as_silo_with_home() {
    let project = test_project("/tmp/project");
    let command = exec_command(
        true,
        &project,
        "abc123",
        &[OsString::from("codex"), OsString::from("--compact")],
        Shell::Fish,
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
            "--env",
            "SHELL=/home/linuxbrew/.linuxbrew/bin/fish",
            project.id.as_str(),
            "/usr/local/bin/silo-session",
            "abc123",
            "codex",
            "--compact",
        ]
    );
}

#[test]
fn exec_command_uses_selected_shell_and_omits_tty_without_a_terminal() {
    let project = test_project("/tmp/project");
    let command = exec_command(false, &project, "abc123", &[], Shell::Nu);
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(args[0..2], ["exec", "-i"]);
    assert!(!args.contains(&"-t"));
    assert!(args.contains(&"SHELL=/home/linuxbrew/.linuxbrew/bin/nu"));
    assert_eq!(args.last(), Some(&NU_PATH));
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
fn stop_guard_uses_the_current_embedded_script_for_existing_containers() {
    let command = stop_guard_command("silo-old");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("argument is UTF-8"))
        .collect();
    assert_eq!(
        &args[..7],
        ["exec", "--user", "silo", "silo-old", "sh", "-c", STOP_GUARD]
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

    let session = exec_command(
        false,
        &project,
        "abc123",
        &[OsString::from("true")],
        Shell::Zsh,
    );
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
        "configuration":{
            "image":{
                "reference":"silo:latest",
                "descriptor":{"digest":"sha256:aaaa"}
            },
            "resources":{"cpus":8,"memoryInBytes":12884901888}
        },
        "status":{"state":"running","networks":[]}
    }]"#;
    let inspection = parse_container_inspection(json, "silo-test").expect("state parses");
    assert_eq!(inspection.state, ContainerState::Running);
    assert_eq!(inspection.image.as_deref(), Some("silo:latest"));
    assert_eq!(inspection.image_digest.as_deref(), Some("sha256:aaaa"));
    assert_eq!(inspection.resources.cpus, Some(8));
    assert_eq!(
        inspection.resources.memory_bytes,
        Some(12 * 1024 * 1024 * 1024)
    );
}

#[test]
fn inspect_parser_allows_missing_resources() {
    let json = br#"[{
        "id":"silo-test",
        "configuration":{},
        "status":{"state":"stopped"}
    }]"#;

    let inspection = parse_container_inspection(json, "silo-test").expect("inspection parses");

    assert_eq!(inspection.resources, ContainerResources::default());
}

#[test]
fn inspect_parser_reads_bind_sources_for_safe_mount_deletion() {
    let json = br#"[{
        "id":"silo-test",
        "configuration":{"mounts":[
            {"type":"bind","source":"/home/user/.local/state/silo/state/user/codex","destination":"/home/silo/.codex"},
            {"type":"bind","source":"/home/user/.local/state/silo/state/project/abc/entries/cache","destination":"/cache"}
        ]},
        "status":{"state":"running"}
    }]"#;
    let inspection = parse_container_inspection(json, "silo-test").expect("mounts parse");
    assert_eq!(
        inspection.mount_sources,
        [
            PathBuf::from("/home/user/.local/state/silo/state/user/codex"),
            PathBuf::from("/home/user/.local/state/silo/state/project/abc/entries/cache")
        ]
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
fn mount_selectors_support_ids_names_paths_and_report_ambiguity() {
    let first = MountInfo(test_managed_mount(
        StateScope::Project,
        "cargo",
        Some(Path::new("/one/project")),
    ));
    let second = MountInfo(test_managed_mount(
        StateScope::Project,
        "cargo",
        Some(Path::new("/two/project")),
    ));
    let shared = MountInfo(test_managed_mount(StateScope::User, "codex", None));
    let items = [first, second, shared];

    assert_eq!(
        select_mount(&items, "codex").expect("logical name resolves"),
        &items[2]
    );
    assert_eq!(
        select_mount(&items, "/one/project").expect("project path resolves"),
        &items[0]
    );
    assert_eq!(
        select_mount(&items, &items[0].0.id).expect("exact ID resolves"),
        &items[0]
    );
    let message = select_mount(&items, "cargo")
        .expect_err("duplicate logical names are ambiguous")
        .to_string();
    assert!(message.contains(&items[0].0.id), "{message}");
    assert!(message.contains(&items[1].0.id), "{message}");
    assert!(message.contains("project"), "{message}");
    assert!(message.contains("cargo"), "{message}");
    assert!(
        message.contains(&display_path(Path::new("/one/project"))),
        "{message}"
    );
    assert!(
        message.contains(&display_path(Path::new("/two/project"))),
        "{message}"
    );
}

#[test]
fn mount_selector_reports_project_and_logical_name_collisions() {
    let project = MountInfo(test_managed_mount(
        StateScope::Project,
        "cargo",
        Some(Path::new("/work/codex")),
    ));
    let shared = MountInfo(test_managed_mount(StateScope::User, "codex", None));
    let items = [project, shared];

    let message = select_mount(&items, "codex")
        .expect_err("cross-category collision must be ambiguous")
        .to_string();
    assert!(message.contains(&items[0].0.id), "{message}");
    assert!(message.contains(&items[1].0.id), "{message}");
    assert_eq!(
        select_mount(&items, "/work/codex").expect("exact project path is unambiguous"),
        &items[0]
    );
}

#[test]
fn mount_table_shows_identity_scope_name_project_and_source() {
    let item = MountInfo(test_managed_mount(
        StateScope::Project,
        "cargo",
        Some(Path::new("/work/project")),
    ));
    let rendered = render_mount_table(std::slice::from_ref(&item));
    for expected in [
        "ID",
        "SCOPE",
        "NAME",
        "PROJECT",
        "SOURCE",
        "project",
        "cargo",
        "silo/state",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected}: {rendered}"
        );
    }
    assert!(rendered.contains(&item.0.id), "{rendered}");
    assert!(
        !rendered.contains(&display_path(Path::new("/work/project"))),
        "{rendered}"
    );
}

#[test]
fn mount_table_uses_paths_only_for_repeated_project_names() {
    let items = [
        MountInfo(test_managed_mount(
            StateScope::Project,
            "cargo",
            Some(Path::new("/work/alpha")),
        )),
        MountInfo(test_managed_mount(
            StateScope::Project,
            "codex",
            Some(Path::new("/work/alpha")),
        )),
        MountInfo(test_managed_mount(
            StateScope::Project,
            "cargo",
            Some(Path::new("/one/project")),
        )),
        MountInfo(test_managed_mount(
            StateScope::Project,
            "cargo",
            Some(Path::new("/two/project")),
        )),
    ];

    let rendered = render_mount_table(&items);

    assert!(rendered.contains("alpha"), "{rendered}");
    assert!(
        !rendered.contains(&display_path(Path::new("/work/alpha"))),
        "{rendered}"
    );
    assert!(
        rendered.contains(&display_path(Path::new("/one/project"))),
        "{rendered}"
    );
    assert!(
        rendered.contains(&display_path(Path::new("/two/project"))),
        "{rendered}"
    );
}

fn shared_identity(project: &Project) -> ContainerIdentity {
    container_identity(
        project,
        TEST_IMAGE_DIGEST,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
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
        image_digest: Some(TEST_IMAGE_DIGEST.to_string()),
        mount_sources: Vec::new(),
        resources: ContainerResources::default(),
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
        image_digest: None,
        mount_sources: Vec::new(),
        resources: ContainerResources::default(),
    };

    let error = validate_shared_container(&foreign, &project, &identity)
        .expect_err("an unlabeled runtime container must never be adopted");

    assert!(error.to_string().contains(LABEL_OWNER));
}

#[test]
fn inspect_identity_distinguishes_owned_spec_drift() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &identity);
    inspection
        .labels
        .insert(LABEL_SPEC.to_string(), "b".repeat(64));

    assert!(
        !shared_container_matches(&inspection, &project, &identity)
            .expect("the outdated container is still owned")
    );
}

#[test]
fn forward_network_and_guest_mapping_change_the_container_specification() {
    let project = test_project("/tmp/project");
    let without_network = shared_identity(&project);
    let with_network = container_identity(
        &project,
        TEST_IMAGE_DIGEST,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts {
            network: Some("silo-net-test".to_string()),
            ..ConfigMounts::default()
        },
        &Container::default(),
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );
    let with_guest_mapping = container_identity(
        &project,
        TEST_IMAGE_DIGEST,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts {
            network: Some("silo-net-test".to_string()),
            guest_forwarding: Some(forward::GuestForwarding::new(
                "192.168.64.1".parse().expect("gateway parses"),
                BTreeMap::from([(5432, 15432)]),
            )),
            ..ConfigMounts::default()
        },
        &Container::default(),
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );

    assert_ne!(without_network.spec, with_network.spec);
    assert_ne!(with_network.spec, with_guest_mapping.spec);
}

#[test]
fn existing_container_identity_reuses_digest_spec_when_image_lookup_fails() {
    let project = test_project("/tmp/project");
    let expected = shared_identity(&project);
    let inspection = shared_inspection(&project, &expected);

    let actual = desired_existing_container_identity_with(
        &inspection,
        &project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
        || Err(anyhow!("image inspection is unavailable")),
    )
    .expect("the inspected container remains reusable");

    assert_eq!(actual, expected);
}

#[test]
fn existing_container_identity_requires_a_recorded_digest_when_image_lookup_fails() {
    let project = test_project("/tmp/project");
    let expected = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &expected);
    inspection.image_digest = None;

    let error = desired_existing_container_identity_with(
        &inspection,
        &project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
        || Err(anyhow!("image inspection is unavailable")),
    )
    .expect_err("an identity without a recorded digest is not reusable");

    assert!(
        error
            .to_string()
            .contains("image inspection is unavailable")
    );
}

#[test]
fn existing_container_identity_requires_an_image_for_configuration_drift() {
    let project = test_project("/tmp/project");
    let mut inspection = shared_inspection(&project, &shared_identity(&project));
    inspection
        .labels
        .insert(LABEL_SPEC.to_string(), "outdated".to_string());

    let error = desired_existing_container_identity_with(
        &inspection,
        &project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
        || Err(anyhow!("image is missing")),
    )
    .expect_err("configuration drift needs a replacement image");

    assert!(error.to_string().contains("image is missing"));
}

#[test]
fn existing_container_identity_prefers_the_current_image_digest() {
    let project = test_project("/tmp/project");
    let inspection = shared_inspection(&project, &shared_identity(&project));
    let updated_digest = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    let actual = desired_existing_container_identity_with(
        &inspection,
        &project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
        || Ok(updated_digest.to_string()),
    )
    .expect("the current image is available");

    assert_ne!(actual.spec, shared_identity(&project).spec);
}

#[test]
fn spec_drift_deletes_an_idle_running_container() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &identity);
    inspection
        .labels
        .insert(LABEL_SPEC.to_string(), "b".repeat(64));
    let stopped = Cell::new(false);
    let deleted = Cell::new(false);
    let image_checked = Cell::new(false);

    let outcome = replace_outdated_shared_container_with(
        &project,
        &inspection,
        |_| Ok(0),
        || {
            image_checked.set(true);
            Ok(())
        },
        |_| {
            assert!(image_checked.get());
            stopped.set(true);
            Ok(())
        },
        |_| {
            deleted.set(true);
            Ok(())
        },
    )
    .expect("an idle outdated container is replaced");

    assert!(matches!(outcome, ReplacementOutcome::Replaced));
    assert!(stopped.get());
    assert!(deleted.get());
}

#[test]
fn spec_drift_preserves_the_container_when_the_image_is_missing() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &identity);
    inspection
        .labels
        .insert(LABEL_SPEC.to_string(), "b".repeat(64));
    let stopped = Cell::new(false);
    let deleted = Cell::new(false);

    let error = replace_outdated_shared_container_with(
        &project,
        &inspection,
        |_| Ok(0),
        || Err(anyhow!("replacement image is missing")),
        |_| {
            stopped.set(true);
            Ok(())
        },
        |_| {
            deleted.set(true);
            Ok(())
        },
    )
    .expect_err("the existing container must survive without a replacement image");

    assert!(error.to_string().contains("replacement image is missing"));
    assert!(!stopped.get());
    assert!(!deleted.get());
}

#[test]
fn spec_drift_reports_other_active_sessions_explicitly() {
    let id = "silo-4070dfe2dfb71225713e6507";
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &identity);
    inspection
        .labels
        .insert(LABEL_SPEC.to_string(), "b".repeat(64));
    let stopped = Cell::new(false);
    let deleted = Cell::new(false);
    let one = replace_outdated_shared_container_with(
        &project,
        &inspection,
        |_| Ok(1),
        || Ok(()),
        |_| {
            stopped.set(true);
            Ok(())
        },
        |_| {
            deleted.set(true);
            Ok(())
        },
    )
    .expect_err("an active outdated container is preserved")
    .to_string();
    assert!(one.contains("1 other active session"), "{one}");
    assert!(one.contains("configuration changed"), "{one}");
    assert!(!stopped.get());
    assert!(!deleted.get());

    let several = active_config_drift_error(id, 3).to_string();
    assert!(several.contains("3 other active sessions"), "{several}");
    assert!(
        several.contains(&format!("silo containers delete {id} --force")),
        "{several}"
    );
}

#[test]
fn spec_drift_classifies_a_concurrent_stop_as_retryable() {
    let project = test_project("/tmp/project");
    let identity = shared_identity(&project);
    let mut inspection = shared_inspection(&project, &identity);
    inspection
        .labels
        .insert(LABEL_SPEC.to_string(), "b".repeat(64));
    let deleted = Cell::new(false);

    let outcome = replace_outdated_shared_container_with(
        &project,
        &inspection,
        |_| Ok(0),
        || Ok(()),
        |_| Err(anyhow!("ownership changed")),
        |_| {
            deleted.set(true);
            Ok(())
        },
    )
    .expect("a concurrent replacement is not fatal");

    let ReplacementOutcome::Conflict(error) = outcome else {
        panic!("the ownership race must be retried");
    };
    assert!(
        error
            .to_string()
            .contains("could not safely stop the existing container")
    );
    assert!(!deleted.get());
}

#[test]
fn isolated_orphan_discovery_reads_runtime_ids_without_matching_buildkit() {
    let json = br#"[
        {"id":"silo-123","status":{"state":"running"}},
        {"id":"silo-456","status":{"state":"stopped"}},
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
        IMAGE_TAG,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
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
        image_digest: None,
        mount_sources: Vec::new(),
        resources: ContainerResources::default(),
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
    mounts.named.push(resolved_host(
        "shared",
        "/tmp/shared",
        "/home/silo/shared",
        Permission::ReadOnly,
    ));
    let changed = container_identity(
        &project,
        TEST_IMAGE_DIGEST,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &mounts,
        &Container::default(),
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );

    assert_eq!(base, shared_identity(&project));
    assert_ne!(base.spec, changed.spec);
    assert_eq!(base.project.len(), 64);
}

#[test]
fn specification_digest_tracks_read_only_sources_and_targets() {
    let project = test_project("/tmp/project");
    let base = shared_identity(&project);
    let identity_with = |host: &str, relative: &str| {
        container_identity(
            &project,
            TEST_IMAGE_DIGEST,
            &HostIds {
                uid: "501".into(),
                gid: "20".into(),
            },
            &ConfigMounts {
                read_only: vec![read_only_path(host, relative)],
                ..ConfigMounts::default()
            },
            &Container::default(),
            LABEL_SHARED_VALUE,
            &[OsString::from(SHARED_INIT_COMMAND)],
        )
    };

    let git = identity_with("/tmp/project/.git", ".git");
    let jj = identity_with("/tmp/project/.jj", ".jj");
    let aliased_git = identity_with("/tmp/project/.git", "metadata");

    assert_ne!(base.spec, git.spec);
    assert_ne!(git.spec, jj.spec);
    assert_ne!(git.spec, aliased_git.spec);
}

#[test]
fn specification_digest_tracks_the_built_image() {
    let project = test_project("/tmp/project");
    let current = shared_identity(&project);
    let updated = container_identity(
        &project,
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );

    assert_ne!(current.spec, updated.spec);
}

#[test]
fn specification_digest_tracks_container_settings() {
    let project = test_project("/tmp/project");
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let mounts = ConfigMounts::default();
    let command = [OsString::from(SHARED_INIT_COMMAND)];
    let defaults = container_identity(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &mounts,
        &Container::default(),
        LABEL_SHARED_VALUE,
        &command,
    );
    let with_cpus = container_identity(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &mounts,
        &Container {
            cpus: Some(8),
            memory: None,
            sudo: false,
        },
        LABEL_SHARED_VALUE,
        &command,
    );
    let with_memory = container_identity(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &mounts,
        &Container {
            cpus: None,
            memory: Some("8G".to_string()),
            sudo: false,
        },
        LABEL_SHARED_VALUE,
        &command,
    );
    let with_sudo = container_identity(
        &project,
        TEST_IMAGE_DIGEST,
        &ids,
        &mounts,
        &Container {
            sudo: true,
            ..Container::default()
        },
        LABEL_SHARED_VALUE,
        &command,
    );

    assert_ne!(defaults.spec, with_cpus.spec);
    assert_ne!(defaults.spec, with_memory.spec);
    assert_ne!(defaults.spec, with_sudo.spec);
    assert_ne!(with_cpus.spec, with_memory.spec);
}

#[test]
fn embedded_image_contains_guest_lifecycle_programs() {
    assert!(DOCKERFILE.contains("COPY silo-supervisor.sh"));
    assert!(DOCKERFILE.contains("COPY silo-session.sh"));
    assert!(DOCKERFILE.contains("COPY silo-reserve.sh"));
    assert!(DOCKERFILE.contains("COPY silo-status.sh"));
    assert!(DOCKERFILE.contains("COPY silo-stop-guard.sh"));
    assert!(DOCKERFILE.contains("COPY silo-forward.sh"));
    assert!(DOCKERFILE.contains("util-linux"));
    assert!(DOCKERFILE.contains("sudo"));
    assert!(DOCKERFILE.contains("${SILO_SUDO:-0}"));
    assert!(DOCKERFILE.contains("rm -f /etc/sudoers.d/silo"));
    assert!(SUPERVISOR.contains("flock --exclusive --nonblock"));
    assert!(SUPERVISOR.contains("-ge 100"));
    assert!(SESSION_WRAPPER.contains("flock --shared"));
    assert!(SESSION_WRAPPER.contains("reservation_file"));
    assert!(SESSION_RESERVER.contains("flock --shared"));
    assert!(STATUS_HELPER.contains("count=$((count + 1))"));
    assert!(STOP_GUARD.contains("flock --exclusive --nonblock"));
    assert!(FORWARD_WRAPPER.contains("TCP4-LISTEN"));
    assert!(FORWARD_WRAPPER.contains("TCP6-LISTEN"));
    assert!(FORWARD_WRAPPER.contains("touch \"$runtime/ready\""));
    assert!(SESSION_WRAPPER.contains("exec \"$@\""));
    assert!(!DOCKERFILE.contains("SILO_STATE_TARGETS"));
    assert!(!DOCKERFILE.contains("silo-init-state"));
    assert!(!DOCKERFILE.contains("os.lchown"));
}

#[test]
fn forwarding_wrapper_has_valid_posix_shell_syntax() {
    let dir = TestDir::new("forward-wrapper-syntax");
    let path = dir.path().join("silo-forward");
    fs::write(&path, FORWARD_WRAPPER).expect("forwarding wrapper is written");

    let status = Command::new("sh")
        .arg("-n")
        .arg(path)
        .status()
        .expect("POSIX shell starts");

    assert!(status.success());
}

#[test]
fn embedded_image_installs_and_registers_supported_shells() {
    for package in ["fish", "nushell", "zsh"] {
        assert!(
            DOCKERFILE
                .lines()
                .any(|line| line.trim().trim_end_matches(" \\").trim() == package),
            "missing Homebrew package {package}"
        );
    }
    assert!(DOCKERFILE.contains(BASH_PATH));
    for name in ["zsh", "fish", "nu"] {
        let path = format!("${{BREW_PREFIX}}/bin/{name}");
        assert!(DOCKERFILE.contains(&path), "missing shell path {path}");
    }
    assert_eq!(Shell::Bash.path(), BASH_PATH);
    assert_eq!(Shell::Zsh.path(), ZSH_PATH);
    assert_eq!(Shell::Fish.path(), FISH_PATH);
    assert_eq!(Shell::Nu.path(), NU_PATH);
    assert!(DOCKERFILE.contains("CMD [\"zsh\"]"));
    assert!(DOCKERFILE.contains("usermod --shell \"${BREW_PREFIX}/bin/zsh\" silo"));
}

#[test]
fn embedded_image_installs_developer_tooling_and_yazi_dependencies() {
    for package in [
        "actionlint",
        "antigravity-cli",
        "claude-code",
        "copilot-cli",
        "fzf",
        "gh",
        "helix",
        "jq",
        "just",
        "lazygit",
        "playwright-cli",
        "python",
        "qwen-code",
        "ruff",
        "shellcheck",
        "socat",
        "tmux",
        "uv",
        "vim",
        "yamllint",
    ] {
        assert!(
            DOCKERFILE
                .lines()
                .any(|line| line.trim().trim_end_matches(" \\").trim() == package),
            "missing Homebrew package {package}"
        );
    }
    assert!(
        DOCKERFILE
            .lines()
            .any(|line| line.trim().trim_end_matches(" \\").trim() == "file"),
        "missing system package file"
    );
}

#[test]
fn embedded_image_installs_playwright_cli_with_chromium() {
    assert!(DOCKERFILE.contains("PLAYWRIGHT_BROWSERS_PATH=/home/silo/.cache/ms-playwright"));
    assert!(DOCKERFILE.contains("playwright-cli install-browser --with-deps"));
    assert!(DOCKERFILE.contains("sudo rm -f /etc/sudoers.d/silo"));
    assert!(!DOCKERFILE.contains("npm install --global @playwright/test"));
}

#[test]
fn embedded_image_removes_package_caches_in_their_install_layers() {
    assert!(DOCKERFILE.contains("brew cleanup --prune=all"));
    assert!(DOCKERFILE.contains("rm -rf \"$(brew --cache)\""));
    assert!(DOCKERFILE.contains("sudo apt-get clean"));
    assert!(DOCKERFILE.contains("sudo rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*"));
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
        resources: ContainerResources {
            cpus: Some(4),
            memory_bytes: Some(1024 * 1024 * 1024),
        },
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
        inventory_item("silo-222", "/two/project", ContainerLifecycle::Isolated),
    ];
    let error = select_container(&items, "project").expect_err("name is ambiguous");
    let message = error.to_string();
    assert!(message.contains("silo-111"), "{message}");
    assert!(message.contains("silo-222"), "{message}");
    assert!(message.contains("shared"), "{message}");
    assert!(message.contains("isolated"), "{message}");
    assert!(
        message.contains(&display_path(Path::new("/one/project"))),
        "{message}"
    );
    assert!(
        message.contains(&display_path(Path::new("/two/project"))),
        "{message}"
    );
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
        resources: ContainerResources::default(),
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
fn normal_delete_policy_refuses_active_and_unknown_sessions() {
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
    require_inactive_sessions(&item.id, item.sessions).expect("an idle container can be deleted");
}

#[test]
fn container_table_shows_the_agreed_snapshot_fields() {
    let mut item = inventory_item(
        "silo-1234567890abcdef12345678",
        "/work/project",
        ContainerLifecycle::Shared,
    );
    item.resources.cpus = Some(37);
    let table = render_container_table(&[item]);
    for expected in [
        "CONTAINER",
        "TYPE",
        "STATE",
        "SESSIONS",
        "CPUS",
        "MEMORY",
        "PROJECT",
        "IMAGE",
        "37",
        "1 GiB",
        "silo-1234567890a",
        "project",
        "silo:latest",
    ] {
        assert!(table.contains(expected), "missing {expected} in:\n{table}");
    }
    assert!(
        !table.contains(&display_path(Path::new("/work/project"))),
        "{table}"
    );
}

#[test]
fn container_table_formats_memory_compactly_and_marks_unknown_resources() {
    let mut item = inventory_item("silo-known", "/work/known", ContainerLifecycle::Shared);
    item.resources.memory_bytes = Some(1536 * 1024 * 1024);
    let mut unknown = inventory_item("silo-unknown", "/work/unknown", ContainerLifecycle::Shared);
    unknown.resources = ContainerResources::default();

    let table = render_container_table(&[item, unknown]);

    assert!(table.contains("1536 MiB"), "{table}");
    assert!(table.contains('?'), "{table}");
}

#[test]
fn container_table_uses_paths_only_for_repeated_project_names() {
    let items = [
        inventory_item("silo-alpha", "/work/alpha", ContainerLifecycle::Shared),
        inventory_item(
            "silo-alpha-isolated",
            "/work/alpha",
            ContainerLifecycle::Isolated,
        ),
        inventory_item("silo-one", "/one/project", ContainerLifecycle::Shared),
        inventory_item("silo-two", "/two/project", ContainerLifecycle::Shared),
    ];

    let rendered = render_container_table(&items);

    assert!(rendered.contains("alpha"), "{rendered}");
    assert!(
        !rendered.contains(&display_path(Path::new("/work/alpha"))),
        "{rendered}"
    );
    assert!(
        rendered.contains(&display_path(Path::new("/one/project"))),
        "{rendered}"
    );
    assert!(
        rendered.contains(&display_path(Path::new("/two/project"))),
        "{rendered}"
    );
}

#[test]
fn project_display_falls_back_to_paths_without_a_basename() {
    let ambiguous_names = ambiguous_project_names([Path::new("/")]);

    assert_eq!(display_project(Path::new("/"), &ambiguous_names), "/");
}

#[cfg(unix)]
#[test]
fn project_display_keeps_paths_for_non_utf8_basenames() {
    use std::os::unix::ffi::OsStrExt;

    let first = Path::new(OsStr::from_bytes(b"/one/\x80"));
    let second = Path::new(OsStr::from_bytes(b"/two/\x81"));
    let ambiguous_names = ambiguous_project_names([first, second]);

    assert_eq!(
        display_project(first, &ambiguous_names),
        display_path(first)
    );
    assert_eq!(
        display_project(second, &ambiguous_names),
        display_path(second)
    );
    assert_ne!(
        display_project(first, &ambiguous_names),
        display_project(second, &ambiguous_names)
    );
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
    let reserved = Command::new("sh")
        .arg(&guard)
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("guard checks the pending reservation");
    assert_eq!(reserved.status.code(), Some(76));
    assert_eq!(String::from_utf8_lossy(&reserved.stderr).trim(), "1");

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
