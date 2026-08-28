//! Managed-state inventory presentation and safe administration.

use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use super::{
    ManagedMount, StateOwner, acquire_mount_lock, managed_mount, mount_inventory,
    prune_empty_project_state_directory, validate_managed_mount,
};
use crate::apple::container_mount_source_in_use;
use crate::output::{tsv_field, write_json, write_stdout};

#[derive(Serialize)]
struct StateOutput<'a> {
    id: &'a str,
    scope: &'static str,
    name: &'a str,
    project: Option<Cow<'a, str>>,
    source: Cow<'a, str>,
}

/// Prints every persistent state directory owned by Silo.
pub(crate) fn print_state(json: bool) -> Result<ExitCode> {
    let inventory = mount_inventory()?;
    if json {
        write_json(&state_output(&inventory.items))?;
    } else {
        let text = if inventory.items.is_empty() {
            "No Silo managed state.\n".to_string()
        } else {
            format!("{}\n", render_state_list(&inventory.items))
        };
        write_stdout(&text)?;
    }
    for warning in inventory.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Permanently deletes one selected, currently unused managed state entry.
pub(crate) fn delete_selected_state(selector: &str) -> Result<ExitCode> {
    let inventory = mount_inventory()?;
    let selected = select_mount(&inventory.items, selector)?;
    let _mount_lock = acquire_mount_lock()?;
    let expected = managed_mount(
        selected.owner.clone(),
        &selected.name,
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?;
    if selected != &expected {
        return Err(anyhow!(
            "refusing to delete managed state `{}` because its path or identity is inconsistent",
            selected.id
        ));
    }
    validate_managed_mount(selected)?;
    let metadata = fs::symlink_metadata(&selected.path).with_context(|| {
        format!(
            "managed state `{}` disappeared before deletion",
            selected.id
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "refusing to delete managed state `{}` because it is not a real directory",
            selected.id
        ));
    }
    if container_mount_source_in_use(&selected.path)? {
        return Err(anyhow!(
            "could not delete managed state `{}` because a container still references it",
            selected.id
        ));
    }
    fs::remove_dir_all(&selected.path)
        .with_context(|| format!("could not delete managed state `{}`", selected.id))?;
    if matches!(&selected.owner, StateOwner::Project(_)) {
        prune_empty_project_state_directory(&selected.path)?;
    }
    Ok(ExitCode::SUCCESS)
}
fn render_state_list(items: &[ManagedMount]) -> String {
    let mut lines = vec!["ID\tSCOPE\tNAME\tPROJECT\tSOURCE".to_string()];
    lines.extend(items.iter().map(|mount| {
        let project = mount.owner.project().map_or_else(
            || "-".to_string(),
            |path| tsv_field(&path.to_string_lossy()),
        );
        format!(
            "{}\t{}\t{}\t{}\t{}",
            mount.id,
            mount.owner.as_str(),
            tsv_field(&mount.name),
            project,
            tsv_field(&mount.path.to_string_lossy())
        )
    }));
    lines.join("\n")
}

fn state_output(items: &[ManagedMount]) -> Vec<StateOutput<'_>> {
    items
        .iter()
        .map(|mount| StateOutput {
            id: &mount.id,
            scope: mount.owner.as_str(),
            name: &mount.name,
            project: mount
                .owner
                .project()
                .map(|project| project.to_string_lossy()),
            source: mount.path.to_string_lossy(),
        })
        .collect()
}
/// Selects an exact ID or one globally unique logical state name.
fn select_mount<'a>(items: &'a [ManagedMount], selector: &str) -> Result<&'a ManagedMount> {
    if let Some(item) = items.iter().find(|item| item.id == selector) {
        return Ok(item);
    }
    let mut matches = items.iter().filter(|item| item.name == selector);
    let selected = matches
        .next()
        .ok_or_else(|| anyhow!("no Silo managed state matches `{selector}`"))?;
    if matches.next().is_some() {
        return Err(anyhow!(
            "selector `{selector}` is ambiguous because multiple managed state entries have that name"
        ));
    }
    Ok(selected)
}

#[cfg(test)]
mod tests;
