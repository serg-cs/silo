//! Shared interaction with Apple's `container` command.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

pub(crate) const CONTAINER_BIN: &str = "container";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerState {
    Running,
    Stopped,
    Stopping,
}

impl ContainerState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Stopping => "stopping",
        }
    }
}

/// Runtime fields needed to join or safely manage an inspected container.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ContainerInspection {
    pub(crate) state: ContainerState,
    pub(crate) ipv4_address: Option<Ipv4Addr>,
    pub(crate) labels: HashMap<String, String>,
    pub(crate) image_digest: Option<String>,
    pub(crate) mount_sources: Vec<PathBuf>,
}

/// Current machine-readable shape emitted by Apple Container inspection.
#[derive(Deserialize)]
struct InspectItem {
    id: String,
    configuration: InspectConfiguration,
    status: InspectStatus,
}

#[derive(Deserialize)]
struct InspectConfiguration {
    id: String,
    labels: HashMap<String, String>,
    image: InspectImage,
    mounts: Vec<InspectMount>,
}

#[derive(Deserialize)]
struct InspectImage {
    descriptor: InspectDescriptor,
}

#[derive(Deserialize)]
struct InspectDescriptor {
    digest: String,
}

#[derive(Deserialize)]
struct InspectMount {
    source: String,
}

#[derive(Deserialize)]
struct InspectStatus {
    state: String,
    networks: Vec<InspectNetwork>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InspectNetwork {
    ipv4_address: String,
}

#[derive(Deserialize)]
struct ContainerListItem {
    id: String,
}

/// Returns a path suitable for Apple's comma-delimited `--mount` syntax.
pub(crate) fn mount_argument_path(path: &Path) -> Result<&str> {
    path.to_str()
        .filter(|value| !value.contains([',', '=', '\n', '\r']))
        .ok_or_else(|| {
            anyhow!(
                "cannot mount `{}`: the path must be valid UTF-8 without `,`, `=`, or newlines",
                path.display()
            )
        })
}

/// Recognizes the runtime's documented system-not-started diagnostic.
pub(crate) fn system_not_started(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    stderr.contains("container system service has been started")
        && stderr.contains("container system start")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemStart {
    NotNeeded,
    Started,
    Failed,
}

/// Starts the runtime only when `stderr` carries its stopped-system hint.
pub(crate) fn start_system_for_error(stderr: &str, start: impl FnOnce() -> bool) -> SystemStart {
    if !system_not_started(stderr) {
        return SystemStart::NotNeeded;
    }
    if start() {
        SystemStart::Started
    } else {
        SystemStart::Failed
    }
}

/// Runs one read-only probe and repeats it once after successfully starting a
/// stopped runtime. Ordinary misses and failed starts preserve the first
/// result for the caller to classify.
pub(crate) fn probe_with_system_start<T>(
    mut probe: impl FnMut() -> Result<(Option<T>, String)>,
    start: impl FnOnce() -> bool,
) -> Result<(Option<T>, String)> {
    let first = probe()?;
    if first.0.is_none() && start_system_for_error(&first.1, start) == SystemStart::Started {
        return probe();
    }
    Ok(first)
}

/// Starts Apple's container system and forwards its output to the user.
pub(crate) fn start_container_system() -> bool {
    Command::new(CONTAINER_BIN)
        .args(["system", "start"])
        .status()
        .is_ok_and(|status| status.success())
}

/// Inspects one container, starting the runtime once when necessary.
pub(crate) fn inspect_container(id: &str) -> Result<Option<ContainerInspection>> {
    let (inspection, stderr) =
        probe_with_system_start(|| probe_container(id), start_container_system)?;
    inspection_result(id, inspection, &stderr)
}

fn inspection_result(
    id: &str,
    inspection: Option<ContainerInspection>,
    stderr: &str,
) -> Result<Option<ContainerInspection>> {
    if inspection.is_some() || container_not_found(stderr, id) {
        Ok(inspection)
    } else {
        Err(anyhow!("could not inspect container `{id}`: {stderr}"))
    }
}

/// Runs one raw container inspection; not-found is represented as `None`.
fn probe_container(id: &str) -> Result<(Option<ContainerInspection>, String)> {
    let output = Command::new(CONTAINER_BIN)
        .args(["inspect", id])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        return Ok((
            Some(parse_container_inspection(&output.stdout, id)?),
            stderr,
        ));
    }
    Ok((None, stderr))
}

/// Parses the current Apple Container JSON shape without accepting incomplete
/// mount inventory that could make managed-state deletion unsafe.
fn parse_container_inspection(stdout: &[u8], id: &str) -> Result<ContainerInspection> {
    let items: Vec<InspectItem> =
        serde_json::from_slice(stdout).context("invalid container inspect JSON")?;
    let item = items
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| anyhow!("container inspect did not return `{id}`"))?;
    ensure!(
        item.configuration.id == id,
        "container inspect returned inconsistent IDs `{}` and `{}`",
        item.id,
        item.configuration.id
    );
    let state = match item.status.state.as_str() {
        "running" => ContainerState::Running,
        "stopped" => ContainerState::Stopped,
        "stopping" => ContainerState::Stopping,
        status => return Err(anyhow!("container `{id}` has unsupported state `{status}`")),
    };
    let ipv4_address = item
        .status
        .networks
        .into_iter()
        .map(|network| {
            let address = network.ipv4_address.split('/').next().unwrap_or_default();
            address.parse().with_context(|| {
                format!(
                    "container inspect returned invalid IPv4 address `{}` for `{id}`",
                    network.ipv4_address
                )
            })
        })
        .next()
        .transpose()?;
    let image_digest = (!item.configuration.image.descriptor.digest.is_empty())
        .then_some(item.configuration.image.descriptor.digest);
    let mount_sources = item
        .configuration
        .mounts
        .into_iter()
        .map(|mount| {
            ensure!(
                !mount.source.is_empty(),
                "container inspect returned a mount without a source for `{id}`"
            );
            Ok(PathBuf::from(mount.source))
        })
        .collect::<Result<_>>()?;
    Ok(ContainerInspection {
        state,
        ipv4_address,
        labels: item.configuration.labels,
        image_digest,
        mount_sources,
    })
}

/// Lists every runtime container ID, starting the runtime once when necessary.
pub(crate) fn list_container_ids() -> Result<Vec<String>> {
    let (ids, stderr) = probe_with_system_start(probe_container_ids, start_container_system)?;
    if let Some(ids) = ids {
        return Ok(ids);
    }
    Err(anyhow!("could not list containers: {stderr}"))
}

fn probe_container_ids() -> Result<(Option<Vec<String>>, String)> {
    let output = Command::new(CONTAINER_BIN)
        .args(["list", "--all", "--format", "json"])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok((Some(parse_container_ids(&output.stdout)?), stderr))
    } else {
        Ok((None, stderr))
    }
}

/// Parses container IDs from the runtime's JSON list response.
fn parse_container_ids(stdout: &[u8]) -> Result<Vec<String>> {
    let items: Vec<ContainerListItem> =
        serde_json::from_slice(stdout).context("invalid container list JSON")?;
    Ok(items.into_iter().map(|item| item.id).collect())
}

/// Reports whether any runtime container still references a mount source.
pub(crate) fn container_mount_source_in_use(path: &Path) -> Result<bool> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("could not resolve mount source `{}`", path.display()))?;
    for id in list_container_ids()? {
        let Some(inspection) = inspect_container(&id)? else {
            continue;
        };
        if inspection.mount_sources.iter().any(|source| {
            source == path || fs::canonicalize(source).is_ok_and(|source| source == canonical)
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Matches only the runtime's explicit missing-container diagnostic.
pub(crate) fn container_not_found(stderr: &str, id: &str) -> bool {
    let message = stderr
        .trim()
        .strip_prefix("Error: ")
        .unwrap_or_else(|| stderr.trim());
    message == format!("container not found: {id}")
}

/// Deletes one stopped container, treating an already-removed instance as success.
pub(crate) fn delete_container(id: &str) -> Result<()> {
    delete_container_with(id, false)
}

/// Force-deletes one container, treating an already-removed instance as success.
pub(crate) fn force_delete_container(id: &str) -> Result<()> {
    delete_container_with(id, true)
}

fn delete_container_with(id: &str, force: bool) -> Result<()> {
    let mut command = Command::new(CONTAINER_BIN);
    command.arg("delete");
    if force {
        command.arg("--force");
    }
    command.arg(id);
    let output = command.output().map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if delete_succeeded(output.status, &stderr, id) {
        return Ok(());
    }
    let force = if force { " --force" } else { "" };
    Err(anyhow!(
        "`{CONTAINER_BIN} delete{force} {id}` failed: {}",
        stderr.trim()
    ))
}

/// Returns whether the delete result means the container is already gone.
fn delete_succeeded(status: ExitStatus, stderr: &str, id: &str) -> bool {
    status.success() || container_not_found(stderr, id)
}

pub(crate) fn execute(command: &mut Command) -> Result<ExitCode> {
    let status = command.status().map_err(spawn_error)?;
    Ok(exit_code(status))
}

pub(crate) fn exit_code(status: ExitStatus) -> ExitCode {
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
        .clamp(0, 255);
    ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
}

pub(crate) fn spawn_error(err: io::Error) -> anyhow::Error {
    if err.kind() == io::ErrorKind::NotFound {
        anyhow!(
            "`{CONTAINER_BIN}` not found on PATH. \
             Install it from https://github.com/apple/container/releases"
        )
    } else {
        anyhow!(err).context(format!("failed to run `{CONTAINER_BIN}`"))
    }
}

#[cfg(test)]
mod tests;
