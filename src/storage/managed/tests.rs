use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::test_support::test_dir;

#[test]
fn owner_encodes_the_state_scope_invariant() {
    let root = Path::new("/state");
    let project = PathBuf::from("/work/project");
    let project_mount = managed_mount_at_root(StateOwner::Project(project.clone()), "cargo", root);
    let user_mount = managed_mount_at_root(StateOwner::User, "codex", root);

    assert_eq!(project_mount.owner.project(), Some(project.as_path()));
    assert_eq!(user_mount.owner.project(), None);
    assert!(project_mount.id.starts_with("silo-state-p-"));
    assert!(user_mount.id.starts_with("silo-state-u-"));
    assert!(project_mount.path.ends_with("entries/cargo"));
    assert_eq!(user_mount.path, Path::new("/state/user/codex"));
}

#[test]
fn project_metadata_round_trips_and_rejects_mismatches() {
    let dir = test_dir("metadata");
    let project_dir = dir.path().join("project");
    fs::create_dir(&project_dir).expect("project state directory creates");
    let project = Path::new("/work/project");
    ensure_project_metadata(&project_dir, project).expect("metadata writes");
    assert_eq!(
        read_project_metadata(&project_dir).expect("metadata reads"),
        project
    );

    let error = ensure_project_metadata(&project_dir, Path::new("/other/project"))
        .expect_err("different projects cannot share state")
        .to_string();
    assert!(error.contains("does not match"), "{error}");
}

#[cfg(unix)]
#[test]
fn project_metadata_rejects_linked_files() {
    use std::os::unix::fs::symlink;

    let dir = test_dir("linked-metadata");
    let project_dir = dir.path().join("project");
    fs::create_dir(&project_dir).expect("project state directory creates");
    let metadata = project_dir.join(PROJECT_ROOT_METADATA);
    let target = dir.path().join("metadata-target");
    fs::write(&target, "/work/project").expect("metadata target writes");

    symlink(&target, &metadata).expect("metadata symlink creates");
    assert!(
        read_project_metadata(&project_dir).is_err(),
        "symlinked metadata must fail closed"
    );

    fs::remove_file(&metadata).expect("metadata symlink removes");
    fs::hard_link(&target, &metadata).expect("metadata hard link creates");
    assert!(
        read_project_metadata(&project_dir).is_err(),
        "hard-linked metadata must fail closed"
    );
}

#[test]
fn inventory_returns_only_valid_real_directories() {
    let dir = test_dir("inventory");
    let root = dir.path().join("state");
    let valid = root.join("user/codex");
    fs::create_dir_all(&valid).expect("valid state creates");
    fs::write(root.join("user/not-a-directory"), "").expect("invalid entry creates");

    let MountInventory { items, warnings } = mount_inventory_at(&root);
    assert_eq!(
        items,
        [managed_mount_at_root(StateOwner::User, "codex", &root)]
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("not a real directory"));
}

#[test]
fn project_inventory_requires_matching_metadata_and_real_directories() {
    let dir = test_dir("project-inventory");
    let root = dir.path().join("state");
    let project = PathBuf::from("/work/project");
    let mount = managed_mount_at_root(StateOwner::Project(project.clone()), "cargo", &root);
    ensure_managed_mount(&mount).expect("project state creates");

    let inventory = mount_inventory_at(&root);
    assert_eq!(inventory.items, std::slice::from_ref(&mount));
    assert!(inventory.warnings.is_empty());

    let project_dir = mount
        .path
        .parent()
        .and_then(Path::parent)
        .expect("project directory exists");
    fs::write(project_dir.join(PROJECT_ROOT_METADATA), "/other/project").expect("metadata changes");
    let inventory = mount_inventory_at(&root);
    assert!(inventory.items.is_empty());
    assert!(
        inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("metadata does not match its digest"))
    );
}

#[cfg(unix)]
#[test]
fn managed_mount_validation_rejects_a_replaced_directory() {
    use std::os::unix::fs::symlink;

    let dir = test_dir("replaced-mount");
    let root = dir.path().join("state");
    let mount = managed_mount_at_root(StateOwner::User, "cache", &root);
    ensure_managed_mount(&mount).expect("user state creates");
    fs::remove_dir(&mount.path).expect("managed directory removes");
    let replacement = dir.path().join("replacement");
    fs::create_dir(&replacement).expect("replacement creates");
    symlink(&replacement, &mount.path).expect("managed path is replaced");

    let error = validate_managed_mount(&mount)
        .expect_err("symlinked managed state fails closed")
        .to_string();
    assert!(error.contains("not a real directory"), "{error}");
}

#[test]
fn pruning_removes_only_an_empty_project_state_envelope() {
    let dir = test_dir("prune-project");
    let root = dir.path().join("state");
    let project = PathBuf::from("/work/project");
    let mount = managed_mount_at_root(StateOwner::Project(project), "cache", &root);
    ensure_managed_mount(&mount).expect("project state creates");
    let project_dir = mount
        .path
        .parent()
        .and_then(Path::parent)
        .expect("project directory exists")
        .to_path_buf();
    fs::remove_dir(&mount.path).expect("state entry removes");

    prune_empty_project_state_directory(&mount.path).expect("empty envelope prunes");
    assert!(!project_dir.exists());
}
