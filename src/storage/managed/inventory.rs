//! Managed-state discovery and validation.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::Result;

use super::{
    ManagedMount, StateOwner, managed_mount_at_root, managed_mount_root, read_project_metadata,
};
use crate::config::valid_mount_name;
use crate::digest::is_sha256_hex;
use crate::project::project_digest;

#[derive(Default)]
pub(super) struct MountInventory {
    pub(super) items: Vec<ManagedMount>,
    pub(super) warnings: Vec<String>,
}

pub(super) fn mount_inventory() -> Result<MountInventory> {
    let root = managed_mount_root(
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?;
    let mut inventory = MountInventory::default();
    if !inventory_root_is_directory(root.parent().unwrap_or(&root), "Silo state", &mut inventory) {
        return Ok(inventory);
    }
    let discovered = mount_inventory_at(&root);
    inventory.items = discovered.items;
    inventory.warnings.extend(discovered.warnings);
    inventory.warnings.sort();
    Ok(inventory)
}

pub(super) fn mount_inventory_at(root: &Path) -> MountInventory {
    let mut inventory = MountInventory::default();
    if !inventory_root_is_directory(root, "managed state", &mut inventory) {
        return inventory;
    }
    collect_user_state(root, &mut inventory);
    collect_project_mounts(root, &mut inventory);
    inventory.items.sort_by(|left, right| {
        left.owner
            .cmp(&right.owner)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    inventory.warnings.sort();
    inventory
}

fn inventory_root_is_directory(
    root: &Path,
    description: &str,
    inventory: &mut MountInventory,
) -> bool {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            inventory.warnings.push(format!(
                "ignored {description} root `{}` because it is not a real directory",
                root.display()
            ));
            false
        }
        Ok(_) => true,
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => {
            inventory.warnings.push(format!(
                "could not inspect {description} root `{}`: {err}",
                root.display()
            ));
            false
        }
    }
}

fn collect_user_state(root: &Path, inventory: &mut MountInventory) {
    let user = root.join("user");
    if !inventory_root_is_directory(&user, "user state", inventory) {
        return;
    }
    for entry in read_inventory_directory(&user, "user state", inventory) {
        let Some(name) = inventory_mount_name(&entry, "user", inventory) else {
            continue;
        };
        let mount = managed_mount_at_root(StateOwner::User, &name, root);
        inventory.items.push(mount);
    }
}

fn collect_project_mounts(root: &Path, inventory: &mut MountInventory) {
    let projects = root.join("project");
    if !inventory_root_is_directory(&projects, "project state", inventory) {
        return;
    }
    for entry in read_inventory_directory(&projects, "project state", inventory) {
        let Some(digest) = entry.file_name().to_str().map(ToString::to_string) else {
            inventory
                .warnings
                .push("ignored project state directory with a non-UTF-8 digest".to_string());
            continue;
        };
        if !is_sha256_hex(&digest) {
            inventory.warnings.push(format!(
                "ignored invalid project state directory `{digest}`"
            ));
            continue;
        }
        if !inventory_entry_is_directory(&entry, "project state directory", inventory) {
            continue;
        }
        let project_dir = entry.path();
        let project = match read_project_metadata(&project_dir) {
            Ok(project) if project_digest(&project) == digest => project,
            Ok(_) => {
                inventory.warnings.push(format!(
                    "ignored project state directory `{digest}` because its metadata does not match its digest"
                ));
                continue;
            }
            Err(err) => {
                inventory.warnings.push(format!(
                    "ignored project state directory `{digest}`: {err:#}"
                ));
                continue;
            }
        };
        let mounts = project_dir.join("entries");
        if !inventory_root_is_directory(&mounts, "project state collection", inventory) {
            continue;
        }
        for mount_entry in read_inventory_directory(&mounts, "project state", inventory) {
            let Some(name) = inventory_mount_name(&mount_entry, "project", inventory) else {
                continue;
            };
            let mount = managed_mount_at_root(StateOwner::Project(project.clone()), &name, root);
            inventory.items.push(mount);
        }
    }
}

fn read_inventory_directory(
    root: &Path,
    description: &str,
    inventory: &mut MountInventory,
) -> Vec<fs::DirEntry> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => {
            inventory.warnings.push(format!(
                "could not read {description} directory `{}`: {err}",
                root.display()
            ));
            return Vec::new();
        }
    };
    let mut collected = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => collected.push(entry),
            Err(err) => {
                inventory
                    .warnings
                    .push(format!("could not inspect a {description} entry: {err}"));
            }
        }
    }
    collected
}

fn inventory_mount_name(
    entry: &fs::DirEntry,
    scope: &str,
    inventory: &mut MountInventory,
) -> Option<String> {
    let Some(name) = entry.file_name().to_str().map(ToString::to_string) else {
        inventory
            .warnings
            .push(format!("ignored {scope} mount with a non-UTF-8 name"));
        return None;
    };
    if !valid_mount_name(&name) {
        inventory
            .warnings
            .push(format!("ignored invalid {scope} mount entry `{name}`"));
        return None;
    }
    inventory_entry_is_directory(entry, &format!("{scope} mount `{name}`"), inventory)
        .then_some(name)
}

fn inventory_entry_is_directory(
    entry: &fs::DirEntry,
    description: &str,
    inventory: &mut MountInventory,
) -> bool {
    let file_type = match entry.file_type() {
        Ok(file_type) => file_type,
        Err(err) => {
            inventory
                .warnings
                .push(format!("could not inspect {description}: {err}"));
            return false;
        }
    };
    if file_type.is_symlink() || !file_type.is_dir() {
        inventory.warnings.push(format!(
            "ignored {description} because it is not a real directory"
        ));
        false
    } else {
        true
    }
}
