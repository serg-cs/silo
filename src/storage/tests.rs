use std::sync::mpsc;
use std::time::Duration;

use super::*;

#[test]
fn lock_serializes_competing_callers() {
    let directory = tempfile::tempdir().expect("temporary lock directory is created");
    let path = directory.path().join("operation.lock");
    let first = Lock::acquire(&path, "test operation").expect("first lock succeeds");
    let (started_tx, started_rx) = mpsc::channel();
    let (acquired_tx, acquired_rx) = mpsc::channel();

    let handle = std::thread::spawn(move || {
        started_tx.send(()).expect("lock attempt is announced");
        let second = Lock::acquire(&path, "test operation").expect("second lock succeeds");
        acquired_tx.send(()).expect("lock acquisition is announced");
        drop(second);
    });

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("second caller starts");
    assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
    drop(first);
    acquired_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("second caller proceeds after release");
    handle.join().expect("lock thread joins");
}

#[test]
fn lock_rejects_a_symlink() {
    let directory = tempfile::tempdir().expect("temporary lock directory is created");
    let target = directory.path().join("target");
    fs::write(&target, b"").expect("lock target is created");
    let path = directory.path().join("operation.lock");
    std::os::unix::fs::symlink(&target, &path).expect("lock symlink is created");

    assert!(Lock::acquire(&path, "test operation").is_err());
}

#[test]
fn owned_file_permissions_are_repaired_through_its_descriptor() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary file directory is created");
    let path = directory.path().join("managed");
    fs::write(&path, b"managed").expect("managed file is created");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400))
        .expect("managed file is made read-only");

    let file = open_owned_file(&path, 0o600, "test file").expect("managed file is protected");

    assert_eq!(
        file.metadata()
            .expect("managed file metadata is available")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn owned_file_rejects_hard_links() {
    let directory = tempfile::tempdir().expect("temporary file directory is created");
    let path = directory.path().join("managed");
    let alias = directory.path().join("alias");
    fs::write(&path, b"managed").expect("managed file is created");
    fs::hard_link(&path, &alias).expect("hard link is created");

    assert!(open_owned_file(&path, 0o600, "test file").is_err());
}

#[test]
fn shared_parent_directory_is_created_private() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary parent directory is created");
    let path = directory.path().join("private");
    ensure_owned_private_directory(&path, "test directory").expect("private directory is created");

    assert_eq!(
        fs::metadata(path)
            .expect("private directory metadata is available")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}
