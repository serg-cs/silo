//! Container inventory presentation and safe administration.

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read};
use std::process::{ChildStdout, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use super::inventory::{
    ContainerInfo, container_inventory, delete_stopped_container, revalidate_selected_container,
    shared_container_info,
};
use super::runtime::{CONFLICT_RETRY_INTERVAL, CONFLICT_RETRY_TIMEOUT, ContainerLifecycle};
use crate::apple::{
    CONTAINER_BIN, ContainerState, container_not_found, force_delete_container, inspect_container,
    spawn_error,
};
use crate::image::runtime_contract::LIFECYCLE;
use crate::output::{tsv_field, write_json, write_stdout};
use crate::project::Project;

#[derive(Serialize)]
struct ContainerOutput<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    lifecycle: &'static str,
    state: &'static str,
    project: Cow<'a, str>,
}

/// Prints the core identity and state of every owned container.
pub(crate) fn print_containers(json: bool) -> Result<ExitCode> {
    let inventory = container_inventory()?;
    if json {
        write_json(&container_output(&inventory.items))?;
    } else {
        let text = if inventory.items.is_empty() {
            "No Silo containers.\n".to_string()
        } else {
            format!("{}\n", render_container_list(&inventory.items))
        };
        write_stdout(&text)?;
    }
    for warning in inventory.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}
/// Stops and deletes the selected owned container.
pub(crate) fn delete_selected_container(selector: &str, force: bool) -> Result<ExitCode> {
    let inventory = container_inventory()?;
    let container = select_container(&inventory.items, selector)?;
    stop_selected_container(container, force)?;
    if force {
        if revalidate_selected_container(container)?.is_some() {
            force_delete_container(&container.id)?;
        }
    } else {
        delete_stopped_container(container)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Stops the current project's shared container without selecting other projects.
pub(crate) fn stop_project_container(project: &Project, force: bool) -> Result<ExitCode> {
    let Some(inspection) = inspect_container(&project.id)? else {
        return Ok(ExitCode::SUCCESS);
    };
    let container = shared_container_info(project, &inspection)?;
    stop_selected_container(&container, force)?;
    Ok(ExitCode::SUCCESS)
}

fn render_container_list(items: &[ContainerInfo]) -> String {
    let mut lines = vec!["ID\tTYPE\tSTATE\tPROJECT".to_string()];
    lines.extend(items.iter().map(|item| {
        format!(
            "{}\t{}\t{}\t{}",
            item.id,
            item.lifecycle.as_str(),
            item.state.as_str(),
            tsv_field(&item.project.to_string_lossy())
        )
    }));
    lines.join("\n")
}

fn container_output(items: &[ContainerInfo]) -> Vec<ContainerOutput<'_>> {
    items
        .iter()
        .map(|item| ContainerOutput {
            id: &item.id,
            lifecycle: item.lifecycle.as_str(),
            state: item.state.as_str(),
            project: item.project.to_string_lossy(),
        })
        .collect()
}
/// Selects an exact ID or one unambiguous project basename.
fn select_container<'a>(items: &'a [ContainerInfo], selector: &str) -> Result<&'a ContainerInfo> {
    if let Some(item) = items.iter().find(|item| item.id == selector) {
        return Ok(item);
    }
    let mut matches = items.iter().filter(|item| {
        item.project
            .file_name()
            .is_some_and(|name| name == selector)
    });
    let selected = matches
        .next()
        .ok_or_else(|| anyhow!("no Silo container matches `{selector}`"))?;
    if matches.next().is_some() {
        return Err(anyhow!(
            "selector `{selector}` is ambiguous because multiple containers have that project name"
        ));
    }
    Ok(selected)
}
fn stop_selected_container(container: &ContainerInfo, force: bool) -> Result<()> {
    let Some(inspection) = revalidate_selected_container(container)? else {
        return Ok(());
    };
    match inspection.state {
        ContainerState::Running if force => stop_runtime_container(container),
        ContainerState::Running if container.lifecycle == ContainerLifecycle::Isolated => {
            Err(anyhow!(
                "container `{}` is an active isolated session; use `--force` to terminate it",
                container.id
            ))
        }
        ContainerState::Running => guarded_stop(container),
        ContainerState::Stopped => Ok(()),
        ContainerState::Stopping => wait_until_stopped(container),
    }
}

fn guarded_stop(container: &ContainerInfo) -> Result<()> {
    let id = &container.id;
    let mut guard = stop_guard_command(id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let input = guard
        .stdin
        .take()
        .context("stop guard stdin was not piped")?;
    let stdout = guard
        .stdout
        .take()
        .context("stop guard stdout was not piped")?;
    let ready = match read_guard_readiness(stdout, CONFLICT_RETRY_TIMEOUT) {
        Ok(ready) => ready,
        Err(err) => {
            drop(input);
            let _ = guard.kill();
            let _ = guard.wait();
            return Err(err.context(format!(
                "could not establish the stop guard for container `{id}`"
            )));
        }
    };
    if ready.trim() != "ready" {
        drop(input);
        let status = guard.wait().context("failed to wait for stop guard")?;
        let mut stderr = String::new();
        if let Some(mut pipe) = guard.stderr.take() {
            pipe.read_to_string(&mut stderr)
                .context("failed to read stop guard error")?;
        }
        return match status.code() {
            Some(75) => Err(anyhow!(
                "container `{id}` became active while stopping; retry after its sessions finish or use `--force`"
            )),
            Some(76) => Err(anyhow!(
                "container `{id}` has a session starting; retry after handoff finishes or use `--force`"
            )),
            _ => Err(anyhow!(
                "could not guard container `{id}`: {}",
                stderr.trim()
            )),
        };
    }

    if revalidate_selected_container(container)?.is_none() {
        drop(input);
        let _ = guard.wait();
        return Ok(());
    }
    let output = Command::new(CONTAINER_BIN)
        .args(["stop", id])
        .output()
        .map_err(spawn_error)?;
    drop(input);
    let _ = guard.wait();
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to stop container `{id}`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Bounds the guest handshake so an incompatible helper cannot hang cleanup.
fn read_guard_readiness(stdout: ChildStdout, timeout: Duration) -> Result<String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut ready = String::new();
        let result = BufReader::new(stdout)
            .read_line(&mut ready)
            .map(|_| ready)
            .context("failed to read stop guard readiness");
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(anyhow!("stop guard did not become ready")),
        Err(RecvTimeoutError::Disconnected) => {
            Err(anyhow!("stop guard readiness reader exited unexpectedly"))
        }
    }
}

/// Runs the guard embedded in the current host binary.
fn stop_guard_command(id: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command
        .args(["exec", "--user", "silo", id, "sh", "-c"])
        .arg(LIFECYCLE)
        .arg("silo-lifecycle")
        .arg("stop-guard");
    command
}

fn stop_runtime_container(container: &ContainerInfo) -> Result<()> {
    let Some(inspection) = revalidate_selected_container(container)? else {
        return Ok(());
    };
    if inspection.state == ContainerState::Stopped {
        return Ok(());
    }
    let id = &container.id;
    let output = Command::new(CONTAINER_BIN)
        .args(["stop", id])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() || container_not_found(&stderr, id) {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to stop container `{id}`: {}",
            stderr.trim()
        ))
    }
}

fn wait_until_stopped(container: &ContainerInfo) -> Result<()> {
    let id = &container.id;
    let deadline = Instant::now() + CONFLICT_RETRY_TIMEOUT;
    loop {
        let Some(inspection) = revalidate_selected_container(container)? else {
            return Ok(());
        };
        match inspection.state {
            ContainerState::Stopped => return Ok(()),
            ContainerState::Stopping => {}
            ContainerState::Running => {
                return Err(anyhow!(
                    "container `{id}` left the stopping state; retry or use `--force`"
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "container `{id}` did not stop within {} seconds",
                CONFLICT_RETRY_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(CONFLICT_RETRY_INTERVAL);
    }
}

#[cfg(test)]
mod tests;
