//! Container mount resolution and managed-state safety.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};

use crate::apple::mount_argument_path;
use crate::config::{Config, Permission, valid_mount_name};
use crate::host_ports;

use crate::image::runtime_contract::{CONTAINER_HOME, RUNTIME_ASSETS};
use crate::project::shared_dir_name;
use crate::storage::managed::{ManagedMount, StateOwner, ensure_managed_mount, managed_mount};

/// Small set of mount destinations owned directly by Silo's lifecycle.
pub(super) const PROTECTED_RUNTIME_DIRS: &[&str] = &[
    "/etc/sudoers.d",
    "/run/silo",
    "/run/sshd",
    "/var/run/silo",
    "/var/run/sshd",
];

/// One configured bind or managed-state mount resolved for this run.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ConfiguredMount {
    pub(super) source: MountSource,
    pub(super) dest: PathBuf,
    pub(super) access: Permission,
}

/// One project-owned path overlaid read-only after the writable project bind.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ReadOnlyProjectPath {
    pub(super) host: PathBuf,
    pub(super) relative: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum MountSource {
    Host(PathBuf),
    Managed(ManagedMount),
}

pub(super) fn ensure_managed_mounts(mounts: &[ConfiguredMount]) -> Result<()> {
    let mut managed = BTreeMap::new();
    for entry in mounts {
        if let MountSource::Managed(mount) = &entry.source {
            managed.entry(mount.path.clone()).or_insert(mount);
        }
    }
    for mount in managed.into_values() {
        ensure_managed_mount(mount)?;
    }
    Ok(())
}

pub(super) fn needs_mount_lock(mounts: &[ConfiguredMount]) -> bool {
    mounts
        .iter()
        .any(|mount| matches!(mount.source, MountSource::Managed(_)))
}

/// Creation-time project setup shared by identity and command construction.
#[derive(Default)]
pub(super) struct ConfigMounts {
    pub(super) read_only: Vec<ReadOnlyProjectPath>,
    pub(super) configured: Vec<ConfiguredMount>,
    pub(super) host_ports: Option<host_ports::SshAssets>,
}

pub(super) fn resolve_read_only_paths(
    project_root: &Path,
    paths: &[PathBuf],
) -> Vec<ReadOnlyProjectPath> {
    let Some(root) = fs::canonicalize(project_root).ok() else {
        return Vec::new();
    };

    let mut unique = BTreeSet::new();
    let mut resolved = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(relative) = normalize_read_only_path(path) else {
            continue;
        };
        if !unique.insert(relative.clone()) {
            continue;
        }
        let Some(host) = mount_host(&project_root.join(&relative), &root) else {
            continue;
        };
        if !host.is_dir() {
            continue;
        }
        resolved.push(ReadOnlyProjectPath { host, relative });
    }
    resolved
}

fn normalize_read_only_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    if path.is_absolute() {
        return None;
    }
    let text = path.to_str()?;
    if text.contains([',', '=', '\n', '\r']) {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if normalized.pop() => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    Some(normalized)
}

/// Returns the canonical path of `path` when it resolves to a real path
/// inside `root_canonical`, or `None` otherwise (a symlink pointing away
/// must not become a mount, since the container runtime would mount the
/// link's target).
pub(super) fn mount_host(path: &Path, root_canonical: &Path) -> Option<PathBuf> {
    let host = fs::canonicalize(path).ok()?;
    host.starts_with(root_canonical).then_some(host)
}

/// Resolves configured binds and state for this project. Bind sources are expanded
/// and canonicalized; managed state receives deterministic private host
/// directories. Sorting parents before children makes nested overrides stable.
///
/// # Errors
///
/// Returns an error when a mount overlaps the managed runtime or names a bind
/// whose source does not exist or cannot be resolved (e.g. a broken symlink).
pub(super) fn resolve_configured_mounts(
    config: &Config,
    project_root: &Path,
    read_only: &[ReadOnlyProjectPath],
    home: Option<&Path>,
    xdg_state_home: Option<&Path>,
) -> Result<Vec<ConfiguredMount>> {
    let project_dir = shared_dir_name(project_root)?;
    validate_config(config)?;
    let project_state = eligible_project_state(config, project_root);
    // Enforce the mount contract against the same state snapshot used below.
    validate_eligible_project_targets(config, &project_state, &project_dir, read_only)?;

    let capacity = config.binds.len() + config.state.project.len() + config.state.user.len();
    let mut resolved = Vec::with_capacity(capacity);
    for (name, bind) in &config.binds {
        let dest = effective_target(&bind.target, &project_dir)
            .ok_or_else(|| anyhow!("bind or state entry `{name}` has no valid container target"))?;
        let host = fs::canonicalize(expand_tilde(&bind.source, home)).with_context(|| {
            format!(
                "cannot resolve source `{}` for bind `{name}` at `{}` (missing path or broken symlink?)",
                bind.source.display(),
                dest.display()
            )
        })?;
        if !host.is_dir() {
            return Err(anyhow!(
                "source `{}` for bind `{name}` must be a directory",
                bind.source.display()
            ));
        }
        resolved.push(ConfiguredMount {
            source: MountSource::Host(host),
            dest,
            access: bind.access,
        });
    }
    for (name, entry) in config
        .state
        .project
        .iter()
        .filter(|(name, _)| project_state.contains(*name))
    {
        let dest = effective_target(&entry.target, &project_dir)
            .ok_or_else(|| anyhow!("state entry `{name}` has no valid container target"))?;
        let owner = StateOwner::Project(project_root.to_path_buf());
        let source = MountSource::Managed(managed_mount(owner, name, xdg_state_home, home)?);
        resolved.push(ConfiguredMount {
            source,
            dest,
            access: Permission::ReadWrite,
        });
    }
    for (name, entry) in &config.state.user {
        let dest = effective_target(&entry.target, &project_dir)
            .ok_or_else(|| anyhow!("state entry `{name}` has no valid container target"))?;
        let source =
            MountSource::Managed(managed_mount(StateOwner::User, name, xdg_state_home, home)?);
        resolved.push(ConfiguredMount {
            source,
            dest,
            access: Permission::ReadWrite,
        });
    }
    resolved.sort_by(|left, right| {
        left.dest
            .components()
            .count()
            .cmp(&right.dest.components().count())
            .then_with(|| left.dest.cmp(&right.dest))
    });
    for mount in &resolved {
        let source = match &mount.source {
            MountSource::Host(path) => path,
            MountSource::Managed(managed) => &managed.path,
        };
        mount_argument_path(source)?;
        mount_argument_path(&mount.dest)?;
    }
    Ok(resolved)
}

fn effective_target(target: &Path, project_dir: &Path) -> Option<PathBuf> {
    if target.is_absolute() {
        return Some(target.to_path_buf());
    }
    let text = target.to_str()?;
    text.strip_prefix("~/")
        .map(|relative| Path::new(CONTAINER_HOME).join(relative))
        .or_else(|| {
            text.strip_prefix("./")
                .map(|relative| project_dir.join(relative))
        })
}

/// Expands a leading `~` in `path` to the home directory: `~` and `~/x`
/// become `<home>` and `<home>/x`. Any other path (absolute, `~user`, plain
/// relative) is returned unchanged. Without a known, non-empty home
/// directory nothing is expanded, so an unset or empty `HOME` leaves the
/// path relative and the later `canonicalize` fails loudly instead of
/// resolving against the working directory.
pub(super) fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home.filter(|home| !home.as_os_str().is_empty()) else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Places a normalized project-relative path below its container project
/// root, treating `.` as the root itself rather than emitting a trailing dot.
pub(super) fn project_path_target(project_dest: &Path, relative: &Path) -> PathBuf {
    if relative == Path::new(".") {
        project_dest.to_path_buf()
    } else {
        project_dest.join(relative)
    }
}

/// Rejects malformed mount settings without consulting the filesystem.
pub(super) fn validate_config(config: &Config) -> Result<()> {
    for (name, bind) in &config.binds {
        validate_host_path(name, &bind.source)?;
    }

    let mut names = BTreeSet::new();
    let mut targets = BTreeMap::<PathBuf, &str>::new();
    for (name, target, _) in configured_mounts(config, None) {
        ensure!(
            valid_mount_name(name),
            "bind or state entry `{name}`: name must start with an ASCII letter or number and contain only ASCII letters, numbers, `_`, or `-`"
        );
        ensure!(
            names.insert(name),
            "entry `{name}` is defined in more than one bind or state category"
        );
        validate_container_path(name, target)?;
        let Some(target) = context_independent_target(target) else {
            continue;
        };
        if let Some(existing) = targets.insert(target.clone(), name) {
            return Err(anyhow!(
                "entries `{existing}` and `{name}` both target `{}`",
                target.display()
            ));
        }
        if let Some(runtime) = protected_runtime_overlap(&target) {
            return Err(anyhow!(
                "bind or state entry `{name}` targets `{}`, which overlaps Silo-managed runtime path `{}`",
                target.display(),
                runtime.display()
            ));
        }
    }

    Ok(())
}

fn context_independent_target(target: &Path) -> Option<PathBuf> {
    if target.is_absolute() {
        return Some(target.to_path_buf());
    }
    target
        .to_str()?
        .strip_prefix("~/")
        .map(|relative| Path::new(CONTAINER_HOME).join(relative))
}

/// Resolves effective targets against the actual project destination before
/// accepting duplicate or writable-overlap invariants.
pub(super) fn validate_project_targets(
    config: &Config,
    project_root: &Path,
    project_dir: &Path,
    read_only: &[ReadOnlyProjectPath],
) -> Result<()> {
    validate_config(config)?;
    let project_state = eligible_project_state(config, project_root);
    validate_eligible_project_targets(config, &project_state, project_dir, read_only)
}

pub(super) fn validate_eligible_project_targets(
    config: &Config,
    project_state: &BTreeSet<String>,
    project_dir: &Path,
    read_only: &[ReadOnlyProjectPath],
) -> Result<()> {
    let protected: Vec<_> = read_only
        .iter()
        .map(|path| project_path_target(project_dir, &path.relative))
        .collect();
    let mut targets = BTreeMap::<PathBuf, &str>::new();
    for (name, configured, access) in configured_mounts(config, Some(project_state)) {
        let target = effective_target(configured, project_dir).ok_or_else(|| {
            anyhow!("validated entry `{name}` did not resolve to a container target")
        })?;
        if let Some(existing) = targets.insert(target.clone(), name) {
            return Err(anyhow!(
                "entries `{existing}` and `{name}` both target `{}`",
                target.display()
            ));
        }
        if let Some(runtime) = protected_runtime_overlap(&target) {
            return Err(anyhow!(
                "bind or state entry `{name}` targets `{}`, which overlaps Silo-managed runtime path `{}`",
                target.display(),
                runtime.display()
            ));
        }
        if access == Permission::ReadWrite
            && let Some(workspace) = protected
                .iter()
                .find(|path| target.starts_with(path) || path.starts_with(&target))
        {
            return Err(anyhow!(
                "read-write entry `{name}` targets `{}`, which overlaps read-only workspace path `{}`",
                target.display(),
                workspace.display()
            ));
        }
    }
    Ok(())
}

pub(super) fn configured_mounts<'a>(
    config: &'a Config,
    project_state: Option<&'a BTreeSet<String>>,
) -> impl Iterator<Item = (&'a str, &'a Path, Permission)> + 'a {
    config
        .binds
        .iter()
        .map(|(name, entry)| (name.as_str(), entry.target.as_path(), entry.access))
        .chain(
            config
                .state
                .project
                .iter()
                .filter(move |(name, _)| {
                    project_state.is_none_or(|eligible| eligible.contains(*name))
                })
                .map(|(name, entry)| {
                    (name.as_str(), entry.target.as_path(), Permission::ReadWrite)
                }),
        )
        .chain(
            config.state.user.iter().map(|(name, entry)| {
                (name.as_str(), entry.target.as_path(), Permission::ReadWrite)
            }),
        )
}

pub(super) fn eligible_project_state(config: &Config, project_root: &Path) -> BTreeSet<String> {
    config
        .state
        .project
        .iter()
        .filter(|(_, entry)| project_state_target_exists(project_root, &entry.target))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Project-relative state is only useful when its mount point already exists.
/// Omitting missing paths prevents the container runtime from creating them in
/// the writable project bind. Home-relative and absolute targets are unaffected.
fn project_state_target_exists(project_root: &Path, target: &Path) -> bool {
    let Some(relative) = target.to_str().and_then(|target| target.strip_prefix("./")) else {
        return true;
    };
    project_root.join(relative).is_dir()
}

fn validate_host_path(name: &str, path: &Path) -> Result<()> {
    validate_mount_path(name, "source", path)?;
    ensure!(
        path.is_absolute() || path.starts_with("~"),
        "bind or state entry `{name}`: source path is not absolute and does not start with a bare `~`"
    );
    Ok(())
}

fn validate_container_path(name: &str, path: &Path) -> Result<()> {
    let text = validate_mount_path(name, "target", path)?;
    ensure!(
        !path
            .components()
            .any(|component| component == Component::ParentDir),
        "bind or state entry `{name}`: target path contains `..`, which can cross container symlinks"
    );
    ensure!(
        path.is_absolute() || text.starts_with("./") || text.starts_with("~/"),
        "bind or state entry `{name}`: target path must start with `./` for the project, `~/` for the container home, or `/` for an absolute location"
    );
    Ok(())
}

fn validate_mount_path<'a>(name: &str, field: &str, path: &'a Path) -> Result<&'a str> {
    ensure!(
        !path.as_os_str().is_empty(),
        "bind or state entry `{name}`: {field} path is empty"
    );
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("bind or state entry `{name}`: {field} path is not valid UTF-8"))?;
    ensure!(
        !text.contains([',', '=']),
        "bind or state entry `{name}`: {field} path contains `,` or `=`"
    );
    ensure!(
        !text.contains(['\n', '\r']),
        "bind or state entry `{name}`: {field} path contains a newline"
    );
    Ok(text)
}

fn protected_runtime_overlap(target: &Path) -> Option<&Path> {
    // Children of the silo home are intentional mount targets, but replacing
    // the home itself or an ancestor would hide its managed ownership.
    let home = Path::new(CONTAINER_HOME);
    if home.starts_with(target) {
        return Some(home);
    }

    for protected in PROTECTED_RUNTIME_DIRS.iter().map(Path::new) {
        if target.starts_with(protected) || protected.starts_with(target) {
            return Some(protected);
        }
    }
    for asset in RUNTIME_ASSETS {
        let protected = Path::new(asset.image_path);
        if target.starts_with(protected) || protected.starts_with(target) {
            return Some(protected);
        }
    }
    None
}

pub(super) fn resolve_config_mounts(
    config: &Config,
    project_root: &Path,
    host_ports: Option<&host_ports::Tunnel>,
) -> Result<ConfigMounts> {
    let read_only = resolve_read_only_paths(project_root, &config.workspace.read_only);
    let configured = resolve_configured_mounts(
        config,
        project_root,
        &read_only,
        std::env::var_os("HOME").as_deref().map(Path::new),
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
    )?;
    Ok(ConfigMounts {
        read_only,
        configured,
        host_ports: host_ports.map(|tunnel| tunnel.assets.clone()),
    })
}
