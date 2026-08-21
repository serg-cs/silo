//! Project identity metadata for managed state directories.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};

use super::PROJECT_ROOT_METADATA;
use crate::storage::open_owned_file;

/// Creates the project identity file once or verifies the protected existing file.
pub(super) fn ensure_project_metadata(project_dir: &Path, project: &Path) -> Result<()> {
    let path = project_dir.join(PROJECT_ROOT_METADATA);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let stored = read_metadata_bytes(&path)?;
            if stored != project.as_os_str().as_bytes() {
                return Err(anyhow!(
                    "refusing to use project state directory `{}` because its project metadata does not match `{}`",
                    project_dir.display(),
                    project.display()
                ));
            }
            return Ok(());
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "could not inspect project state metadata `{}`",
                    path.display()
                )
            });
        }
    }

    // Publish the exact path bytes atomically after durable creation.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = project_dir.join(format!(
        "{PROJECT_ROOT_METADATA}.tmp.{}-{nonce}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "could not create project state metadata `{}`",
                    temporary.display()
                )
            })?;
        file.write_all(project.as_os_str().as_bytes())
            .with_context(|| {
                format!(
                    "could not write project state metadata `{}`",
                    temporary.display()
                )
            })?;
        file.sync_all().with_context(|| {
            format!(
                "could not sync project state metadata `{}`",
                temporary.display()
            )
        })?;
        fs::rename(&temporary, &path).with_context(|| {
            format!(
                "could not publish project state metadata `{}`",
                path.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Reads and validates the project path stored for one state directory.
pub(super) fn read_project_metadata(project_dir: &Path) -> Result<PathBuf> {
    let path = project_dir.join(PROJECT_ROOT_METADATA);
    let project = PathBuf::from(OsString::from_vec(read_metadata_bytes(&path)?));
    if !project.is_absolute() {
        return Err(anyhow!(
            "project state metadata `{}` does not contain an absolute path",
            path.display()
        ));
    }
    Ok(project)
}

/// Reads through the validated descriptor so path replacement cannot redirect I/O.
fn read_metadata_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut file = open_owned_file(path, 0o600, "project state metadata")?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("could not read project state metadata `{}`", path.display()))?;
    Ok(bytes)
}
