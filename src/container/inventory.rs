//! Runtime container discovery, ownership snapshots, and revalidation.

use std::path::PathBuf;

use anyhow::{Result, anyhow};

use crate::apple::{
    ContainerInspection, ContainerState, delete_container, inspect_container, list_container_ids,
};
use crate::digest::is_sha256_hex;
use crate::project::{Project, project_container_id};

use super::runtime::{
    ContainerLifecycle, LABEL_INSTANCE, LABEL_ISOLATED_VALUE, LABEL_LIFECYCLE, LABEL_OWNER,
    LABEL_OWNER_VALUE, LABEL_PROJECT_ROOT, LABEL_SHARED_VALUE, isolated_owner_pid,
};

/// Owned runtime identity retained while a management action is selected.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ContainerInfo {
    pub(super) id: String,
    pub(super) lifecycle: ContainerLifecycle,
    pub(super) state: ContainerState,
    pub(super) project: PathBuf,
    pub(super) instance: String,
}

/// Valid containers and non-fatal discovery diagnostics.
#[derive(Default)]
pub(super) struct ContainerInventory {
    pub(super) items: Vec<ContainerInfo>,
    pub(super) warnings: Vec<String>,
}

/// Parses the labels that prove Silo owns a specific runtime ID.
pub(super) fn silo_metadata(
    id: &str,
    inspection: &ContainerInspection,
) -> Option<(ContainerLifecycle, PathBuf, String)> {
    let labels = &inspection.labels;
    if labels.get(LABEL_OWNER).map(String::as_str) != Some(LABEL_OWNER_VALUE) {
        return None;
    }
    let lifecycle = match labels.get(LABEL_LIFECYCLE).map(String::as_str) {
        Some(LABEL_SHARED_VALUE) => ContainerLifecycle::Shared,
        Some(LABEL_ISOLATED_VALUE) => ContainerLifecycle::Isolated,
        _ => return None,
    };
    let project = PathBuf::from(labels.get(LABEL_PROJECT_ROOT)?);
    if !project.is_absolute() {
        return None;
    }
    let valid_id = match lifecycle {
        ContainerLifecycle::Shared => project_container_id(&project) == id,
        ContainerLifecycle::Isolated => isolated_owner_pid(id).is_some(),
    };
    if !valid_id {
        return None;
    }
    let instance = labels
        .get(LABEL_INSTANCE)
        .filter(|instance| is_sha256_hex(instance))?
        .clone();
    Some((lifecycle, project, instance))
}

pub(super) fn shared_container_info(
    project: &Project,
    inspection: &ContainerInspection,
) -> Result<ContainerInfo> {
    let Some((ContainerLifecycle::Shared, stored_project, instance)) =
        silo_metadata(&project.id, inspection)
    else {
        return Err(anyhow!(
            "refusing to manage container '{}' because its ownership labels are invalid",
            project.id
        ));
    };
    if stored_project != project.root {
        return Err(anyhow!(
            "refusing to manage container '{}' because it belongs to a different project",
            project.id
        ));
    }
    Ok(ContainerInfo {
        id: project.id.clone(),
        lifecycle: ContainerLifecycle::Shared,
        state: inspection.state,
        project: project.root.clone(),
        instance,
    })
}

pub(super) fn revalidate_selected_container(
    container: &ContainerInfo,
) -> Result<Option<ContainerInspection>> {
    let Some(inspection) = inspect_container(&container.id)? else {
        return Ok(None);
    };
    validate_selected_ownership(container, &inspection)?;
    Ok(Some(inspection))
}

pub(super) fn validate_selected_ownership(
    container: &ContainerInfo,
    inspection: &ContainerInspection,
) -> Result<()> {
    let Some((lifecycle, project, instance)) = silo_metadata(&container.id, inspection) else {
        return Err(anyhow!(
            "refusing to manage container `{}` because its ownership labels changed",
            container.id
        ));
    };
    if lifecycle != container.lifecycle
        || project != container.project
        || instance != container.instance
    {
        return Err(anyhow!(
            "refusing to manage container `{}` because it is no longer the selected Silo instance",
            container.id
        ));
    }
    Ok(())
}

pub(super) fn container_inventory() -> Result<ContainerInventory> {
    let mut inventory = ContainerInventory::default();
    for id in list_container_ids()? {
        let inspection = match inspect_container(&id) {
            Ok(Some(inspection)) => inspection,
            Ok(None) => continue,
            Err(err) => {
                inventory
                    .warnings
                    .push(format!("could not inspect container `{id}`: {err:#}"));
                continue;
            }
        };
        let Some((lifecycle, project, instance)) = silo_metadata(&id, &inspection) else {
            continue;
        };
        inventory.items.push(ContainerInfo {
            id,
            lifecycle,
            state: inspection.state,
            project,
            instance,
        });
    }
    inventory.items.sort_by(|left, right| {
        left.project
            .cmp(&right.project)
            .then_with(|| left.lifecycle.cmp(&right.lifecycle))
            .then_with(|| left.id.cmp(&right.id))
    });
    inventory.warnings.sort();
    Ok(inventory)
}

pub(super) fn delete_stopped_container(container: &ContainerInfo) -> Result<()> {
    let Some(inspection) = revalidate_selected_container(container)? else {
        return Ok(());
    };
    if inspection.state != ContainerState::Stopped {
        return Err(anyhow!(
            "container `{}` is no longer stopped and was not deleted",
            container.id
        ));
    }
    delete_container(&container.id)
}

pub(super) fn is_owned_isolated(id: &str, inspection: &ContainerInspection) -> bool {
    silo_metadata(id, inspection)
        .is_some_and(|(lifecycle, _, _)| lifecycle == ContainerLifecycle::Isolated)
}
