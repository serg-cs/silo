use std::cell::Cell;
use std::ffi::OsStr;

use super::*;

#[test]
fn creation_uses_only_the_minimal_runtime_identity() {
    let project = test_project("/tmp/project");
    let command = create_command(
        &project,
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts::default(),
        &Container::default(),
        Path::new("/tmp/container.cid"),
    )
    .expect("command builds");
    let labels = command_labels(&command);

    assert_eq!(labels.len(), 4);
    assert_eq!(labels[LABEL_OWNER], LABEL_OWNER_VALUE);
    assert_eq!(labels[LABEL_PROJECT_ROOT], "/tmp/project");
    assert_eq!(labels[LABEL_LIFECYCLE], LABEL_SHARED_VALUE);
    assert_eq!(labels[LABEL_INSTANCE], TEST_INSTANCE);
    assert!(args_without_labels(&command).ends_with(&[TEST_IMAGE, LIFECYCLE_COMMAND, "init"]));
}

#[test]
fn ownership_requires_consistent_labels_and_runtime_id() {
    let project = test_project("/tmp/project");
    let valid = inspection(&project, ContainerLifecycle::Shared);
    assert_eq!(
        silo_metadata(&project.id, &valid),
        Some((
            ContainerLifecycle::Shared,
            project.root.clone(),
            TEST_INSTANCE.to_string(),
        ))
    );
    assert!(silo_metadata("wrong-id", &valid).is_none());

    let mut invalid = valid;
    invalid
        .labels
        .insert(LABEL_INSTANCE.to_string(), "ABC".repeat(21));
    assert!(silo_metadata(&project.id, &invalid).is_none());

    let mut missing_instance = inspection(&project, ContainerLifecycle::Shared);
    missing_instance.labels.remove(LABEL_INSTANCE);
    assert!(silo_metadata(&project.id, &missing_instance).is_none());

    let selected =
        shared_container_info(&project, &inspection(&project, ContainerLifecycle::Shared))
            .expect("owned container is selectable");
    let mut replacement = inspection(&project, ContainerLifecycle::Shared);
    replacement
        .labels
        .insert(LABEL_INSTANCE.to_string(), "b".repeat(64));
    let error = validate_selected_ownership(&selected, &replacement)
        .expect_err("a replacement instance is never managed through an old selection")
        .to_string();
    assert!(error.contains("no longer the selected"), "{error}");
}

#[test]
fn failed_creation_retries_only_when_an_owned_instance_appeared() {
    let project = test_project("/tmp/project");
    let current_instance = "b".repeat(64);
    let fatal = classify_failed_creation(
        &project,
        &current_instance,
        None,
        anyhow::anyhow!("runtime rejected create"),
    )
    .expect_err("a deterministic failure is returned immediately")
    .to_string();
    assert_eq!(fatal, "runtime rejected create");

    let retry = classify_failed_creation(
        &project,
        &current_instance,
        Some(inspection(&project, ContainerLifecycle::Shared)),
        anyhow::anyhow!("name conflict"),
    )
    .expect("an owned competing instance is retryable");
    assert!(matches!(retry, SharedCreation::Retry(_)));

    let mut unowned = inspection(&project, ContainerLifecycle::Shared);
    unowned
        .labels
        .insert(LABEL_OWNER.to_string(), "someone-else".to_string());
    let refusal = classify_failed_creation(
        &project,
        &current_instance,
        Some(unowned),
        anyhow::anyhow!("name conflict"),
    )
    .expect_err("an unowned collision must fail closed")
    .to_string();
    assert!(
        refusal.contains("ownership labels are invalid"),
        "{refusal}"
    );

    let own_failure = classify_failed_creation(
        &project,
        TEST_INSTANCE,
        Some(inspection(&project, ContainerLifecycle::Shared)),
        anyhow::anyhow!("runtime rejected create"),
    )
    .expect_err("a failed instance from this attempt is not a concurrency race")
    .to_string();
    assert_eq!(own_failure, "runtime rejected create");
}

#[test]
fn isolated_cleanup_deletes_only_the_current_instance() {
    let project = test_project("/tmp/project");
    let id = "silo-123";
    let deleted = Cell::new(false);
    cleanup_isolated_container_with(
        id,
        &project,
        &"b".repeat(64),
        |_| Ok(Some(inspection(&project, ContainerLifecycle::Isolated))),
        |_| {
            deleted.set(true);
            Ok(())
        },
    )
    .expect("mismatched cleanup remains safe");
    assert!(!deleted.get());

    cleanup_isolated_container_with(
        id,
        &project,
        TEST_INSTANCE,
        |_| Ok(Some(inspection(&project, ContainerLifecycle::Isolated))),
        |deleted_id| {
            assert_eq!(deleted_id, id);
            deleted.set(true);
            Ok(())
        },
    )
    .expect("owned cleanup succeeds");
    assert!(deleted.get());
}

#[test]
fn session_commands_use_the_consolidated_guest_lifecycle() {
    let project = test_project("/tmp/project");
    let reserve = session_reserve_command(&project);
    let reserve_args: Vec<&str> = reserve
        .get_args()
        .map(|argument| argument.to_str().expect("argument is UTF-8"))
        .collect();
    assert!(reserve_args.ends_with(&[LIFECYCLE_COMMAND, "reserve"]));

    let session = exec_command(
        false,
        &project,
        "abc123",
        &[OsString::from("true")],
        Shell::Zsh,
    );
    let session_args: Vec<&str> = session
        .get_args()
        .map(|argument| argument.to_str().expect("argument is UTF-8"))
        .collect();
    assert!(session_args.ends_with(&[LIFECYCLE_COMMAND, "session", "abc123", "true"]));
}

#[test]
fn resource_and_shell_validation_remain_strict() {
    assert!(
        crate::container::runtime::validate_config(&Container {
            cpus: Some(0),
            ..Container::default()
        })
        .is_err()
    );
    assert_eq!(
        resolve_shell(Some(Shell::Fish), Some(OsStr::new("/bin/bash"))),
        Shell::Fish
    );
    assert_eq!(
        resolve_shell(None, Some(OsStr::new("/bin/unknown"))),
        Shell::Zsh
    );
}
