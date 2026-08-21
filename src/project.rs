use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::apple::mount_argument_path;
use crate::digest::hex as hex_digest;
use crate::image::runtime_contract::CONTAINER_HOME;

pub(super) const PROJECT_MARKER: &str = ".silo.toml";
pub(super) const CONTAINER_NAME_PREFIX: &str = "silo-";
pub(super) const PROJECT_DIGEST_HEX_LEN: usize = 24;
/// Stable identity and container path for the current project.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Project {
    pub(crate) root: PathBuf,
    pub(super) workdir: PathBuf,
    pub(super) id: String,
}

impl Project {
    /// Discovers the current project through symlinks before deriving its ID.
    pub(crate) fn current() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to determine current directory")?;
        Self::from_path(&cwd)
    }

    /// Canonicalizes a starting directory and discovers its project root.
    pub(super) fn from_path(cwd: &Path) -> Result<Self> {
        Self::from_root(project_root_from_path(cwd)?)
    }

    /// Applies container-specific validation after config-independent project
    /// root discovery has completed.
    pub(super) fn from_root(root: PathBuf) -> Result<Self> {
        let workdir = validated_project_workdir(&root)?;
        let id = project_container_id(&root);
        Ok(Self { root, workdir, id })
    }
}

/// Discovers the current project root without requiring it to be a valid
/// container mount. Config-only commands use this lighter-weight path.
///
/// # Errors
///
/// Returns an error when the current directory cannot be read or resolved.
pub(crate) fn current_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    project_root_from_path(&cwd)
}

pub(super) fn project_root_from_path(cwd: &Path) -> Result<PathBuf> {
    let cwd = fs::canonicalize(cwd)
        .with_context(|| format!("failed to resolve project directory `{}`", cwd.display()))?;
    Ok(discover_project_root(&cwd))
}

/// Selects a project root from an already-canonical starting directory.
///
/// An explicit Silo marker takes precedence over Git's own root discovery.
/// Without either, the exact starting directory is the project.
pub(super) fn discover_project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(PROJECT_MARKER).is_file())
        .map(Path::to_path_buf)
        .or_else(|| git_project_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// Delegates repository semantics, including Gitfiles, to Git itself.
fn git_project_root(cwd: &Path) -> Option<PathBuf> {
    let output = git_root_command(cwd).output().ok()?;
    output
        .status
        .success()
        .then(|| git_root_from_stdout(cwd, &output.stdout))?
}

fn git_root_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"]);
    command
}

/// Accepts only one absolute, canonical directory that contains the caller.
fn git_root_from_stdout(cwd: &Path, stdout: &[u8]) -> Option<PathBuf> {
    let bytes = stdout.strip_suffix(b"\n").unwrap_or(stdout);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.is_empty() || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return None;
    }
    let root = fs::canonicalize(PathBuf::from(OsString::from_vec(bytes.to_vec()))).ok()?;
    if !root.is_dir() || !cwd.starts_with(&root) || validated_project_workdir(&root).is_err() {
        return None;
    }
    Some(root)
}

/// Validates the host root and its derived container working directory.
fn validated_project_workdir(root: &Path) -> Result<PathBuf> {
    let workdir = shared_dir_name(root)?;
    mount_argument_path(root)?;
    mount_argument_path(&workdir)?;
    Ok(workdir)
}

/// Returns the deterministic shared ID derived from a canonical project path.
pub(super) fn project_container_id(project: &Path) -> String {
    let digest = project_digest(project);
    format!(
        "{CONTAINER_NAME_PREFIX}{}",
        &digest[..PROJECT_DIGEST_HEX_LEN]
    )
}

/// Computes the full project digest stored in runtime and state metadata.
pub(super) fn project_digest(project: &Path) -> String {
    hex_digest(Sha256::digest(project.as_os_str().as_bytes()))
}

/// Returns where the shared project directory lands in the container.
pub(super) fn shared_dir_name(project_root: &Path) -> Result<PathBuf> {
    let name = project_root.file_name().ok_or_else(|| {
        anyhow!(
            "cannot share the root directory `{}`",
            project_root.display()
        )
    })?;
    Ok(Path::new(CONTAINER_HOME).join(name))
}

#[cfg(test)]
mod tests;
