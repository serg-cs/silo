use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::apple::{ContainerInspection, ContainerState};
use crate::config::{Bind, Config, Container, Permission, Shell, StateEntry};
use crate::project::{Project, project_container_id};
use crate::storage::managed::{ManagedMount, StateOwner, managed_mount_at_root};
use crate::test_support::test_dir;

use super::inventory::*;
use super::lifecycle::*;
use super::mounts::*;
use super::runtime::*;
use crate::image::runtime_contract::*;

const TEST_IMAGE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TEST_INSTANCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TEST_IMAGE: &str = "silo:latest";

fn isolated_create_command(
    config_mounts: &ConfigMounts,
    resources: &Container,
    command: &[OsString],
    shell: Shell,
) -> Result<Command> {
    isolated_create_command_with_env(config_mounts, resources, &BTreeSet::new(), command, shell)
}

fn isolated_create_command_with_env(
    config_mounts: &ConfigMounts,
    resources: &Container,
    env_vars: &BTreeSet<String>,
    command: &[OsString],
    shell: Shell,
) -> Result<Command> {
    let project = test_project("/tmp/project");
    let host_ids = HostIds {
        uid: "501".into(),
        gid: "20".into(),
    };
    let launch = LaunchSpec {
        project: &project,
        id: "silo-test",
        image: TEST_IMAGE,
        instance: TEST_INSTANCE,
        host_ids: &host_ids,
        mounts: config_mounts,
        resources,
        env_vars,
        lifecycle: ContainerLifecycle::Isolated,
    };
    super::runtime::isolated_create_command(false, &launch, command, shell)
}

fn create_command(
    project: &Project,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
    cidfile: &Path,
) -> Result<Command> {
    create_command_with_env(
        project,
        host_ids,
        config_mounts,
        resources,
        &BTreeSet::new(),
        cidfile,
    )
}

fn create_command_with_env(
    project: &Project,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
    env_vars: &BTreeSet<String>,
    cidfile: &Path,
) -> Result<Command> {
    let launch = LaunchSpec {
        project,
        id: &project.id,
        image: TEST_IMAGE,
        instance: TEST_INSTANCE,
        host_ids,
        mounts: config_mounts,
        resources,
        env_vars,
        lifecycle: ContainerLifecycle::Shared,
    };
    shared_create_command(&launch, cidfile)
}

fn configured_host(host: &str, dest: &str, access: Permission) -> ConfiguredMount {
    ConfiguredMount {
        source: MountSource::Host(PathBuf::from(host)),
        dest: PathBuf::from(dest),
        access,
    }
}

fn state_entry(target: impl Into<PathBuf>) -> StateEntry {
    StateEntry {
        target: target.into(),
    }
}

fn read_only_path(host: &str, relative: &str) -> ReadOnlyProjectPath {
    ReadOnlyProjectPath {
        host: PathBuf::from(host),
        relative: PathBuf::from(relative),
    }
}

fn test_managed_mount(owner: StateOwner, name: &str) -> ManagedMount {
    managed_mount_at_root(owner, name, Path::new("/home/user/.local/state/silo/state"))
}

fn args_without_labels(command: &Command) -> Vec<&str> {
    let mut arguments = command
        .get_args()
        .map(|argument| argument.to_str().expect("argument is UTF-8"));
    let mut filtered = Vec::new();
    while let Some(argument) = arguments.next() {
        if matches!(argument, "--label" | "--user" | "--entrypoint") {
            arguments.next().expect("paired option has a value");
        } else {
            filtered.push(argument);
        }
    }
    filtered
}

fn command_labels(command: &Command) -> HashMap<&str, &str> {
    let arguments: Vec<&str> = command
        .get_args()
        .map(|argument| argument.to_str().expect("argument is UTF-8"))
        .collect();
    arguments
        .windows(2)
        .filter(|pair| pair[0] == "--label")
        .map(|pair| pair[1].split_once('=').expect("label has key and value"))
        .collect()
}

fn mount_specs(command: &Command) -> Vec<String> {
    let args: Vec<&str> = command
        .get_args()
        .map(|arg| arg.to_str().expect("arg is UTF-8"))
        .collect();
    args.windows(2)
        .filter(|pair| pair[0] == "--mount")
        .map(|pair| pair[1].to_string())
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("path resolves")
}

fn test_project(root: &str) -> Project {
    let root = PathBuf::from(root);
    Project {
        workdir: Path::new(CONTAINER_HOME).join(root.file_name().expect("project has a name")),
        id: project_container_id(&root),
        root,
    }
}

fn inspection(project: &Project, lifecycle: ContainerLifecycle) -> ContainerInspection {
    ContainerInspection {
        state: ContainerState::Running,
        ipv4_address: Some(Ipv4Addr::new(192, 168, 64, 2)),
        labels: HashMap::from([
            (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
            (
                LABEL_PROJECT_ROOT.to_string(),
                project.root.display().to_string(),
            ),
            (LABEL_LIFECYCLE.to_string(), lifecycle.as_str().to_string()),
            (LABEL_INSTANCE.to_string(), TEST_INSTANCE.to_string()),
        ]),
        image_digest: Some(TEST_IMAGE_DIGEST.to_string()),
        mount_sources: Vec::new(),
    }
}

#[cfg(target_os = "linux")]
mod guest_lifecycle;
mod mounts;
mod runtime;
