//! Host and container interpretation of configured paths.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

/// Joins a relative host path to `project_root`.
///
/// Absolute and `~…` paths are left unchanged because they are not
/// project-relative. Empty paths are left unchanged so later validation can
/// report them rather than silently becoming `project_root`.
#[must_use]
pub(crate) fn join_project(path: &Path, project_root: &Path) -> PathBuf {
    if path.as_os_str().is_empty() || path.is_absolute() || starts_with_tilde(path) {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

/// Host path for a configured location, resolved at use rather than load.
///
/// # Errors
///
/// Returns an error when home expansion is required and cannot be performed.
pub(crate) fn resolve_host(
    path: &Path,
    project_root: &Path,
    home: Option<&Path>,
) -> Result<PathBuf> {
    expand_home(&join_project(path, project_root), home)
}

/// True unless the path is a `~user` form, which Silo never expands.
#[must_use]
pub(crate) fn is_host_source(path: &Path) -> bool {
    !starts_with_tilde(path) || path.strip_prefix("~").is_ok()
}

/// Expands a bare `~` or `~/…` against the host home directory.
///
/// # Errors
///
/// Returns an error when the path uses `~user` expansion, or when it is
/// home-relative and `home` is missing, empty, or not absolute.
pub(crate) fn expand_home(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    if !starts_with_tilde(path) {
        return Ok(path.to_path_buf());
    }
    let rest = path.strip_prefix("~").map_err(|_| {
        anyhow!(
            "path `{}` uses unsupported `~user` expansion; use `~/...` for the current user",
            path.display()
        )
    })?;
    let home = home
        .filter(|home| !home.as_os_str().is_empty() && home.is_absolute())
        .ok_or_else(|| {
            anyhow!(
                "cannot expand home-relative path `{}` because HOME is unset, empty, or not absolute",
                path.display()
            )
        })?;
    Ok(home.join(rest))
}

/// Resolves a container bind or state target.
///
/// `project_dir` is required only for `./…`. Absolute and `~/…` targets
/// resolve without it. Bare `~` is not a container target.
#[must_use]
pub(crate) fn container_target(
    path: &Path,
    project_dir: Option<&Path>,
    home: &Path,
) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.starts_with(b"~/") {
        return Some(home.join(path.strip_prefix("~").ok()?));
    }
    if bytes.starts_with(b"./") {
        return Some(project_dir?.join(path.strip_prefix(".").ok()?));
    }
    None
}

fn starts_with_tilde(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().starts_with(b"~")
}

#[cfg(test)]
mod tests;
