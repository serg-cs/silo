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
    let command = run_command(true, Path::new("/tmp/project"), &ids).expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args,
        [
            "run",
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
    let command = run_command(false, Path::new("/tmp/project"), &ids).expect("command builds");
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    assert_eq!(
        args,
        [
            "run",
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
    let command = run_command(true, Path::new("/home/user/src/silo"), &ids).expect("command builds");
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
    let err = run_command(true, Path::new("/"), &ids).expect_err("root has no name");
    assert!(err.to_string().contains("cannot share the root directory"));
}

#[test]
fn run_command_rejects_paths_with_colons() {
    let ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let err = run_command(true, Path::new("/tmp/foo:bar"), &ids).expect_err("colon is a separator");
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
    let err = run_command(true, path, &ids).expect_err("name is not valid UTF-8");
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
