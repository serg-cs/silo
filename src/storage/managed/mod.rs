//! Managed-state identity, storage, locking, and filesystem safety.

mod inventory;
mod management;
mod metadata;

use inventory::mount_inventory;
#[cfg(test)]
use inventory::{MountInventory, mount_inventory_at};
pub(crate) use management::{delete_selected_state, print_state};
use metadata::{ensure_project_metadata, read_project_metadata};

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::digest::hex as hex_digest;
use crate::storage::{Lock, ensure_private_directory, state_root, state_root_from_env};

use crate::project::{PROJECT_DIGEST_HEX_LEN, project_digest};

pub(crate) const MANAGED_STATE_ID_PREFIX: &str = "silo-state-";
pub(crate) const PROJECT_ROOT_METADATA: &str = ".project-root";
pub(crate) const STATE_PROJECT_VALUE: &str = "project";
pub(crate) const STATE_USER_VALUE: &str = "user";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ManagedMount {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) owner: StateOwner,
    pub(crate) path: PathBuf,
}

pub(crate) fn acquire_mount_lock() -> Result<Lock> {
    let root = state_root_from_env()?;
    ensure_private_directory(&root, "Silo state root")?;
    Lock::acquire_in(&root.join("state"), "managed state")
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StateOwner {
    Project(PathBuf),
    User,
}

impl StateOwner {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Project(_) => STATE_PROJECT_VALUE,
            Self::User => STATE_USER_VALUE,
        }
    }

    pub(crate) fn project(&self) -> Option<&Path> {
        match self {
            Self::Project(project) => Some(project),
            Self::User => None,
        }
    }
}

pub(crate) fn ensure_managed_mount(mount: &ManagedMount) -> Result<()> {
    let root = managed_mount_root_from_path(&mount.path, &mount.owner)?;
    ensure_private_directory(root, "managed state root")?;
    match &mount.owner {
        StateOwner::User => {
            ensure_private_directory(&root.join("user"), "user state root")?;
        }
        StateOwner::Project(project) => {
            let projects = root.join("project");
            let project_dir = projects.join(project_digest(project));
            ensure_private_directory(&projects, "project state root")?;
            ensure_private_directory(&project_dir, "project state directory")?;
            ensure_project_metadata(&project_dir, project)?;
            ensure_private_directory(&project_dir.join("entries"), "project state collection")?;
        }
    }
    ensure_private_directory(&mount.path, "managed state directory")?;
    Ok(())
}

fn managed_mount_root_from_path<'a>(path: &'a Path, owner: &StateOwner) -> Result<&'a Path> {
    let levels = match owner {
        StateOwner::User => 2,
        StateOwner::Project(_) => 4,
    };
    path.ancestors()
        .nth(levels)
        .ok_or_else(|| anyhow!("managed state `{}` has no storage root", path.display()))
}

fn managed_mount_root(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    Ok(state_root(xdg_state_home, home)
        .context("managed state requires XDG_STATE_HOME or HOME to be absolute")?
        .join("state"))
}

pub(crate) fn managed_mount(
    owner: StateOwner,
    name: &str,
    xdg_state_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<ManagedMount> {
    let root = managed_mount_root(xdg_state_home, home)?;
    Ok(managed_mount_at_root(owner, name, &root))
}

pub(crate) fn managed_mount_at_root(owner: StateOwner, name: &str, root: &Path) -> ManagedMount {
    let name_digest = hex_digest(Sha256::digest(name.as_bytes()));
    let id = match &owner {
        StateOwner::Project(project) => format!(
            "{MANAGED_STATE_ID_PREFIX}p-{}-{}",
            &project_digest(project)[..PROJECT_DIGEST_HEX_LEN],
            &name_digest[..16]
        ),
        StateOwner::User => {
            format!("{MANAGED_STATE_ID_PREFIX}u-{}", &name_digest[..24])
        }
    };
    let path = match &owner {
        StateOwner::Project(project) => root
            .join("project")
            .join(project_digest(project))
            .join("entries")
            .join(name),
        StateOwner::User => root.join("user").join(name),
    };
    ManagedMount {
        id,
        name: name.to_string(),
        owner,
        path,
    }
}

fn validate_managed_mount(mount: &ManagedMount) -> Result<()> {
    let root = managed_mount_root_from_path(&mount.path, &mount.owner)?;
    validate_real_directory(root, "managed state root")?;
    if let StateOwner::Project(project) = &mount.owner {
        validate_real_directory(&root.join("project"), "project state root")?;
        let project_dir = mount
            .path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("project state `{}` has no project directory", mount.id))?;
        validate_real_directory(project_dir, "project state directory")?;
        validate_real_directory(
            mount.path.parent().unwrap_or(&mount.path),
            "project state collection",
        )?;
        let stored = read_project_metadata(project_dir)?;
        if stored.as_path() != project
            || OsStr::new(&project_digest(&stored)) != project_dir.file_name().unwrap_or_default()
        {
            return Err(anyhow!(
                "refusing to use project state `{}` because its project metadata is inconsistent",
                mount.id
            ));
        }
    } else {
        validate_real_directory(&root.join("user"), "user state root")?;
    }
    validate_real_directory(&mount.path, "managed state directory")?;
    Ok(())
}

fn validate_real_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {description} `{}`", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "refusing to use {description} `{}` because it is not a real directory",
            path.display()
        ));
    }
    Ok(())
}

fn prune_empty_project_state_directory(path: &Path) -> Result<()> {
    let mounts_dir = path
        .parent()
        .ok_or_else(|| anyhow!("deleted project state has no entry collection"))?;
    if fs::read_dir(mounts_dir)?.next().is_some() {
        return Ok(());
    }
    let project_dir = mounts_dir
        .parent()
        .ok_or_else(|| anyhow!("deleted project state has no project directory"))?;
    let mut unexpected = Vec::new();
    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != OsStr::new("entries") && name != OsStr::new(PROJECT_ROOT_METADATA) {
            unexpected.push(name);
        }
    }
    if !unexpected.is_empty() {
        return Ok(());
    }
    fs::remove_dir(mounts_dir).with_context(|| {
        format!(
            "could not remove empty project state collection `{}`",
            mounts_dir.display()
        )
    })?;
    fs::remove_file(project_dir.join(PROJECT_ROOT_METADATA)).with_context(|| {
        format!(
            "could not remove project state metadata `{}`",
            project_dir.display()
        )
    })?;
    fs::remove_dir(project_dir).with_context(|| {
        format!(
            "could not remove empty project state directory `{}`",
            project_dir.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests;
