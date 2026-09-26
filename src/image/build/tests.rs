use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;
use crate::image::runtime_contract::*;
use crate::test_support::test_dir;
use anyhow::anyhow;

const NOT_STARTED_HINT: &str =
    "Ensure container system service has been started with `container system start`.";

fn command_args(command: &Command) -> Vec<&str> {
    command
        .get_args()
        .map(|argument| argument.to_str().expect("argument is UTF-8"))
        .collect()
}

fn append_log(path: &Path, line: &str) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("log opens");
    writeln!(file, "{line}").expect("log writes");
}

fn read_log(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("log reads")
        .lines()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn build_commands_pull_only_the_runtime_base() {
    let build_args = runtime_asset_build_args();
    let base = build_command(
        Path::new("/tmp/Dockerfile"),
        Path::new("/tmp/context"),
        BASE_STAGING_IMAGE_TAG,
        true,
        BuildCache::Disabled,
        &build_args,
    );
    let base_args = command_args(&base);
    assert_eq!(
        &base_args[..7],
        [
            "build",
            "--file",
            "/tmp/Dockerfile",
            "--tag",
            BASE_STAGING_IMAGE_TAG,
            "--pull",
            "--no-cache",
        ]
    );
    assert_eq!(
        base_args
            .iter()
            .filter(|argument| **argument == "--build-arg")
            .count(),
        RUNTIME_ASSETS.len()
    );
    assert_eq!(base_args.last(), Some(&"/tmp/context"));

    let derivative = build_command(
        Path::new("/tmp/Dockerfile"),
        Path::new("/tmp/context"),
        STAGING_IMAGE_TAG,
        false,
        BuildCache::Reuse,
        &build_args,
    );
    let derivative_args = command_args(&derivative);
    assert!(!derivative_args.contains(&"--pull"));
    assert!(!derivative_args.contains(&"--no-cache"));
    assert_eq!(
        derivative_args
            .iter()
            .filter(|argument| **argument == "--build-arg")
            .count(),
        RUNTIME_ASSETS.len()
    );
    assert_eq!(derivative_args.last(), Some(&"/tmp/context"));
}
#[test]
fn runtime_check_uses_the_standard_entrypoint_for_a_small_smoke_test() {
    let command = image_runtime_check_command(STAGING_IMAGE_TAG);
    let args = command_args(&command);

    assert!(args.windows(2).any(|pair| pair == ["--user", "root"]));
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--entrypoint", ENTRYPOINT_COMMAND])
    );
    assert!(args.windows(2).any(|pair| pair == ["--env", "SILO_SUDO=0"]));
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--env", "SILO_INTERNAL_HOST_PORTS=0"])
    );
    assert!(!args.contains(&"--mount"));
    let image_index = args
        .iter()
        .position(|argument| *argument == STAGING_IMAGE_TAG)
        .expect("staging image argument exists");
    assert_eq!(&args[image_index + 1..image_index + 3], ["/bin/sh", "-c"]);
    assert!(args[image_index + 3].contains("id -un"));
    assert!(args[image_index + 3].contains("silo-lifecycle"));
}

#[test]
fn maintenance_commands_target_global_apple_storage() {
    assert_eq!(
        command_args(&builder_delete_command()),
        ["builder", "delete", "--force"]
    );
    assert_eq!(
        command_args(&image_tag_command(STAGING_IMAGE_TAG, DEFAULT_IMAGE_TAG)),
        ["image", "tag", "silo-build:staging", "silo:latest"]
    );
}

#[test]
fn staging_deletion_rejects_published_aliases() {
    for (staging, stable) in [
        (BASE_STAGING_IMAGE_TAG, BASE_IMAGE_TAG),
        (STAGING_IMAGE_TAG, DEFAULT_IMAGE_TAG),
        (STAGING_IMAGE_TAG, "silo:custom-example"),
    ] {
        // Builds keep their short tag; publication adds the registry prefix.
        for reference in [
            staging.to_string(),
            format!("registry-1.docker.io/library/{stable}"),
            format!("docker.io/library/{staging}"),
        ] {
            let inspection = serde_json::json!([{"configuration": {
                "name": reference,
                "descriptor": {"annotations": {
                    "com.apple.containerization.image.name": staging
                }}
            }}]);
            let command = image_delete_command(staging, inspection.to_string().as_bytes()).unwrap();
            assert_eq!(command.is_some(), reference == staging);
        }
    }
    assert!(image_delete_command(STAGING_IMAGE_TAG, b"[]").is_err());
    assert!(image_delete_command(STAGING_IMAGE_TAG, b"not json").is_err());
}

#[test]
fn superseded_deletion_only_removes_the_reclaim_tag_for_the_replaced_digest() {
    const PREVIOUS: &str = "sha256:old-silo-image";
    const CURRENT: &str = "sha256:new-silo-image";
    let matching = serde_json::json!([{
        "configuration": {
            "name": OBSOLETE_IMAGE_TAG,
            "descriptor": {"digest": PREVIOUS}
        }
    }])
    .to_string();

    let command =
        superseded_image_delete_command(OBSOLETE_IMAGE_TAG, PREVIOUS, matching.as_bytes())
            .expect("replaced reclaim tag is eligible")
            .expect("delete command is produced");
    assert_eq!(
        command_args(&command),
        ["image", "delete", "--force", OBSOLETE_IMAGE_TAG]
    );

    for stored in [
        format!("registry-1.docker.io/library/{OBSOLETE_IMAGE_TAG}"),
        format!("docker.io/library/{OBSOLETE_IMAGE_TAG}"),
    ] {
        let inspection = serde_json::json!([{
            "configuration": {
                "name": stored.as_str(),
                "descriptor": {"digest": PREVIOUS}
            }
        }]);
        let command = superseded_image_delete_command(
            OBSOLETE_IMAGE_TAG,
            PREVIOUS,
            inspection.to_string().as_bytes(),
        )
        .expect("normalized reclaim tag is eligible")
        .expect("delete command uses the stored name");
        assert_eq!(
            command_args(&command),
            ["image", "delete", "--force", stored.as_str()]
        );
    }

    for (name, digest) in [
        (OBSOLETE_IMAGE_TAG.to_string(), CURRENT),
        (DEFAULT_IMAGE_TAG.to_string(), PREVIOUS),
        (
            format!("registry-1.docker.io/library/{DEFAULT_IMAGE_TAG}"),
            PREVIOUS,
        ),
    ] {
        let inspection = serde_json::json!([{
            "configuration": {
                "name": name,
                "descriptor": {"digest": digest}
            }
        }]);
        assert!(
            superseded_image_delete_command(
                OBSOLETE_IMAGE_TAG,
                PREVIOUS,
                inspection.to_string().as_bytes()
            )
            .expect("mismatched inspect is left")
            .is_none()
        );
    }

    assert!(superseded_image_delete_command(OBSOLETE_IMAGE_TAG, PREVIOUS, b"[]").is_err());
    assert!(superseded_image_delete_command(OBSOLETE_IMAGE_TAG, PREVIOUS, b"not json").is_err());
}

#[test]
fn either_candidate_failing_prevents_all_publication() {
    for fail_base in [true, false] {
        for execution_error in [true, false] {
            let derivative_attempted = Cell::new(false);
            let published = Cell::new(false);
            let failure = || {
                if execution_error {
                    Err(anyhow!("runtime unavailable"))
                } else {
                    Ok(ExitCode::from(23))
                }
            };
            let result = run_image_publication(
                || {
                    if fail_base {
                        failure()
                    } else {
                        Ok(ExitCode::SUCCESS)
                    }
                },
                || {
                    derivative_attempted.set(true);
                    failure()
                },
                || {
                    published.set(true);
                    Ok(())
                },
            );
            assert_eq!(derivative_attempted.get(), !fail_base);
            assert!(!published.get());
            if execution_error {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("runtime unavailable")
                );
            } else {
                assert_eq!(result.unwrap(), ExitCode::from(23));
            }
        }
    }
}

#[test]
fn publication_waits_for_both_validated_candidates_and_preserves_errors() {
    for publication_error in [false, true] {
        let events = RefCell::new(Vec::new());
        let result = run_image_publication(
            || {
                events.borrow_mut().push("base-ready");
                Ok(ExitCode::SUCCESS)
            },
            || {
                events.borrow_mut().push("derivative-ready");
                Ok(ExitCode::SUCCESS)
            },
            || {
                events.borrow_mut().push("publish");
                if publication_error {
                    Err(anyhow!("publication failed"))
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(
            *events.borrow(),
            ["base-ready", "derivative-ready", "publish"]
        );
        if publication_error {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("publication failed")
            );
        } else {
            assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        }
    }
}

#[test]
fn build_lifecycle_orders_work_and_preserves_failures() {
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
    .expect("build lifecycle succeeds");
    assert_eq!(result, ExitCode::SUCCESS);
    assert_eq!(
        events.into_inner(),
        ["delete-before", "build", "cleanup-after"]
    );

    let failed = run_build_lifecycle(
        || Ok(()),
        || Ok(ExitCode::from(23)),
        || Err(anyhow!("cleanup failed")),
    )
    .expect("build status wins over cleanup failure");
    assert_eq!(failed, ExitCode::from(23));
}

#[test]
fn build_lifecycle_reports_preflight_and_success_cleanup_failures() {
    let built = Cell::new(false);
    let error = run_build_lifecycle(
        || Err(anyhow!("delete failed")),
        || {
            built.set(true);
            Ok(ExitCode::SUCCESS)
        },
        || Ok(()),
    )
    .expect_err("preflight failure aborts the build");
    assert!(error.to_string().contains("delete failed"));
    assert!(!built.get());

    let error = run_build_lifecycle(
        || Ok(()),
        || Ok(ExitCode::SUCCESS),
        || Err(anyhow!("cleanup failed")),
    )
    .expect_err("successful build reports cleanup failure");
    assert!(error.to_string().contains("cleanup failed"));
}

#[test]
fn captured_stderr_is_forwarded_and_bounded() {
    let mut command = Command::new("sh");
    command.args(["-c", "printf to-stderr >&2; exit 7"]);
    let captured = run_captured(&mut command).expect("command runs");
    assert_eq!(captured.status.code(), Some(7));
    assert_eq!(captured.stderr, b"to-stderr");

    let mut large = vec![b'a'; 300 * 1024];
    large.extend_from_slice(b"tail");
    trim_captured(&mut large);
    assert_eq!(large.len(), 256 * 1024);
    assert!(large.ends_with(b"tail"));
}

#[test]
fn execute_build_boots_before_building_when_the_probe_reports_a_stopped_system() {
    let dir = test_dir("build-boot-first");
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

    assert_eq!(result, ExitCode::SUCCESS);
    assert_eq!(boots.get(), 1);
    assert_eq!(read_log(&log), ["boot", "build"]);
}

#[test]
fn execute_build_retries_once_when_the_first_failure_reports_a_stopped_system() {
    let dir = test_dir("build-retry");
    let log = dir.path().join("log");
    let marker = dir.path().join("built");
    let log_path = log.display();
    let marker_path = marker.display();
    let boots = Cell::new(0);
    let mut command = Command::new("sh");
    command.args([
        "-c",
        &format!(
            "echo build >> '{log_path}'; if [ ! -e '{marker_path}' ]; then touch '{marker_path}'; echo '{NOT_STARTED_HINT}' >&2; exit 1; fi"
        ),
    ]);
    let result = execute_build_with(
        &mut command,
        || Ok((None, "Error: image not found".to_string())),
        || {
            boots.set(boots.get() + 1);
            append_log(&log, "boot");
            true
        },
    )
    .expect("build runs");

    assert_eq!(result, ExitCode::SUCCESS);
    assert_eq!(boots.get(), 1);
    assert_eq!(read_log(&log), ["build", "boot", "build"]);
}

#[test]
fn execute_build_preserves_unrelated_failure_status() {
    let boots = Cell::new(0);
    let mut command = Command::new("sh");
    command.args(["-c", "exit 23"]);
    let result = execute_build_with(
        &mut command,
        || Ok((None, "Error: image not found".to_string())),
        || {
            boots.set(boots.get() + 1);
            true
        },
    )
    .expect("build runs");

    assert_eq!(result, ExitCode::from(23));
    assert_eq!(boots.get(), 0);
}
#[test]
fn build_directory_is_removed_on_drop() {
    let dir = test_dir("build-directory");
    let build_dir =
        BuildDir::create_for_test(dir.path()).expect("build directory creation succeeds");
    let path = build_dir.path().to_path_buf();
    assert!(path.is_dir());
    drop(build_dir);
    assert!(!path.exists());
}

#[test]
fn build_context_contains_only_the_embedded_base_dockerfile() {
    let dir = test_dir("build-context");
    let build_dir =
        BuildDir::create_for_test(dir.path()).expect("build directory creation succeeds");

    write_build_context(&build_dir).expect("build context write succeeds");

    let entries = fs::read_dir(build_dir.path())
        .expect("build context reads")
        .map(|entry| entry.expect("build context entry reads").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, [OsString::from("silo-base.dockerfile")]);
    assert_eq!(
        fs::read_to_string(build_dir.path().join("silo-base.dockerfile"))
            .expect("base Dockerfile reads"),
        BASE_DOCKERFILE
    );
}

#[test]
fn dockerfile_validation_and_context_cover_supported_paths() {
    assert!(validate_dockerfile(Path::new("")).is_err());
    let dir = test_dir("dockerfile-validation");
    assert!(validate_dockerfile(dir.path()).is_err());
    let dockerfile = dir.path().join("Dockerfile");
    assert!(validate_dockerfile(&dockerfile).is_err());
    fs::write(&dockerfile, "FROM scratch\n").expect("Dockerfile write succeeds");
    assert!(validate_dockerfile(&dockerfile).is_err());
    fs::write(&dockerfile, "FROM silo-base:latest\n").expect("Dockerfile write succeeds");
    validate_dockerfile(&dockerfile).expect("regular Dockerfile is valid");

    assert_eq!(
        dockerfile_context(Path::new("images/dev/Dockerfile")),
        Path::new("images/dev")
    );
    assert_eq!(dockerfile_context(Path::new("Dockerfile")), Path::new("."));
}

#[test]
fn dockerfile_specific_ignore_rules_follow_the_generated_dockerfile() {
    let dir = test_dir("dockerfile-ignore");
    let dockerfile = dir.path().join("Containerfile.dev");
    let source = dir.path().join("Containerfile.dev.dockerignore");
    let destination = dir.path().join("silo-derivative.dockerfile.dockerignore");
    fs::write(&dockerfile, "FROM silo-base:latest\n").expect("Dockerfile write succeeds");
    fs::write(&source, "ignored.txt\n").expect("ignore file write succeeds");

    copy_dockerignore(&dockerfile, &destination).expect("ignore file copy succeeds");

    assert_eq!(
        fs::read_to_string(destination).expect("copied ignore file reads"),
        "ignored.txt\n"
    );
}

#[test]
fn build_validates_custom_dockerfiles_before_runtime_access() {
    let dir = test_dir("missing-build-dockerfile");
    let mut config = Config::default();
    config.image.dockerfile = Some(dir.path().join("Dockerfile"));

    let error = build(&config, dir.path()).expect_err("missing Dockerfile prevents maintenance");
    assert!(error.to_string().contains("does not exist"));
}

#[test]
fn build_lock_serializes_competing_image_builds() {
    let dir = test_dir("build-lock");
    let first = acquire_build_lock_at(dir.path()).expect("first lock succeeds");
    let root = dir.path().to_path_buf();
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let second = acquire_build_lock_at(&root).expect("second lock succeeds");
        acquired_tx.send(()).expect("acquisition is reported");
        drop(second);
    });

    assert!(
        acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    drop(first);
    acquired_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("second lock proceeds after release");
    handle.join().expect("lock thread joins");
}

#[test]
fn build_lock_location_is_stable_for_the_current_user() {
    assert_eq!(
        global_build_lock_root(),
        Path::new(BUILD_LOCK_PARENT).join(format!("silo-build-{}", unsafe { libc::geteuid() }))
    );
}
#[test]
fn update_build_command_disables_cache_without_pulling_the_base() {
    let command = update_build_command(Path::new("/tmp/silo-extras.dockerfile"), Path::new("/tmp"));
    let args = command_args(&command);
    assert_eq!(
        &args[..7],
        [
            "build",
            "--file",
            "/tmp/silo-extras.dockerfile",
            "--tag",
            STAGING_IMAGE_TAG,
            "--no-cache",
            "/tmp",
        ]
    );
    assert!(!args.contains(&"--pull"));
    assert!(!args.contains(&"--build-arg"));
}

#[test]
fn update_writes_the_embedded_extras_dockerfile_unchanged() {
    let dir = test_dir("update-extras");
    let build_dir =
        BuildDir::create_for_test(dir.path()).expect("build directory creation succeeds");
    let (dockerfile, context) = update_dockerfile(&Config::default(), dir.path(), &build_dir)
        .expect("embedded extras resolve");

    assert_eq!(context, build_dir.path());
    assert_eq!(
        fs::read_to_string(&dockerfile).expect("extras Dockerfile reads"),
        EXTRAS_DOCKERFILE
    );
    assert!(EXTRAS_DOCKERFILE.starts_with("FROM silo-base:latest\n"));
    let entries = fs::read_dir(build_dir.path())
        .expect("build context reads")
        .map(|entry| entry.expect("build context entry reads").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, [OsString::from("silo-derivative.dockerfile")]);
}

#[test]
fn update_uses_a_configured_dockerfile_and_its_context_directly() {
    let dir = test_dir("update-custom");
    let dockerfile = dir.path().join("Dockerfile.silo");
    fs::write(&dockerfile, "FROM silo-base:latest\nRUN true\n").expect("Dockerfile write succeeds");
    let build_dir =
        BuildDir::create_for_test(dir.path()).expect("build directory creation succeeds");
    let mut config = Config::default();
    config.image.dockerfile = Some(dockerfile.clone());

    let (resolved, context) =
        update_dockerfile(&config, dir.path(), &build_dir).expect("configured Dockerfile resolves");
    assert_eq!(resolved, dockerfile);
    assert_eq!(context, dir.path());

    let command = update_build_command(&resolved, &context);
    let args = command_args(&command);
    assert!(args.windows(2).any(|pair| {
        pair == [
            "--file",
            dockerfile.to_str().expect("dockerfile path is UTF-8"),
        ]
    }));
    assert_eq!(args.last().copied(), dir.path().to_str());
    assert!(args.contains(&"--no-cache"));
    assert!(!args.contains(&"--pull"));
    assert!(!args.contains(&"--build-arg"));
    assert!(args.contains(&STAGING_IMAGE_TAG));
    let entries = fs::read_dir(build_dir.path())
        .expect("temporary context reads")
        .count();
    assert_eq!(entries, 0);
}

#[test]
fn update_validates_custom_dockerfiles_before_runtime_access() {
    let dir = test_dir("missing-update-dockerfile");
    let mut config = Config::default();
    config.image.dockerfile = Some(dir.path().join("Dockerfile"));

    let error = update(&config, dir.path()).expect_err("missing Dockerfile prevents an update");
    assert!(error.to_string().contains("does not exist"));
}

#[test]
fn missing_base_requires_a_full_image_build() {
    let missing = require_base_digest(Ok(None)).expect_err("missing base");
    let message = missing.to_string();
    assert!(message.contains(BASE_IMAGE_TAG));
    assert!(message.contains("silo image build"));

    let digest = "sha256:abc";
    assert_eq!(
        require_base_digest(Ok(Some(digest.to_string()))).expect("present base"),
        digest
    );

    let inspect = require_base_digest(Err(anyhow!("could not check for image `{BASE_IMAGE_TAG}`")))
        .expect_err("inspect failure");
    assert!(inspect.to_string().contains("could not check"));
    assert!(!inspect.to_string().contains("not built yet"));
}

#[test]
fn update_publication_keeps_a_moved_tag_when_reclaim_cleanup_fails() {
    assert!(finish_updated_publication(Ok(()), Err(anyhow!("image is in use"))).is_ok());

    let error = finish_updated_publication(Err(anyhow!("publish failed")), Ok(()))
        .expect_err("publication failure remains");
    assert!(error.to_string().contains("publish failed"));
}

#[test]
fn update_publication_replaces_only_the_derivative_tag() {
    for target in ["silo:latest", "silo:custom-example"] {
        let publication = update_publication(target);
        assert_eq!(publication.reclaim_tag, OBSOLETE_IMAGE_TAG);
        let publish = command_args(&publication.publish);
        assert_eq!(publish, ["image", "tag", STAGING_IMAGE_TAG, target]);
        let retain_command = image_tag_command(target, publication.reclaim_tag);
        let retain = command_args(&retain_command);
        assert_eq!(retain, ["image", "tag", target, OBSOLETE_IMAGE_TAG]);
        let rendered = format!("{retain:?} {publish:?}");
        assert!(!rendered.contains(BASE_IMAGE_TAG));
        assert!(!rendered.contains(OBSOLETE_BASE_IMAGE_TAG));
        assert!(!rendered.contains(BASE_STAGING_IMAGE_TAG));
    }
}

#[test]
fn runtime_asset_build_arguments_round_trip_without_context_files() {
    let build_args = runtime_asset_build_args();
    assert_eq!(build_args.len(), RUNTIME_ASSETS.len());
    for (asset, build_arg) in RUNTIME_ASSETS.iter().zip(build_args) {
        let (name, encoded) = build_arg
            .split_once('=')
            .expect("runtime build argument has a value");
        assert_eq!(name, asset.build_arg);
        assert_eq!(
            BASE64_STANDARD
                .decode(encoded)
                .expect("runtime build argument is base64"),
            asset.contents.as_bytes()
        );
    }
}
