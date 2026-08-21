//! Shared and isolated container lifecycle orchestration.

use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::config::{Config, Container, Shell};
use crate::digest::hex as hex_digest;
use crate::host_ports;

use super::inventory::{
    delete_stopped_container, is_owned_isolated, shared_container_info, silo_metadata,
};
use super::mounts::{ConfigMounts, ensure_managed_mounts, needs_mount_lock, resolve_config_mounts};
use super::process::{SavedTerminal, install_signal_handlers, wait_for_child};
use super::runtime::{
    CONFLICT_RETRY_INTERVAL, CONFLICT_RETRY_TIMEOUT, ContainerLifecycle, GUEST_READY_TIMEOUT,
    HostIds, LaunchSpec, exec_command, host_ids, isolated_container_id, isolated_create_command,
    isolated_owner_pid, isolated_start_command, owner_alive, resolve_shell, shared_create_command,
};
use crate::apple::{
    CONTAINER_BIN, ContainerInspection, ContainerState, exit_code, force_delete_container,
    inspect_container, list_container_ids, spawn_error,
};
use crate::image;
use crate::image::runtime_contract::{GUEST_READY_PATH, LIFECYCLE_COMMAND};
use crate::project::Project;
use crate::storage::managed::acquire_mount_lock;

/// Runs a command in this project's shared container, or in a one-shot
/// foreground container when `isolated` is set. Every selected image shares
/// the base runtime contract, so lifecycle and feature behavior is independent
/// of whether its development tools came from the default or a custom layer.
///
/// # Errors
///
/// Returns an error when setup, state inspection, container creation, or the
/// attached process fails.
pub(crate) fn run_session(
    config: &Config,
    project: &Project,
    command: &[OsString],
    isolated: bool,
) -> Result<ExitCode> {
    let shell = resolve_shell(config.shell, std::env::var_os("SHELL").as_deref());
    if isolated {
        let image = image::reference(config)?;
        image::require_digest(&image)?;
        if !config.host_ports.is_empty() {
            eprintln!(
                "warning: host ports require a shared container; ignoring host ports for this isolated run"
            );
        }
        return run_isolated(config, project, command, shell, &image);
    }
    run_shared(config, project, command, shell)
}

/// Runs an ephemeral create/start lifecycle with the same image and storage
/// contract as shared mode.
fn run_isolated(
    config: &Config,
    project: &Project,
    command: &[OsString],
    shell: Shell,
    image: &str,
) -> Result<ExitCode> {
    let ids = host_ids();
    let id = isolated_container_id();
    let instance = instance_token(project);
    // Build the command first: it only fails on path validation, before any
    // container exists.
    let config_mounts = resolve_config_mounts(config, &project.root, None)?;
    let mount_lock = needs_mount_lock(&config_mounts.configured)
        .then(acquire_mount_lock)
        .transpose()?;
    ensure_managed_mounts(&config_mounts.configured)?;
    let launch = LaunchSpec {
        project,
        id: &id,
        image,
        instance: &instance,
        host_ids: &ids,
        mounts: &config_mounts,
        resources: &config.container,
        lifecycle: ContainerLifecycle::Isolated,
    };
    let mut create =
        isolated_create_command(std::io::stdin().is_terminal(), &launch, command, shell)?;
    sweep_orphaned_isolated_containers(Some(&id));
    create.stdout(Stdio::null());
    let create_status = create.status().map_err(spawn_error)?;
    drop(mount_lock);
    if !create_status.success() {
        cleanup_isolated_container(&id, project, &instance);
        return Ok(exit_code(create_status));
    }
    if let Err(err) = install_signal_handlers() {
        cleanup_isolated_container(&id, project, &instance);
        return Err(err);
    }
    // Captured before the child starts, so it holds the pre-raw-mode state.
    let terminal = SavedTerminal::capture();
    let mut start = isolated_start_command(&id);
    let mut child = match start.spawn() {
        Ok(child) => child,
        Err(err) => {
            cleanup_isolated_container(&id, project, &instance);
            return Err(spawn_error(err));
        }
    };
    let status = wait_for_child(&mut child);
    if let Some(terminal) = &terminal {
        terminal.restore();
    }
    cleanup_isolated_container(&id, project, &instance);
    status.map(exit_code)
}

/// Best-effort cleanup for isolated containers whose owning process is gone.
fn sweep_orphaned_isolated_containers(current_id: Option<&str>) {
    let ids = match list_container_ids() {
        Ok(ids) => ids,
        Err(err) => {
            eprintln!("warning: could not enumerate isolated containers: {err:#}");
            return;
        }
    };
    for id in ids {
        let Some(owner) = isolated_owner_pid(&id) else {
            continue;
        };
        if current_id != Some(id.as_str()) && owner_alive(owner) {
            continue;
        }
        let Ok(Some(inspection)) = inspect_container(&id) else {
            continue;
        };
        if !is_owned_isolated(&id, &inspection) {
            continue;
        }
        if let Err(err) = force_delete_container(&id) {
            eprintln!("warning: could not remove orphaned isolated container `{id}`: {err:#}");
        }
    }
}

/// Deletes only the isolated container created by this exact invocation.
fn cleanup_isolated_container(id: &str, project: &Project, instance: &str) {
    if let Err(err) = cleanup_isolated_container_with(
        id,
        project,
        instance,
        inspect_container,
        force_delete_container,
    ) {
        eprintln!("warning: {err:#}");
    }
}

/// Executes cleanup through replaceable runtime operations after ownership proof.
pub(super) fn cleanup_isolated_container_with(
    id: &str,
    project: &Project,
    instance: &str,
    inspect: impl FnOnce(&str) -> Result<Option<ContainerInspection>>,
    delete: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    let inspection = inspect(id).with_context(|| {
        format!("could not verify isolated container `{id}` for cleanup; leaving it untouched")
    })?;
    let Some(inspection) = inspection else {
        return Ok(());
    };
    if !is_owned_isolated_instance(id, &inspection, project, instance) {
        return Ok(());
    }
    delete(id).with_context(|| format!("could not remove isolated container `{id}`"))
}

fn is_owned_isolated_instance(
    id: &str,
    inspection: &ContainerInspection,
    project: &Project,
    instance: &str,
) -> bool {
    silo_metadata(id, inspection).is_some_and(|(lifecycle, stored_project, stored_instance)| {
        lifecycle == ContainerLifecycle::Isolated
            && stored_project == project.root
            && stored_instance == instance
    })
}

#[derive(Debug)]
pub(super) enum SharedCreation {
    Created,
    Retry(anyhow::Error),
}

/// Ensures the shared project container and attaches one exec session.
fn run_shared(
    config: &Config,
    project: &Project,
    command: &[OsString],
    shell: Shell,
) -> Result<ExitCode> {
    sweep_orphaned_isolated_containers(None);
    let deadline = Instant::now() + GUEST_READY_TIMEOUT + CONFLICT_RETRY_TIMEOUT;
    let (reservation, address, tunnel) = loop {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "container `{}` repeatedly stopped during session handoff",
                project.id
            ));
        }
        let tunnel = ensure_shared_container(project, config)?;
        let requires_address = tunnel.is_some();
        let Some(inspection) = wait_for_guest_ready(project, requires_address)? else {
            continue;
        };
        let Some(reservation) = reserve_shared_session(project)? else {
            continue;
        };
        break (reservation, inspection.ipv4_address, tunnel);
    };
    // Only the creator owns prepared host-port assets; joins leave the
    // running instance unchanged.
    if let Some(tunnel) = tunnel {
        let address = address.ok_or_else(|| {
            anyhow!(
                "container `{}` did not expose an IPv4 address for host ports",
                project.id
            )
        })?;
        tunnel.ensure(address)?;
    }

    // The attached process owns the terminal, but not the shared container.
    let mut exec = exec_command(
        std::io::stdin().is_terminal(),
        project,
        &reservation,
        command,
        shell,
    );
    install_signal_handlers()?;
    let terminal = SavedTerminal::capture();
    let mut child = exec.spawn().map_err(spawn_error)?;
    let status = wait_for_child(&mut child);
    if let Some(terminal) = &terminal {
        terminal.restore();
    }
    status.map(exit_code)
}

/// Waits until the shared container reports readiness and its inspection is usable.
fn wait_for_guest_ready(
    project: &Project,
    require_address: bool,
) -> Result<Option<ContainerInspection>> {
    let deadline = Instant::now() + GUEST_READY_TIMEOUT;
    loop {
        let output = guest_ready_command(project).output().map_err(spawn_error)?;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let Some(inspection) = inspect_container(&project.id)? else {
            return Ok(None);
        };
        match inspection.state {
            ContainerState::Running => validate_shared_ownership(&inspection, project)?,
            ContainerState::Stopped | ContainerState::Stopping => {
                return Ok(None);
            }
        }
        if output.status.success() && (!require_address || inspection.ipv4_address.is_some()) {
            return Ok(Some(inspection));
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "container `{}` did not publish guest readiness and networking within {} seconds: {}",
                project.id,
                GUEST_READY_TIMEOUT.as_secs(),
                stderr
            ));
        }
        thread::sleep(CONFLICT_RETRY_INTERVAL);
    }
}

fn guest_ready_command(project: &Project) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command
        .args(["exec", "--user", "silo"])
        .arg(&project.id)
        .args(["test", "-e", GUEST_READY_PATH]);
    command
}

/// Establishes a transient guest-side reservation before the user command is
/// submitted. If PID 1 wins the idle race, only this safe helper is retried;
/// the arbitrary user command is never replayed.
fn reserve_shared_session(project: &Project) -> Result<Option<String>> {
    let deadline = Instant::now() + CONFLICT_RETRY_TIMEOUT;
    loop {
        let output = session_reserve_command(project)
            .output()
            .map_err(spawn_error)?;
        if output.status.success() {
            let reservation = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if reservation.is_empty()
                || !reservation.bytes().all(|byte| byte.is_ascii_alphanumeric())
            {
                return Err(anyhow!(
                    "container `{}` returned an invalid session reservation",
                    project.id
                ));
            }
            return Ok(Some(reservation));
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let Some(inspection) = inspect_container(&project.id)? else {
            return Ok(None);
        };
        match inspection.state {
            ContainerState::Running => validate_shared_ownership(&inspection, project)?,
            ContainerState::Stopped | ContainerState::Stopping => {
                return Ok(None);
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "failed to reserve a session in container `{}`: {}",
                project.id,
                stderr
            ));
        }
        thread::sleep(CONFLICT_RETRY_INTERVAL);
    }
}

pub(super) fn session_reserve_command(project: &Project) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command
        .args(["exec", "--user", "silo"])
        .arg(&project.id)
        .arg(LIFECYCLE_COMMAND)
        .arg("reserve");
    command
}

fn instance_token(project: &Project) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = Sha256::new();
    hasher.update(project.root.as_os_str().as_bytes());
    hasher.update(std::process::id().to_be_bytes());
    hasher.update(timestamp.to_be_bytes());
    hex_digest(hasher.finalize())
}

/// Ensures the deterministic shared container exists and is safe to join.
/// Running owned containers remain usable without rebuilding current creation
/// settings; absent and stopped containers are reconciled.
fn ensure_shared_container(
    project: &Project,
    config: &Config,
) -> Result<Option<host_ports::Tunnel>> {
    let deadline = Instant::now() + CONFLICT_RETRY_TIMEOUT;
    let mut last_conflict = None;

    loop {
        let inspection = inspect_container(&project.id)?;
        match inspection {
            None => {
                let host_ports = host_ports::prepare(&config.host_ports, &project.root)?;
                let ids = host_ids();
                let config_mounts =
                    resolve_config_mounts(config, &project.root, host_ports.as_ref())?;
                let image = image::reference(config)?;
                let image_digest = image::require_digest(&image)?;
                let instance = instance_token(project);
                let mount_lock = needs_mount_lock(&config_mounts.configured)
                    .then(acquire_mount_lock)
                    .transpose()?;
                ensure_managed_mounts(&config_mounts.configured)?;
                let creation = create_shared_container(
                    project,
                    &image,
                    &image_digest,
                    &ids,
                    &config_mounts,
                    &config.container,
                    &instance,
                );
                drop(mount_lock);
                match creation? {
                    SharedCreation::Created => return Ok(host_ports),
                    SharedCreation::Retry(err) => last_conflict = Some(err),
                }
            }
            Some(inspection) if inspection.state == ContainerState::Running => {
                validate_shared_ownership(&inspection, project)?;
                return Ok(None);
            }
            Some(inspection) if inspection.state == ContainerState::Stopped => {
                validate_shared_ownership(&inspection, project)?;
                let container = shared_container_info(project, &inspection)?;
                match delete_stopped_container(&container) {
                    Ok(()) => continue,
                    Err(err) => {
                        last_conflict = Some(err.context(format!(
                            "could not remove stopped container '{}' before recreating it",
                            project.id
                        )));
                    }
                }
            }
            Some(_) => {}
        }

        if Instant::now() >= deadline {
            return Err(last_conflict.unwrap_or_else(|| {
                anyhow!(
                    "container '{}' did not reach a usable state within {} seconds",
                    project.id,
                    CONFLICT_RETRY_TIMEOUT.as_secs()
                )
            }));
        }
        thread::sleep(CONFLICT_RETRY_INTERVAL);
    }
}

/// Creates one detached shared container and verifies its published identity.
fn create_shared_container(
    project: &Project,
    image: &str,
    image_digest: &str,
    ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
    instance: &str,
) -> Result<SharedCreation> {
    let cid_dir = tempfile::Builder::new()
        .prefix("silo-cid-")
        .tempdir_in(std::env::temp_dir())
        .context("failed to create temporary cid directory")?;
    let cidfile = cid_dir.path().join("container.cid");
    let current_digest = image::require_digest(image)?;
    if current_digest != image_digest {
        return Ok(SharedCreation::Retry(anyhow!(
            "image `{image}` changed while creating container '{}'; retrying",
            project.id
        )));
    }
    let launch = LaunchSpec {
        project,
        id: &project.id,
        image,
        instance,
        host_ids: ids,
        mounts: config_mounts,
        resources,
        lifecycle: ContainerLifecycle::Shared,
    };
    let output = shared_create_command(&launch, &cidfile)?
        .output()
        .map_err(spawn_error)?;

    if !output.status.success() {
        cleanup_partial_creation(project, instance, &cidfile);
        let error = anyhow!(
            "failed to create shared container '{}': {}",
            project.id,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return classify_failed_creation(project, instance, inspect_container(&project.id)?, error);
    }

    let recorded = match fs::read_to_string(&cidfile) {
        Ok(recorded) => recorded,
        Err(err) => {
            cleanup_owned_creation(project, instance);
            return Err(err)
                .with_context(|| format!("failed to read cidfile '{}'", cidfile.display()));
        }
    };
    if recorded.trim() != project.id {
        cleanup_owned_creation(project, instance);
        return Err(anyhow!(
            "container runtime wrote unexpected ID '{}' to '{}'",
            recorded.trim(),
            cidfile.display()
        ));
    }
    let inspection = inspect_container(&project.id)?
        .ok_or_else(|| anyhow!("container '{}' disappeared after creation", project.id))?;
    if inspection.image_digest.as_deref() != Some(image_digest) {
        cleanup_partial_creation(project, instance, &cidfile);
        return Ok(SharedCreation::Retry(anyhow!(
            "container '{}' was created from a different image than `{image}`; retrying",
            project.id,
        )));
    }
    validate_shared_ownership(&inspection, project)?;
    if inspection
        .labels
        .get(super::runtime::LABEL_INSTANCE)
        .map(String::as_str)
        != Some(instance)
    {
        return Ok(SharedCreation::Retry(anyhow!(
            "container '{}' was created by a competing Silo invocation; retrying",
            project.id
        )));
    }
    Ok(SharedCreation::Created)
}

/// A failed create is retryable only when another valid shared instance won
/// the deterministic container name concurrently.
pub(super) fn classify_failed_creation(
    project: &Project,
    instance: &str,
    inspection: Option<ContainerInspection>,
    error: anyhow::Error,
) -> Result<SharedCreation> {
    let Some(inspection) = inspection else {
        return Err(error);
    };
    validate_shared_ownership(&inspection, project)?;
    if inspection
        .labels
        .get(super::runtime::LABEL_INSTANCE)
        .is_some_and(|stored| stored == instance)
    {
        return Err(error);
    }
    Ok(SharedCreation::Retry(error))
}

/// Deletes a failed creation only if this invocation's cidfile and the
/// runtime's ownership labels independently confirm the target.
fn cleanup_partial_creation(project: &Project, instance: &str, cidfile: &Path) {
    if !fs::read_to_string(cidfile).is_ok_and(|value| value.trim() == project.id) {
        return;
    }
    cleanup_owned_creation(project, instance);
}

/// Reconciles successful create calls whose cidfile publication was incomplete,
/// using the invocation-specific instance label as the deletion proof.
fn cleanup_owned_creation(project: &Project, instance: &str) {
    let owned = inspect_container(&project.id).is_ok_and(|inspection| {
        inspection.is_some_and(|inspection| {
            validate_shared_instance(&inspection, project, instance).is_ok()
        })
    });
    if owned && let Err(err) = force_delete_container(&project.id) {
        eprintln!(
            "warning: could not remove partially created container '{}': {err:#}",
            project.id
        );
    }
}

/// Refuses to adopt containers that are not unambiguously owned by Silo.
pub(super) fn validate_shared_ownership(
    inspection: &ContainerInspection,
    project: &Project,
) -> Result<()> {
    let Some((ContainerLifecycle::Shared, stored_project, _)) =
        silo_metadata(&project.id, inspection)
    else {
        return Err(anyhow!(
            "refusing to manage container '{}': ownership labels are invalid",
            project.id
        ));
    };
    if stored_project != project.root {
        return Err(anyhow!(
            "refusing to manage container '{}': project ownership does not match",
            project.id
        ));
    }
    Ok(())
}

fn validate_shared_instance(
    inspection: &ContainerInspection,
    project: &Project,
    instance: &str,
) -> Result<()> {
    validate_shared_ownership(inspection, project)?;
    if inspection
        .labels
        .get(super::runtime::LABEL_INSTANCE)
        .map(String::as_str)
        != Some(instance)
    {
        return Err(anyhow!(
            "container '{}' was created by a competing Silo invocation; retrying",
            project.id
        ));
    }
    Ok(())
}
