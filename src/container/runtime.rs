//! Container identity and command construction above Apple's runtime adapter.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, ensure};

use super::mounts::{ConfigMounts, MountSource, project_path_target};
use crate::apple::{CONTAINER_BIN, mount_argument_path};
use crate::config::{Container, Permission, Shell};
use crate::image::runtime_contract::{
    BASH_PATH, CONTAINER_HOME, FISH_PATH, LIFECYCLE_COMMAND, NU_PATH, ZSH_PATH,
    append_runtime_contract,
};
use crate::project::{CONTAINER_NAME_PREFIX, Project};

const DEFAULT_SHELL: Shell = Shell::Zsh;

pub(super) const LABEL_OWNER: &str = "dev.silo.owner";
pub(super) const LABEL_PROJECT_ROOT: &str = "dev.silo.project-root";
pub(super) const LABEL_LIFECYCLE: &str = "dev.silo.lifecycle";
pub(super) const LABEL_INSTANCE: &str = "dev.silo.instance";
pub(super) const LABEL_OWNER_VALUE: &str = "silo";
pub(super) const LABEL_SHARED_VALUE: &str = "shared";
pub(super) const LABEL_ISOLATED_VALUE: &str = "isolated";
/// Runtime races are retried only for this bounded interval.
pub(super) const CONFLICT_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const CONFLICT_RETRY_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const GUEST_READY_TIMEOUT: Duration = Duration::from_mins(1);

pub(super) fn validate_config(resources: &Container, env_vars: &BTreeSet<String>) -> Result<()> {
    ensure!(
        resources.cpus != Some(0),
        "invalid `container.cpus` config option: CPU count must be greater than zero"
    );
    ensure!(
        !resources
            .memory
            .as_ref()
            .is_some_and(|memory| memory.trim().is_empty()),
        "invalid `container.memory` config option: memory must not be empty"
    );
    for name in env_vars {
        ensure!(
            valid_env_var_name(name),
            "invalid `env_vars` entry `{name}`: expected an ASCII shell variable name"
        );
        ensure!(
            !matches!(name.as_str(), "HOME" | "PATH" | "SHELL" | "BREW_PREFIX")
                && !name.starts_with("SILO_"),
            "invalid `env_vars` entry `{name}`: the name is reserved by Silo"
        );
    }
    Ok(())
}

fn valid_env_var_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ContainerLifecycle {
    Shared,
    Isolated,
}

impl ContainerLifecycle {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Shared => LABEL_SHARED_VALUE,
            Self::Isolated => LABEL_ISOLATED_VALUE,
        }
    }
}

/// Host user and group ids, forwarded to the container so its `silo` user can
/// be remapped to them and keep the shared directory writable on both sides.
pub(super) struct HostIds {
    pub(super) uid: String,
    pub(super) gid: String,
}

/// Creation inputs shared by isolated and project-scoped containers.
pub(super) struct LaunchSpec<'a> {
    pub(super) project: &'a Project,
    pub(super) id: &'a str,
    pub(super) image: &'a str,
    pub(super) instance: &'a str,
    pub(super) host_ids: &'a HostIds,
    pub(super) mounts: &'a ConfigMounts,
    pub(super) resources: &'a Container,
    pub(super) env_vars: &'a BTreeSet<String>,
    pub(super) lifecycle: ContainerLifecycle,
}

pub(super) fn isolated_container_id() -> String {
    format!("{CONTAINER_NAME_PREFIX}{}", std::process::id())
}

/// Adds the minimal ownership record needed to safely manage one instance.
fn append_identity_labels(run: &mut Command, project: &Project, lifecycle: &str, instance: &str) {
    for (key, value) in [
        (LABEL_OWNER, LABEL_OWNER_VALUE),
        (LABEL_PROJECT_ROOT, project.root.to_string_lossy().as_ref()),
        (LABEL_LIFECYCLE, lifecycle),
        (LABEL_INSTANCE, instance),
    ] {
        run.arg("--label").arg(format!("{key}={value}"));
    }
}

pub(super) fn isolated_owner_pid(id: &str) -> Option<libc::pid_t> {
    id.strip_prefix(CONTAINER_NAME_PREFIX)?
        .parse()
        .ok()
        .filter(|pid| *pid > 0)
}

pub(super) fn owner_alive(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Returns the host user's uid and gid, forwarded to the container so files
/// created there are owned by the host user.
///
/// # Errors
///
/// Returns an error when `id` cannot be run or one of the ids cannot be read.
pub(super) fn host_ids() -> HostIds {
    // These calls have no preconditions and report the process's effective IDs.
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    HostIds {
        uid: uid.to_string(),
        gid: gid.to_string(),
    }
}

/// Builds the create half of an isolated one-shot container lifecycle.
///
/// # Errors
///
/// Returns an error when the project root has no name (e.g. `/`), or its path
/// or a mount's path cannot be expressed in a mount specification.
pub(super) fn isolated_create_command(
    interactive: bool,
    launch: &LaunchSpec<'_>,
    command: &[OsString],
    shell: Shell,
) -> Result<Command> {
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("create")
        .arg("--name")
        .arg(launch.id)
        .arg("--rm")
        .arg("-i");
    if interactive {
        // Allocating a pty without a terminal fails with ENOTTY.
        run.arg("-t");
    }
    append_launch_contract(&mut run, launch)?;
    run.arg("--env").arg(format!("SHELL={}", shell.path()));
    run.arg(launch.image);
    if command.is_empty() {
        run.arg(shell.path());
    } else {
        run.args(command);
    }
    Ok(run)
}

pub(super) fn isolated_start_command(id: &str) -> Command {
    let mut start = Command::new(CONTAINER_BIN);
    start.arg("start").arg("--attach").arg("--interactive");
    start.arg(id);
    start
}

/// Builds the detached creation command for a shared project container.
pub(super) fn shared_create_command(launch: &LaunchSpec<'_>, cidfile: &Path) -> Result<Command> {
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("run")
        .arg("--name")
        .arg(launch.id)
        .arg("--cidfile")
        .arg(cidfile)
        .arg("--rm")
        .arg("-d");
    append_launch_contract(&mut run, launch)?;
    // Creation verifies the tag immediately before and the resolved digest
    // immediately after this command, closing concurrent retag races.
    run.arg(launch.image);
    run.arg(LIFECYCLE_COMMAND).arg("init");
    Ok(run)
}

/// Applies the creation contract shared by both lifecycle modes.
fn append_launch_contract(run: &mut Command, launch: &LaunchSpec<'_>) -> Result<()> {
    append_resources(run, launch.resources);
    append_env_vars(run, launch.env_vars);
    append_identity_labels(
        run,
        launch.project,
        launch.lifecycle.as_str(),
        launch.instance,
    );
    append_creation_mounts(
        run,
        &launch.project.root,
        &launch.project.workdir,
        launch.mounts,
    )?;
    append_host_ids(run, launch.host_ids);
    append_runtime_contract(
        run,
        launch.resources.sudo,
        launch.mounts.host_ports.is_some(),
    );
    Ok(())
}

/// Inherits allowlisted values without placing them in Silo's arguments.
fn append_env_vars(run: &mut Command, env_vars: &BTreeSet<String>) {
    for name in env_vars {
        run.arg("--env").arg(name);
    }
}

/// Adds only explicitly configured limits, preserving Apple container's
/// configured defaults when either setting is omitted.
fn append_resources(run: &mut Command, resources: &Container) {
    if let Some(cpus) = resources.cpus {
        run.arg("--cpus").arg(cpus.to_string());
    }
    if let Some(memory) = &resources.memory {
        run.arg("--memory").arg(memory);
    }
}

/// Adds mounts in their established override order and selects the workdir.
fn append_creation_mounts(
    run: &mut Command,
    project_root: &Path,
    shared_dir: &Path,
    config_mounts: &ConfigMounts,
) -> Result<()> {
    append_bind_mount(run, project_root, shared_dir, Permission::ReadWrite)?;
    run.arg("-w").arg(shared_dir);

    // Overlay protected project paths before non-overlapping configured mounts.
    for entry in &config_mounts.read_only {
        let target = project_path_target(shared_dir, &entry.relative);
        append_bind_mount(run, &entry.host, &target, Permission::ReadOnly)?;
    }
    for entry in &config_mounts.configured {
        let source = match &entry.source {
            MountSource::Host(host) => host,
            MountSource::Managed(mount) => &mount.path,
        };
        append_bind_mount(run, source, &entry.dest, entry.access)?;
    }
    if let Some(host_ports) = &config_mounts.host_ports {
        append_bind_mount(
            run,
            &host_ports.source,
            Path::new("/run/silo-ssh"),
            Permission::ReadOnly,
        )?;
    }
    Ok(())
}

/// Adds one explicit bind mount using the runtime's structured syntax.
fn append_bind_mount(
    run: &mut Command,
    source: &Path,
    target: &Path,
    access: Permission,
) -> Result<()> {
    let source = mount_argument_path(source)?;
    let target = mount_argument_path(target)?;
    let readonly = match access {
        Permission::ReadOnly => ",readonly",
        Permission::ReadWrite => "",
    };
    run.arg("--mount").arg(format!(
        "type=bind,source={source},target={target}{readonly}"
    ));
    Ok(())
}

/// Adds only the stable IDs required by the shared container's entrypoint.
fn append_host_ids(run: &mut Command, host_ids: &HostIds) {
    run.arg("--env")
        .arg(format!("SILO_UID={}", host_ids.uid))
        .arg("--env")
        .arg(format!("SILO_GID={}", host_ids.gid));
}

/// Builds one session attachment command for the running shared container.
pub(super) fn exec_command(
    interactive: bool,
    project: &Project,
    reservation: &str,
    command: &[OsString],
    shell: Shell,
) -> Command {
    let mut exec = Command::new(CONTAINER_BIN);
    exec.arg("exec").arg("-i");
    if interactive {
        exec.arg("-t");
    }
    exec.arg("--user")
        .arg("silo")
        .arg("--workdir")
        .arg(&project.workdir);
    exec.arg("--env")
        .arg(format!("HOME={CONTAINER_HOME}"))
        .arg("--env")
        .arg(format!("SHELL={}", shell.path()));
    exec.arg(&project.id);
    exec.arg(LIFECYCLE_COMMAND).arg("session").arg(reservation);
    if command.is_empty() {
        exec.arg(shell.path());
    } else {
        exec.args(command);
    }
    exec
}

impl Shell {
    const fn path(self) -> &'static str {
        match self {
            Self::Bash => BASH_PATH,
            Self::Zsh => ZSH_PATH,
            Self::Fish => FISH_PATH,
            Self::Nu => NU_PATH,
        }
    }
}

/// Selects the configured shell, otherwise mirrors a supported host shell by
/// executable name. Unknown, missing, and non-UTF-8 host values use Zsh.
pub(super) fn resolve_shell(configured: Option<Shell>, host_shell: Option<&OsStr>) -> Shell {
    configured.unwrap_or_else(|| {
        let name = host_shell
            .and_then(|shell| Path::new(shell).file_name())
            .and_then(OsStr::to_str);
        match name {
            Some("bash") => Shell::Bash,
            Some("zsh") => Shell::Zsh,
            Some("fish") => Shell::Fish,
            Some("nu") => Shell::Nu,
            _ => DEFAULT_SHELL,
        }
    })
}
