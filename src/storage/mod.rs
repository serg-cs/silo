//! Shared application-state storage and cross-process locking.

pub(crate) mod managed;

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Exclusive advisory lock released automatically with its file descriptor.
/// Its backing path must remain stable rather than being deleted after use.
#[must_use = "the lock is released when its guard is dropped"]
pub(crate) struct Lock {
    _file: fs::File,
}

impl Lock {
    /// Locks one stable file inside an already protected directory.
    pub(crate) fn acquire(path: &Path, description: &str) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("could not open {description} lock `{}`", path.display()))?;
        protect_file(&file, path, 0o600, &format!("{description} lock"))?;
        file.lock()
            .with_context(|| format!("could not lock {description}"))?;
        Ok(Self { _file: file })
    }

    /// Creates a private lock root and locks its permanent `.lock` file.
    pub(crate) fn acquire_in(root: &Path, description: &str) -> Result<Self> {
        ensure_private_directory(root, &format!("{description} lock root"))?;
        Self::acquire(&root.join(".lock"), description)
    }
}

/// Resolves Silo's state root from explicit environment values.
pub(crate) fn state_root(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    let state_home = xdg_state_home
        .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
                .map(|path| path.join(".local/state"))
        })
        .ok_or_else(|| anyhow!("Silo state requires XDG_STATE_HOME or HOME to be absolute"))?;
    Ok(state_home.join("silo"))
}

/// Resolves Silo's state root from the current process environment.
pub(crate) fn state_root_from_env() -> Result<PathBuf> {
    state_root(
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

/// Creates or repairs a private directory owned by the current user.
pub(crate) fn ensure_private_directory(path: &Path, description: &str) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("could not create {description} `{}`", path.display()))?;
    protect_directory(path, description)
}

/// Atomically claims a predictable directory beneath a shared parent.
pub(crate) fn ensure_owned_private_directory(path: &Path, description: &str) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not create {description} `{}`", path.display()));
        }
    }
    protect_directory(path, description)
}

/// Opens and protects an existing regular file without following a final symlink.
pub(crate) fn open_owned_file(path: &Path, mode: u32, description: &str) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("could not open {description} `{}`", path.display()))?;
    protect_file(&file, path, mode, description)?;
    Ok(file)
}

/// Validates and protects the inode represented by an open descriptor.
pub(crate) fn protect_file(
    file: &fs::File,
    path: &Path,
    mode: u32,
    description: &str,
) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("could not inspect {description} `{}`", path.display()))?;
    if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.nlink() != 1 {
        return Err(anyhow!(
            "refusing to use unsafe {description} `{}`",
            path.display()
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("could not protect {description} `{}`", path.display()))
}

fn protect_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {description} `{}`", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != effective_uid()
    {
        return Err(anyhow!(
            "refusing to use unsafe {description} `{}`",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect {description} `{}`", path.display()))
}

pub(crate) fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests;
