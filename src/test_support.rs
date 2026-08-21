//! Shared filesystem fixtures for unit tests.

use std::fs;
use std::path::Path;

/// Creates one uniquely named temporary directory outside the project workspace.
pub(crate) fn test_dir(name: &str) -> tempfile::TempDir {
    let root = Path::new("/tmp/agents");
    fs::create_dir_all(root).expect("temporary root creation succeeds");
    tempfile::Builder::new()
        .prefix(&format!("silo-{name}-"))
        .tempdir_in(root)
        .expect("temporary directory creation succeeds")
}
