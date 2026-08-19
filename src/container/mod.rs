use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table, presets::NOTHING};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{
    Config, Container, Forward, Mount, MountKind, Permission, Shell, normalize_read_only_path,
};
use crate::forward;

/// Name of the image this tool builds and runs.
pub const IMAGE_TAG: &str = "silo:latest";

/// Dockerfile embedded into the executable at compile time.
pub const DOCKERFILE: &str = include_str!("silo.dockerfile");

/// Supervisor embedded alongside the built-in Dockerfile.
const SUPERVISOR: &str = include_str!("silo-supervisor.sh");

/// Session lease wrapper embedded alongside the built-in Dockerfile.
const SESSION_WRAPPER: &str = include_str!("silo-session.sh");

/// Session reservation helper embedded alongside the built-in Dockerfile.
const SESSION_RESERVER: &str = include_str!("silo-reserve.sh");

/// Session counter embedded alongside the built-in Dockerfile.
const STATUS_HELPER: &str = include_str!("silo-status.sh");

/// Stop guard embedded alongside the built-in Dockerfile.
const STOP_GUARD: &str = include_str!("silo-stop-guard.sh");

/// Restricted SSH server configuration used only for reverse forwarding.
const SSHD_CONFIG: &str = include_str!("silo-sshd_config");

const CONTAINER_BIN: &str = "container";

/// Fixed parent for the per-user lock guarding Apple's user-global builder.
const BUILD_LOCK_PARENT: &str = "/tmp";

/// File that explicitly marks a directory as a Silo project root.
const PROJECT_MARKER: &str = ".silo.toml";

/// Directory that implicitly marks a project root when no Silo marker exists.
const GIT_DIR: &str = ".git";

/// Jujutsu workspace directory used as a project marker alongside Git.
const JJ_DIR: &str = ".jj";

/// Prefix of every `--name` silo passes to `container run`.
const CONTAINER_NAME_PREFIX: &str = "silo-";

/// Amount of the SHA-256 digest used in a shared container ID.
const PROJECT_DIGEST_HEX_LEN: usize = 24;

const BASH_PATH: &str = "/bin/bash";
const ZSH_PATH: &str = "/home/linuxbrew/.linuxbrew/bin/zsh";
const FISH_PATH: &str = "/home/linuxbrew/.linuxbrew/bin/fish";
const NU_PATH: &str = "/home/linuxbrew/.linuxbrew/bin/nu";
const DEFAULT_SHELL: Shell = Shell::Zsh;

/// PID 1 for shared containers; it exits when the final guest-side session
/// lease closes.
const SHARED_INIT_COMMAND: &str = "/usr/local/bin/silo-supervisor";

/// Guest wrapper that holds a shared lease for the command and all children.
const SESSION_WRAPPER_COMMAND: &str = "/usr/local/bin/silo-session";
const SESSION_RESERVE_COMMAND: &str = "/usr/local/bin/silo-reserve";
const STATUS_COMMAND: &str = "/usr/local/bin/silo-status";
const GUEST_READY_PATH: &str = "/run/silo/ready";

const LABEL_OWNER: &str = "dev.silo.owner";
const LABEL_SCHEMA: &str = "dev.silo.schema";
const LABEL_PROJECT: &str = "dev.silo.project";
const LABEL_PROJECT_ROOT: &str = "dev.silo.project-root";
const LABEL_LIFECYCLE: &str = "dev.silo.lifecycle";
const LABEL_SPEC: &str = "dev.silo.spec";
const LABEL_OWNER_VALUE: &str = "silo";
const LABEL_SCHEMA_VALUE: &str = "1";
const LABEL_SHARED_VALUE: &str = "shared";
const LABEL_ISOLATED_VALUE: &str = "isolated";
/// Runtime races are retried only for this bounded interval.
const CONFLICT_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const CONFLICT_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const GUEST_READY_TIMEOUT: Duration = Duration::from_mins(1);

/// Home directory of the container's `silo` user; the shared project
/// directory is mounted into it as `<home>/<project-name>`.
const CONTAINER_HOME: &str = "/home/silo";

const MANAGED_STATE_ID_PREFIX: &str = "silo-state-";
const PROJECT_ROOT_METADATA: &str = ".project-root";
const STATE_PROJECT_VALUE: &str = "project";
const STATE_USER_VALUE: &str = "user";

/// One named configurable mount resolved for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedMount {
    name: String,
    source: ResolvedMountSource,
    dest: PathBuf,
    access: Permission,
}

/// One project-owned path overlaid read-only after the writable project bind.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadOnlyProjectPath {
    host: PathBuf,
    relative: PathBuf,
}

struct ResolvedIsolatedMounts {
    named: Vec<ResolvedMount>,
    skipped: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedMountSource {
    Host(PathBuf),
    Managed(ManagedMount),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedMount {
    id: String,
    scope: StateScope,
    name: String,
    project: Option<PathBuf>,
    path: PathBuf,
}

struct MountLock {
    file: fs::File,
}

/// Cross-process serialization for Silo operations that replace Apple's
/// user-global image builder. Its location cannot depend on process-specific
/// state-directory environment variables.
struct BuildLock {
    _lock: MountLock,
}

impl BuildLock {
    fn acquire() -> Result<Self> {
        let root = global_build_lock_root();
        ensure_owned_private_directory(&root, "image build lock root")?;
        MountLock::acquire_at_for(&root, "image build").map(|lock| Self { _lock: lock })
    }

    #[cfg(test)]
    fn acquire_at(state_root: &Path) -> Result<Self> {
        MountLock::acquire_at_for(&state_root.join("build"), "image build")
            .map(|lock| Self { _lock: lock })
    }
}

fn global_build_lock_root() -> PathBuf {
    Path::new(BUILD_LOCK_PARENT).join(format!("silo-build-{}", unsafe { libc::geteuid() }))
}

impl MountLock {
    fn acquire() -> Result<Self> {
        let state_root = managed_state_root_from_env().ok_or_else(|| {
            anyhow!("managed state requires XDG_STATE_HOME or HOME to be absolute")
        })?;
        ensure_private_directory(&state_root, "Silo state root")?;
        Self::acquire_at(&state_root.join("state"))
    }

    fn acquire_at(root: &Path) -> Result<Self> {
        Self::acquire_at_for(root, "managed state")
    }

    fn acquire_at_for(root: &Path, description: &str) -> Result<Self> {
        ensure_private_directory(root, &format!("{description} lock root"))?;
        let path = root.join(".lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("could not open {description} lock `{}`", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!("could not protect {description} lock `{}`", path.display())
        })?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("could not lock {description} `{}`", path.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for MountLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StateScope {
    Project,
    User,
}

impl StateScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Project => STATE_PROJECT_VALUE,
            Self::User => STATE_USER_VALUE,
        }
    }

    const fn config_kind(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

fn ensure_managed_mounts(mounts: &[ResolvedMount]) -> Result<()> {
    let mut managed = BTreeMap::new();
    for entry in effective_mounts(mounts) {
        if let ResolvedMountSource::Managed(mount) = &entry.source {
            managed.entry(mount.path.clone()).or_insert(mount);
        }
    }
    for mount in managed.into_values() {
        ensure_managed_mount(mount)?;
    }
    Ok(())
}

fn ensure_managed_mount(mount: &ManagedMount) -> Result<()> {
    let root = managed_mount_root_from_path(&mount.path, mount.scope)?;
    ensure_private_directory(root, "managed state root")?;
    match (&mount.scope, mount.project.as_deref()) {
        (StateScope::User, None) => {
            ensure_private_directory(&root.join("user"), "user state root")?;
        }
        (StateScope::Project, Some(project)) => {
            let projects = root.join("project");
            let project_dir = projects.join(project_digest(project));
            ensure_private_directory(&projects, "project state root")?;
            ensure_private_directory(&project_dir, "project state directory")?;
            ensure_project_metadata(&project_dir, project)?;
            ensure_private_directory(&project_dir.join("entries"), "project state collection")?;
        }
        _ => return Err(anyhow!("managed state scope and project identity disagree")),
    }
    ensure_private_directory(&mount.path, "managed state directory")?;
    Ok(())
}

fn managed_mount_root_from_path(path: &Path, scope: StateScope) -> Result<&Path> {
    let levels = match scope {
        StateScope::User => 2,
        StateScope::Project => 4,
    };
    path.ancestors()
        .nth(levels)
        .ok_or_else(|| anyhow!("managed state `{}` has no storage root", path.display()))
}

fn ensure_project_metadata(project_dir: &Path, project: &Path) -> Result<()> {
    let path = project_dir.join(PROJECT_ROOT_METADATA);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(anyhow!(
                    "refusing to use project state metadata `{}` because it is not a real file",
                    path.display()
                ));
            }
            let stored = fs::read(&path).with_context(|| {
                format!("could not read project state metadata `{}`", path.display())
            })?;
            if stored != project.as_os_str().as_bytes() {
                return Err(anyhow!(
                    "refusing to use project state directory `{}` because its project metadata does not match `{}`",
                    project_dir.display(),
                    project.display()
                ));
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!(
                    "could not protect project state metadata `{}`",
                    path.display()
                )
            })?;
            return Ok(());
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "could not inspect project state metadata `{}`",
                    path.display()
                )
            });
        }
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = project_dir.join(format!(
        "{PROJECT_ROOT_METADATA}.tmp.{}-{nonce}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "could not create project state metadata `{}`",
                    temporary.display()
                )
            })?;
        file.write_all(project.as_os_str().as_bytes())
            .with_context(|| {
                format!(
                    "could not write project state metadata `{}`",
                    temporary.display()
                )
            })?;
        file.sync_all().with_context(|| {
            format!(
                "could not sync project state metadata `{}`",
                temporary.display()
            )
        })?;
        fs::rename(&temporary, &path).with_context(|| {
            format!(
                "could not publish project state metadata `{}`",
                path.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_private_directory(path: &Path, description: &str) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("could not create {description} `{}`", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {description} `{}`", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "refusing to use {description} `{}` because it is not a real directory",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect {description} `{}`", path.display()))
}

/// Protects a predictable directory in a shared parent such as `/tmp` from
/// being substituted by another user before it is used for locking.
fn ensure_owned_private_directory(path: &Path, description: &str) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not create {description} `{}`", path.display()));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {description} `{}`", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "refusing to use {description} `{}` because it is not a real directory",
            path.display()
        ));
    }
    let expected_uid = unsafe { libc::geteuid() };
    if metadata.uid() != expected_uid {
        return Err(anyhow!(
            "refusing to use {description} `{}` because it is owned by user {} instead of {}",
            path.display(),
            metadata.uid(),
            expected_uid
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect {description} `{}`", path.display()))
}

fn effective_mounts(mounts: &[ResolvedMount]) -> impl Iterator<Item = &ResolvedMount> {
    mounts.iter().enumerate().filter_map(|(index, entry)| {
        (!mounts[index + 1..]
            .iter()
            .any(|later| later.dest == entry.dest))
        .then_some(entry)
    })
}

/// Creation-time project setup shared by identity and command construction.
#[derive(Default)]
struct ConfigMounts {
    read_only: Vec<ReadOnlyProjectPath>,
    named: Vec<ResolvedMount>,
    forwarding: Option<forward::GuestAssets>,
}

/// Stable identity and container path for the current project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    pub(crate) root: PathBuf,
    workdir: PathBuf,
    id: String,
}

impl Project {
    /// Discovers the current project through symlinks before deriving its ID.
    pub(crate) fn current() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to determine current directory")?;
        Self::from_path(&cwd)
    }

    /// Canonicalizes a starting directory and discovers its project root.
    fn from_path(cwd: &Path) -> Result<Self> {
        Self::from_root(project_root_from_path(cwd)?)
    }

    /// Applies container-specific validation after config-independent project
    /// root discovery has completed.
    fn from_root(root: PathBuf) -> Result<Self> {
        // Validate before persisting the path in labels or state metadata.
        bind_host_path(&root)?;
        let workdir = shared_dir_name(&root)?;
        let id = project_container_id(&root);
        Ok(Self { root, workdir, id })
    }
}

/// Discovers the current project root without requiring it to be a valid
/// container mount. Config-only commands use this lighter-weight path.
///
/// # Errors
///
/// Returns an error when the current directory cannot be read or resolved.
pub(crate) fn current_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    project_root_from_path(&cwd)
}

fn project_root_from_path(cwd: &Path) -> Result<PathBuf> {
    let cwd = fs::canonicalize(cwd)
        .with_context(|| format!("failed to resolve project directory `{}`", cwd.display()))?;
    Ok(discover_project_root(&cwd))
}

/// Selects a project root from an already-canonical starting directory.
///
/// An explicit Silo marker anywhere in the ancestor chain takes precedence
/// over every VCS directory. Otherwise the nearest Git or Jujutsu directory
/// wins. Without a marker, the exact starting directory is the project.
fn discover_project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(PROJECT_MARKER).is_file())
        .or_else(|| {
            cwd.ancestors()
                .find(|dir| dir.join(GIT_DIR).is_dir() || dir.join(JJ_DIR).is_dir())
        })
        .unwrap_or(cwd)
        .to_path_buf()
}

/// State reported by `container inspect` for one deterministic container ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerState {
    Absent,
    Running,
    Stopped,
    Stopping,
    Unknown,
}

/// The complete durable record returned by `container inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerInspection {
    state: ContainerState,
    ipv4_address: Option<Ipv4Addr>,
    labels: HashMap<String, String>,
    image: Option<String>,
    image_digest: Option<String>,
    mount_sources: Vec<PathBuf>,
    resources: ContainerResources,
}

impl ContainerInspection {
    fn absent() -> Self {
        Self {
            state: ContainerState::Absent,
            ipv4_address: None,
            labels: HashMap::new(),
            image: None,
            image_digest: None,
            mount_sources: Vec::new(),
            resources: ContainerResources::default(),
        }
    }
}

impl ContainerState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Stopping => "stopping",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ContainerLifecycle {
    Shared,
    Isolated,
}

impl ContainerLifecycle {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Shared => LABEL_SHARED_VALUE,
            Self::Isolated => LABEL_ISOLATED_VALUE,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ContainerResources {
    cpus: Option<u64>,
    memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerInfo {
    id: String,
    lifecycle: ContainerLifecycle,
    state: ContainerState,
    sessions: Option<usize>,
    project: PathBuf,
    spec: String,
    image: String,
    resources: ContainerResources,
}

#[derive(Default)]
struct ContainerInventory {
    items: Vec<ContainerInfo>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfo(ManagedMount);

#[derive(Default)]
struct MountInventory {
    items: Vec<MountInfo>,
    warnings: Vec<String>,
}

/// Runtime labels expected on the deterministic shared container.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerIdentity {
    project: String,
    project_root: String,
    spec: String,
}

/// Resolves existing read-only project paths without exposing symlink targets
/// outside the canonical project root. Missing and broken paths are omitted.
///
/// # Errors
///
/// Returns an error when an existing path is not a directory, because Apple
/// container does not support regular files as bind-mount sources.
fn resolve_read_only_paths(
    project_root: &Path,
    paths: &[PathBuf],
) -> Result<Vec<ReadOnlyProjectPath>> {
    let Some(root) = fs::canonicalize(project_root).ok() else {
        return Ok(Vec::new());
    };

    let mut resolved = Vec::with_capacity(paths.len());
    for path in paths {
        let relative = normalize_read_only_path(path)?;
        let Some(host) = mount_host(&project_root.join(&relative), &root) else {
            continue;
        };
        if !host.is_dir() {
            return Err(anyhow!(
                "read-only project path `{}` must be a directory because Apple container does not support bind-mounting individual files",
                relative.display()
            ));
        }
        resolved.push(ReadOnlyProjectPath { host, relative });
    }
    Ok(resolved)
}

/// Returns the canonical path of `path` when it resolves to a real path
/// inside `root_canonical`, or `None` otherwise (a symlink pointing away
/// must not become a mount, since the container runtime would mount the
/// link's target).
fn mount_host(path: &Path, root_canonical: &Path) -> Option<PathBuf> {
    let host = fs::canonicalize(path).ok()?;
    host.starts_with(root_canonical).then_some(host)
}

/// Resolves enabled binds and state for this project. Bind sources are expanded
/// and canonicalized; managed state receives deterministic private host
/// directories. Sorting parents before children makes nested overrides stable.
///
/// # Errors
///
/// Returns an error naming the bind when its source does not exist
/// or cannot be resolved (e.g. a broken symlink).
fn resolve_named_mounts(
    mounts: &BTreeMap<String, Mount>,
    project_root: &Path,
    home: Option<&Path>,
    xdg_state_home: Option<&Path>,
) -> Result<Vec<ResolvedMount>> {
    let mut resolved = Vec::with_capacity(mounts.len());
    let project_dir = shared_dir_name(project_root)?;
    for (name, entry) in mounts.iter().filter(|(_, entry)| entry.is_enabled()) {
        let kind = entry.kind().expect("enabled mounts were validated");
        let dest = entry
            .effective_target(&project_dir)
            .expect("enabled mounts were validated");
        let source = match kind {
            MountKind::Host => {
                let configured = entry.host_source().expect("binds were validated");
                let host = fs::canonicalize(expand_tilde(configured, home)).with_context(|| {
                    format!(
                        "cannot resolve source `{}` for bind `{name}` at `{}` (missing path or broken symlink?)",
                        configured.display(),
                        dest.display()
                    )
                })?;
                if !host.is_dir() {
                    return Err(anyhow!(
                        "source `{}` for bind `{name}` must be a directory",
                        configured.display()
                    ));
                }
                ResolvedMountSource::Host(host)
            }
            MountKind::ProjectState => ResolvedMountSource::Managed(managed_mount(
                StateScope::Project,
                name,
                Some(project_root),
                xdg_state_home,
                home,
            )?),
            MountKind::UserState => ResolvedMountSource::Managed(managed_mount(
                StateScope::User,
                name,
                None,
                xdg_state_home,
                home,
            )?),
        };
        resolved.push(ResolvedMount {
            name: name.clone(),
            source,
            dest,
            access: entry.effective_access(),
        });
    }
    resolved.sort_by(|left, right| {
        left.dest
            .components()
            .count()
            .cmp(&right.dest.components().count())
            .then_with(|| left.dest.cmp(&right.dest))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(resolved)
}

fn managed_mount_root(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    Ok(managed_state_root(xdg_state_home, home)?.join("state"))
}

fn managed_state_root(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    let state_home = xdg_state_home
        .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
                .map(|path| path.join(".local/state"))
        })
        .ok_or_else(|| anyhow!("managed state requires XDG_STATE_HOME or HOME to be absolute"))?;
    Ok(state_home.join("silo"))
}

fn managed_state_root_from_env() -> Option<PathBuf> {
    managed_state_root(
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
    .ok()
}

fn needs_mount_lock(mounts: &[ResolvedMount]) -> bool {
    effective_mounts(mounts).any(|mount| matches!(mount.source, ResolvedMountSource::Managed(_)))
}

fn managed_mount(
    scope: StateScope,
    name: &str,
    project: Option<&Path>,
    xdg_state_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<ManagedMount> {
    let root = managed_mount_root(xdg_state_home, home)?;
    Ok(managed_mount_at_root(scope, name, project, &root))
}

fn managed_mount_at_root(
    scope: StateScope,
    name: &str,
    project: Option<&Path>,
    root: &Path,
) -> ManagedMount {
    let name_digest = hex_digest(Sha256::digest(name.as_bytes()));
    let id = match (scope, project) {
        (StateScope::Project, Some(project)) => format!(
            "{MANAGED_STATE_ID_PREFIX}p-{}-{}",
            &project_digest(project)[..PROJECT_DIGEST_HEX_LEN],
            &name_digest[..16]
        ),
        (StateScope::User, None) => {
            format!("{MANAGED_STATE_ID_PREFIX}u-{}", &name_digest[..24])
        }
        _ => unreachable!("scope and project identity must agree"),
    };
    let path = match (scope, project) {
        (StateScope::Project, Some(project)) => root
            .join("project")
            .join(project_digest(project))
            .join("entries")
            .join(name),
        (StateScope::User, None) => root.join("user").join(name),
        _ => unreachable!("scope and project identity must agree"),
    };
    ManagedMount {
        id,
        scope,
        name: name.to_string(),
        project: project.map(Path::to_path_buf),
        path,
    }
}

/// Expands a leading `~` in `path` to the home directory: `~` and `~/x`
/// become `<home>` and `<home>/x`. Any other path (absolute, `~user`, plain
/// relative) is returned unchanged. Without a known, non-empty home
/// directory nothing is expanded, so an unset or empty `HOME` leaves the
/// path relative and the later `canonicalize` fails loudly instead of
/// resolving against the working directory.
fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home.filter(|home| !home.as_os_str().is_empty()) else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Returns warnings for configured entries whose runtime order silently
/// defeats their intent: a read-write bind or state entry at or under a protected project target
/// restores write access. Duplicate named targets are also reported along
/// with the deterministic winner.
fn mount_conflicts(
    project_dest: &Path,
    read_only: &[ReadOnlyProjectPath],
    mounts: &[ResolvedMount],
) -> Vec<String> {
    let mut warnings = Vec::new();
    for entry in effective_mounts(mounts) {
        if entry.access == Permission::ReadWrite {
            for protected in read_only {
                let protected_dest = project_path_target(project_dest, &protected.relative);
                if entry.dest.starts_with(&protected_dest) {
                    warnings.push(format!(
                        "the read-write entry `{}` at `{}` overlaps the read-only workspace path `{}`, so tools in the container can modify it",
                        entry.name,
                        entry.dest.display(),
                        protected.relative.display()
                    ));
                }
            }
        }
    }
    for (index, entry) in mounts.iter().enumerate() {
        for later in &mounts[index + 1..] {
            if entry.dest == later.dest {
                warnings.push(format!(
                    "mounts `{}` and `{}` both target `{}`; `{}` is applied later and wins",
                    entry.name,
                    later.name,
                    entry.dest.display(),
                    later.name
                ));
            }
        }
    }
    warnings
}

/// Temporary build directory that removes itself on drop, so cleanup also
/// runs on error paths.
struct BuildDir(PathBuf);

impl BuildDir {
    fn create() -> Result<Self> {
        let dir = build_dir();
        // Stale dir from an interrupted previous build.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create build dir `{}`", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn dockerfile(&self) -> PathBuf {
        self.0.join("silo.dockerfile")
    }

    fn supervisor(&self) -> PathBuf {
        self.0.join("silo-supervisor.sh")
    }

    fn session_wrapper(&self) -> PathBuf {
        self.0.join("silo-session.sh")
    }

    fn session_reserver(&self) -> PathBuf {
        self.0.join("silo-reserve.sh")
    }

    fn status_helper(&self) -> PathBuf {
        self.0.join("silo-status.sh")
    }

    fn stop_guard(&self) -> PathBuf {
        self.0.join("silo-stop-guard.sh")
    }

    fn sshd_config(&self) -> PathBuf {
        self.0.join("silo-sshd_config")
    }
}

impl Drop for BuildDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Rebuilds the image without cached layers: the embedded Dockerfile by
/// default, or the Dockerfile configured in `[image] dockerfile`, which is
/// then the user's own image. When the container system has not been started,
/// boots it first so the build does not fail and get retried for that.
///
/// # Errors
///
/// Returns an error when build serialization or storage cleanup fails, when
/// the configured Dockerfile is unusable, when the container CLI is missing,
/// when the embedded Dockerfile cannot be written, or when the build fails.
pub fn build_image(config: &Config) -> Result<ExitCode> {
    validate_image_config(config)?;
    let _build_lock = BuildLock::acquire()?;
    ensure_container_system_started()?;
    run_build_lifecycle(
        delete_builder,
        || build_configured_image(config),
        cleanup_build_storage,
    )
}

/// Validates image-related config without accessing the container runtime.
///
/// # Errors
///
/// Returns an error when a configured Dockerfile path is unusable.
fn validate_image_config(config: &Config) -> Result<()> {
    if let Some(dockerfile) = &config.image.dockerfile {
        validate_dockerfile(dockerfile)?;
    }
    Ok(())
}

/// Validates config relationships that are known to be unusable before any
/// container runtime access occurs.
///
/// # Errors
///
/// Returns an error when image config is invalid or an enabled mount is
/// incompatible with the selected image type.
pub(crate) fn validate_effective_config(config: &Config) -> Result<()> {
    validate_image_config(config)?;
    validate_custom_image_mounts(&config.mounts, config.image.dockerfile.is_some())
}

fn build_configured_image(config: &Config) -> Result<ExitCode> {
    if let Some(dockerfile) = &config.image.dockerfile {
        return execute_build(&mut build_command(
            dockerfile,
            dockerfile_context(dockerfile),
        ));
    }
    let build_dir = BuildDir::create()?;
    fs::write(build_dir.dockerfile(), DOCKERFILE).context("failed to write Dockerfile")?;
    fs::write(build_dir.supervisor(), SUPERVISOR).context("failed to write supervisor")?;
    fs::write(build_dir.session_wrapper(), SESSION_WRAPPER)
        .context("failed to write session wrapper")?;
    fs::write(build_dir.session_reserver(), SESSION_RESERVER)
        .context("failed to write session reserver")?;
    fs::write(build_dir.status_helper(), STATUS_HELPER)
        .context("failed to write session status helper")?;
    fs::write(build_dir.stop_guard(), STOP_GUARD).context("failed to write stop guard")?;
    fs::write(build_dir.sshd_config(), SSHD_CONFIG)
        .context("failed to write SSH forwarding configuration")?;
    execute_build(&mut build_command(
        &build_dir.dockerfile(),
        build_dir.path(),
    ))
}

/// Reclaims stale builder storage before the build and guarantees normal-path
/// teardown afterward, without hiding the build's own failure status.
fn run_build_lifecycle(
    delete_before: impl FnOnce() -> Result<()>,
    build: impl FnOnce() -> Result<ExitCode>,
    cleanup_after: impl FnOnce() -> Result<()>,
) -> Result<ExitCode> {
    delete_before()?;
    let build_result = build();
    let cleanup_result = cleanup_after();
    match build_result {
        Ok(code) if code == ExitCode::SUCCESS => {
            cleanup_result?;
            Ok(code)
        }
        Ok(code) => {
            if let Err(err) = cleanup_result {
                eprintln!("warning: image build cleanup failed: {err:#}");
            }
            Ok(code)
        }
        Err(err) => {
            if let Err(cleanup_err) = cleanup_result {
                eprintln!("warning: image build cleanup failed: {cleanup_err:#}");
            }
            Err(err)
        }
    }
}

fn ensure_container_system_started() -> Result<()> {
    let (_, stderr) = probe_image()?;
    if system_not_started(&stderr) && !start_container_system() {
        return Err(anyhow!(
            "could not start the Apple container system before cleaning build storage"
        ));
    }
    Ok(())
}

fn delete_builder() -> Result<()> {
    // Silo exclusively owns the user's Apple container state, so reclaiming
    // the global builder is part of its storage lifecycle.
    execute_maintenance(builder_delete_command(), "delete the global image builder")
}

fn prune_images() -> Result<()> {
    // The exclusive-ownership contract also makes global dangling images
    // disposable after Silo publishes its tagged image.
    execute_maintenance(image_prune_command(), "prune dangling images")
}

fn cleanup_build_storage() -> Result<()> {
    cleanup_build_storage_with(delete_builder, prune_images)
}

fn cleanup_build_storage_with(
    delete: impl FnOnce() -> Result<()>,
    prune: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let builder = delete();
    let images = prune();
    match (builder, images) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(builder), Ok(())) => Err(builder),
        (Ok(()), Err(images)) => Err(images),
        (Err(builder), Err(images)) => Err(anyhow!(
            "could not delete the global image builder: {builder:#}; could not prune dangling images: {images:#}"
        )),
    }
}

fn builder_delete_command() -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["builder", "delete", "--force"]);
    command
}

fn image_prune_command() -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "prune"]);
    command
}

fn execute_maintenance(mut command: Command, description: &str) -> Result<()> {
    let status = command.status().map_err(spawn_error)?;
    if status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "failed to {description}: `{}` exited with {}",
        command_display(&command),
        status
    ))
}

fn command_display(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Checks that a configured Dockerfile path is usable: non-empty, existing,
/// and a regular file (a symlink to one counts as a file).
///
/// # Errors
///
/// Returns an error when the path is empty, does not exist, or is not a file.
fn validate_dockerfile(dockerfile: &Path) -> Result<()> {
    if dockerfile.as_os_str().is_empty() {
        return Err(anyhow!("image dockerfile path is empty"));
    }
    if !dockerfile.exists() {
        return Err(anyhow!(
            "image dockerfile `{}` does not exist",
            dockerfile.display()
        ));
    }
    if !dockerfile.is_file() {
        return Err(anyhow!(
            "image dockerfile `{}` is not a file",
            dockerfile.display()
        ));
    }
    Ok(())
}

/// Returns the build context for a configured Dockerfile: its own directory,
/// so relative COPY/ADD paths resolve as the author wrote them. A bare file
/// name without a directory component falls back to the current directory.
fn dockerfile_context(dockerfile: &Path) -> &Path {
    dockerfile
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Host user and group ids, forwarded to the container so its `silo` user can
/// be remapped to them and keep the shared directory writable on both sides.
struct HostIds {
    uid: String,
    gid: String,
}

/// Creation inputs prepared from the current shared-container configuration.
struct PreparedSharedContainer {
    ids: HostIds,
    config_mounts: ConfigMounts,
    forwarding: forward::Session,
}

/// Determines whether this invocation may apply current forwarding settings.
enum SharedContainerUse {
    Current(forward::Session),
    Existing(RunningContainerWarning),
}

enum RunningContainerWarning {
    Drift,
    Unverified(anyhow::Error),
}

/// Runs a command in this project's shared container, or in a one-shot
/// foreground container when `isolated` is set or the configured image is
/// custom. Custom images retain the image-defined user, command, and runtime
/// filesystem contract through the established one-shot lifecycle. Shared
/// runs use a guest reservation to hand off safely from runtime inspection to
/// an attached exec session while still allowing concurrent sessions.
///
/// # Errors
///
/// Returns an error when setup, state inspection, container creation, or the
/// attached process fails.
pub fn run_image(
    config: &Config,
    project: &Project,
    command: &[OsString],
    isolated: bool,
) -> Result<ExitCode> {
    let isolated_lifecycle = uses_isolated_lifecycle(config, isolated);
    runtime_preflight(isolated_lifecycle)?;
    let shell = config
        .image
        .dockerfile
        .is_none()
        .then(|| resolve_shell(config.shell, std::env::var_os("SHELL").as_deref()));
    if isolated_lifecycle {
        warn_unsupported_forwards(config, isolated);
        return run_isolated(config, project, command, shell);
    }
    run_shared(
        config,
        project,
        command,
        shell.expect("the shared lifecycle always uses the built-in image"),
    )
}

/// Custom images cannot rely on the built-in image's shared-container user,
/// home, shell, or supervisor, so preserve their image-agnostic lifecycle.
fn uses_isolated_lifecycle(config: &Config, isolated: bool) -> bool {
    isolated || config.image.dockerfile.is_some()
}

/// Prepares only the runtime resources required by the selected lifecycle.
fn runtime_preflight(isolated_lifecycle: bool) -> Result<()> {
    runtime_preflight_with(isolated_lifecycle, require_image)
}

fn runtime_preflight_with(
    isolated_lifecycle: bool,
    require_image: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if isolated_lifecycle {
        require_image()
    } else {
        Ok(())
    }
}

/// Reports enabled forwards that the selected lifecycle intentionally ignores.
fn warn_unsupported_forwards(config: &Config, isolated: bool) {
    if let Some(warning) = unsupported_forward_warning(config, isolated) {
        eprintln!("warning: {warning}");
    }
}

fn unsupported_forward_warning(config: &Config, isolated: bool) -> Option<&'static str> {
    if !config.forward.values().any(Forward::is_enabled) {
        return None;
    }
    if config.image.dockerfile.is_some() {
        Some(
            "host forwards are available only in the built-in shared container; ignoring enabled forwards for this custom-image run",
        )
    } else if isolated {
        Some(
            "host forwards are available only in the built-in shared container; ignoring enabled forwards for this isolated run",
        )
    } else {
        None
    }
}

/// Runs an ephemeral create/start lifecycle. Built-in isolated containers use
/// the same host-backed managed state as shared containers. Custom images
/// omit them because their image-defined user cannot safely be granted access
/// to private host-owned storage.
fn run_isolated(
    config: &Config,
    project: &Project,
    command: &[OsString],
    shell: Option<Shell>,
) -> Result<ExitCode> {
    let ids = host_ids()?;
    let custom_image = config.image.dockerfile.is_some();
    let container = effective_container_settings(&config.container, custom_image);
    let ResolvedIsolatedMounts { named, skipped } = resolve_isolated_named_mounts(
        &config.mounts,
        &project.root,
        std::env::var_os("HOME").as_deref().map(Path::new),
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        custom_image,
    )?;
    let id = isolated_container_id();
    // Build the command first: it only fails on path validation, before any
    // container exists.
    let config_mounts = ConfigMounts {
        read_only: resolve_read_only_paths(&project.root, &config.workspace.read_only)?,
        named,
        forwarding: None,
    };
    for (scope, name) in skipped {
        eprintln!(
            "warning: {scope} state `{name}` was skipped for a custom image because its image-defined user may not be able to access Silo's private host storage; use a bind with permissions appropriate for the image user"
        );
    }
    let mount_lock = needs_mount_lock(&config_mounts.named)
        .then(MountLock::acquire)
        .transpose()?;
    ensure_managed_mounts(&config_mounts.named)?;
    let mut create = isolated_create_command(
        std::io::stdin().is_terminal(),
        &project.root,
        &ids,
        &config_mounts,
        &container,
        &id,
        command,
        shell,
    )?;
    // Warn about named mounts whose later placement restores write access to
    // a protected project path.
    for warning in mount_conflicts(
        &project.workdir,
        &config_mounts.read_only,
        &config_mounts.named,
    ) {
        eprintln!("warning: {warning}");
    }
    sweep_orphaned_isolated_containers(Some(&id));
    create.stdout(Stdio::null());
    let create_status = create.status().map_err(spawn_error)?;
    drop(mount_lock);
    if !create_status.success() {
        cleanup_isolated_container(&id);
        return Ok(exit_code(create_status));
    }
    if let Err(err) = install_signal_handlers() {
        cleanup_isolated_container(&id);
        return Err(err);
    }
    // Captured before the child starts, so it holds the pre-raw-mode state.
    let terminal = SavedTerminal::capture();
    let mut start = isolated_start_command(&id);
    let mut child = match start.spawn() {
        Ok(child) => child,
        Err(err) => {
            cleanup_isolated_container(&id);
            return Err(spawn_error(err));
        }
    };
    let pid = libc::pid_t::try_from(child.id()).expect("child pid fits in pid_t");
    let status = wait_for_child(&mut child, pid);
    if let Some(terminal) = &terminal {
        terminal.restore();
    }
    cleanup_isolated_container(&id);
    status.map(exit_code)
}

/// Leaves custom images in full control of their own privilege policy.
fn effective_container_settings(container: &Container, custom_image: bool) -> Container {
    let mut effective = container.clone();
    effective.sudo &= !custom_image;
    effective
}

/// Applies the image compatibility policy before resolving mount sources, so
/// skipped managed state never requires or inspects managed host storage.
fn resolve_isolated_named_mounts(
    mounts: &BTreeMap<String, Mount>,
    project_root: &Path,
    home: Option<&Path>,
    xdg_state_home: Option<&Path>,
    custom_image: bool,
) -> Result<ResolvedIsolatedMounts> {
    validate_custom_image_mounts(mounts, custom_image)?;
    let mut available = mounts.clone();
    let skipped = remove_unavailable_managed_mounts(&mut available, custom_image);
    let named = resolve_named_mounts(&available, project_root, home, xdg_state_home)?;
    Ok(ResolvedIsolatedMounts { named, skipped })
}

/// Rejects enabled binds whose target depends on the built-in image's
/// home-directory contract.
fn validate_custom_image_mounts(
    mounts: &BTreeMap<String, Mount>,
    custom_image: bool,
) -> Result<()> {
    if !custom_image {
        return Ok(());
    }
    for (name, mount) in mounts
        .iter()
        .filter(|(_, mount)| mount.kind() == Some(MountKind::Host) && mount.is_enabled())
    {
        let Some(target) = mount.target.as_deref() else {
            continue;
        };
        if target
            .to_str()
            .is_some_and(|target| target.starts_with("~/"))
        {
            return Err(anyhow!(
                "bind `{name}` uses home-relative target `{}` but custom images have no Silo-defined home; use a project-relative `./...` target or an absolute target",
                target.display()
            ));
        }
    }
    Ok(())
}

fn remove_unavailable_managed_mounts(
    mounts: &mut BTreeMap<String, Mount>,
    custom_image: bool,
) -> Vec<(&'static str, String)> {
    let mut skipped = Vec::new();
    mounts.retain(|name, mount| match mount.kind() {
        Some(kind @ (MountKind::ProjectState | MountKind::UserState))
            if custom_image && mount.is_enabled() =>
        {
            let scope = match kind {
                MountKind::ProjectState => StateScope::Project,
                MountKind::UserState => StateScope::User,
                MountKind::Host => unreachable!("matched a managed state kind"),
            };
            skipped.push((scope.config_kind(), name.clone()));
            false
        }
        Some(MountKind::Host | MountKind::ProjectState | MountKind::UserState) | None => true,
    });
    skipped
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
    let (reservation, address, container_use) = loop {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "container `{}` repeatedly stopped during session handoff",
                project.id
            ));
        }
        let container_use = ensure_shared_container(project, config)?;
        let requires_address = matches!(
            &container_use,
            SharedContainerUse::Current(forwarding) if forwarding.requires_address()
        );
        let Some(inspection) = wait_for_guest_ready(project, requires_address)? else {
            continue;
        };
        let reservation = session_reservation_token(project);
        if reserve_shared_session(project, &reservation)? {
            break (reservation, inspection.ipv4_address, container_use);
        }
    };
    // Only an exact match may apply current forwarding assets; stale
    // containers keep any forwarding they already provide untouched.
    match container_use {
        SharedContainerUse::Current(forwarding) if forwarding.requires_address() => {
            let address = address.ok_or_else(|| {
                anyhow!(
                    "container `{}` did not expose an IPv4 address for SSH forwarding",
                    project.id
                )
            })?;
            forwarding.ensure_tunnel(address)?;
        }
        SharedContainerUse::Current(_) => {}
        SharedContainerUse::Existing(warning) => {
            eprintln!(
                "warning: {}",
                running_container_warning(&project.id, &warning)
            );
        }
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
    let pid = libc::pid_t::try_from(child.id()).expect("child pid fits in pid_t");
    let status = wait_for_child(&mut child, pid);
    if let Some(terminal) = &terminal {
        terminal.restore();
    }
    status.map(exit_code)
}

/// Resolves all creation-time config mounts for a project.
fn resolve_config_mounts(
    config: &Config,
    project_root: &Path,
    forwarding: &forward::Session,
) -> Result<ConfigMounts> {
    Ok(ConfigMounts {
        read_only: resolve_read_only_paths(project_root, &config.workspace.read_only)?,
        named: resolve_named_mounts(
            &config.mounts,
            project_root,
            std::env::var_os("HOME").as_deref().map(Path::new),
            std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        )?,
        forwarding: forwarding.guest().cloned(),
    })
}

/// Reports mount-order conflicts before creating a container.
fn warn_mount_conflicts(project: &Project, config_mounts: &ConfigMounts) {
    for warning in mount_conflicts(
        &project.workdir,
        &config_mounts.read_only,
        &config_mounts.named,
    ) {
        eprintln!("warning: {warning}");
    }
}

/// Prints a snapshot of every runtime container unambiguously owned by Silo.
pub fn print_containers() -> Result<ExitCode> {
    let inventory = container_inventory()?;
    if inventory.items.is_empty() {
        println!("No Silo containers.");
    } else {
        println!("{}", render_container_table(&inventory.items));
    }
    for warning in inventory.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints every persistent state directory owned by Silo.
pub fn print_state() -> Result<ExitCode> {
    let inventory = mount_inventory()?;
    if inventory.items.is_empty() {
        println!("No Silo managed state.");
    } else {
        println!("{}", render_mount_table(&inventory.items));
    }
    for warning in inventory.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Permanently deletes one selected, currently unused managed state entry.
pub fn delete_selected_state(selector: &str) -> Result<ExitCode> {
    let inventory = mount_inventory()?;
    let selected = select_mount(&inventory.items, selector)?.0.clone();
    let _mount_lock = MountLock::acquire()?;
    let expected = managed_mount(
        selected.scope,
        &selected.name,
        selected.project.as_deref(),
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?;
    if selected != expected {
        return Err(anyhow!(
            "refusing to delete managed state `{}` because its path or identity is inconsistent",
            selected.id
        ));
    }
    validate_managed_mount(&selected)?;
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
    if mount_directory_in_use(&selected.path)? {
        return Err(anyhow!(
            "could not delete managed state `{}` because a container still references it",
            selected.id
        ));
    }
    fs::remove_dir_all(&selected.path)
        .with_context(|| format!("could not delete managed state `{}`", selected.id))?;
    if selected.scope == StateScope::Project {
        prune_empty_project_state_directory(&selected.path)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn validate_managed_mount(mount: &ManagedMount) -> Result<()> {
    let root = managed_mount_root_from_path(&mount.path, mount.scope)?;
    validate_real_directory(root, "managed state root")?;
    if let Some(project) = mount.project.as_deref() {
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
        if stored != project
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

/// Stops and then deletes the single Silo container selected by ID or project.
pub fn delete_selected_container(selector: &str, force: bool) -> Result<ExitCode> {
    let inventory = container_inventory()?;
    let container = select_container(&inventory.items, selector)?.clone();
    stop_selected_container(&container, force)?;
    if force {
        if revalidate_selected_container(&container)?.is_some() {
            delete_container(&container.id)?;
        }
    } else {
        delete_stopped_container(&container)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn stop_selected_container(container: &ContainerInfo, force: bool) -> Result<()> {
    let Some(inspection) = revalidate_selected_container(container)? else {
        return Ok(());
    };
    match inspection.state {
        ContainerState::Running => {
            if force {
                stop_runtime_container(container)?;
            } else {
                require_current_inactive(container)?;
                if container.lifecycle != ContainerLifecycle::Shared {
                    return Err(anyhow!(
                        "container `{}` is an active isolated session; use `--force` to terminate it",
                        container.id
                    ));
                }
                guarded_stop(container)?;
            }
        }
        ContainerState::Stopped | ContainerState::Absent => {}
        ContainerState::Stopping => wait_until_stopped(container)?,
        ContainerState::Unknown => {
            if force {
                stop_runtime_container(container)?;
            } else {
                return Err(anyhow!(
                    "container `{}` is in an unsupported runtime state; use `--force` to terminate and delete it",
                    container.id
                ));
            }
        }
    }
    Ok(())
}

fn revalidate_selected_container(container: &ContainerInfo) -> Result<Option<ContainerInspection>> {
    let inspection = inspect_container(&container.id)?;
    if inspection.state == ContainerState::Absent {
        return Ok(None);
    }
    validate_selected_ownership(container, &inspection)?;
    Ok(Some(inspection))
}

fn validate_selected_ownership(
    container: &ContainerInfo,
    inspection: &ContainerInspection,
) -> Result<()> {
    let Some((lifecycle, project, spec)) = silo_metadata(inspection) else {
        return Err(anyhow!(
            "refusing to manage container `{}` because its ownership labels changed",
            container.id
        ));
    };
    if lifecycle != container.lifecycle || project != container.project || spec != container.spec {
        return Err(anyhow!(
            "refusing to manage container `{}` because it no longer matches the selected Silo container",
            container.id
        ));
    }
    Ok(())
}

fn require_current_inactive(container: &ContainerInfo) -> Result<()> {
    let sessions = match container.lifecycle {
        ContainerLifecycle::Isolated => Some(1),
        ContainerLifecycle::Shared => match inspect_session_count(&container.id) {
            Ok(count) => Some(count),
            Err(err) => {
                let state = revalidate_selected_container(container)?
                    .map_or(ContainerState::Absent, |inspection| inspection.state);
                if matches!(state, ContainerState::Absent | ContainerState::Stopped) {
                    return Ok(());
                }
                return Err(err.context(format!(
                    "container `{}` has unknown session state; use `--force` to terminate it",
                    container.id
                )));
            }
        },
    };
    require_inactive_sessions(&container.id, sessions)
}

fn require_inactive_sessions(id: &str, sessions: Option<usize>) -> Result<()> {
    let sessions = sessions.ok_or_else(|| {
        anyhow!("container `{id}` has unknown session state; use `--force` to terminate it")
    })?;
    if sessions == 0 {
        return Ok(());
    }
    let noun = if sessions == 1 { "session" } else { "sessions" };
    Err(anyhow!(
        "container `{id}` has {sessions} active {noun}; use `--force` to terminate them"
    ))
}

fn container_inventory() -> Result<ContainerInventory> {
    let mut inventory = ContainerInventory::default();
    for id in list_container_ids()? {
        let inspection = match inspect_container(&id) {
            Ok(inspection) if inspection.state != ContainerState::Absent => inspection,
            Ok(_) => continue,
            Err(err) => {
                inventory
                    .warnings
                    .push(format!("could not inspect container `{id}`: {err:#}"));
                continue;
            }
        };
        let Some((lifecycle, project, spec)) = silo_metadata(&inspection) else {
            continue;
        };
        let sessions = match (inspection.state, lifecycle) {
            (ContainerState::Stopped, _) => Some(0),
            (ContainerState::Running, ContainerLifecycle::Isolated) => Some(1),
            _ => None,
        };
        inventory.items.push(ContainerInfo {
            id,
            lifecycle,
            state: inspection.state,
            sessions,
            project,
            spec,
            image: inspection.image.unwrap_or_else(|| IMAGE_TAG.to_string()),
            resources: inspection.resources,
        });
    }
    populate_session_counts(&mut inventory);
    inventory.items.sort_by(|left, right| {
        left.project
            .cmp(&right.project)
            .then_with(|| left.lifecycle.cmp(&right.lifecycle))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(inventory)
}

fn mount_inventory() -> Result<MountInventory> {
    mount_inventory_for_env(
        std::env::var_os("XDG_STATE_HOME").as_deref().map(Path::new),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

fn mount_inventory_for_env(
    xdg_state_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<MountInventory> {
    let root = managed_mount_root(xdg_state_home, home)?;
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

fn mount_inventory_at(root: &Path) -> MountInventory {
    let mut inventory = MountInventory::default();
    if !inventory_root_is_directory(root, "managed state", &mut inventory) {
        return inventory;
    }
    collect_user_state(root, &mut inventory);
    collect_project_mounts(root, &mut inventory);
    inventory.items.sort_by(|left, right| {
        left.0
            .scope
            .cmp(&right.0.scope)
            .then_with(|| left.0.project.cmp(&right.0.project))
            .then_with(|| left.0.name.cmp(&right.0.name))
            .then_with(|| left.0.id.cmp(&right.0.id))
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
        let mount = managed_mount_at_root(StateScope::User, &name, None, root);
        inventory.items.push(MountInfo(mount));
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
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
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
            let mount = managed_mount_at_root(StateScope::Project, &name, Some(&project), root);
            inventory.items.push(MountInfo(mount));
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

fn read_project_metadata(project_dir: &Path) -> Result<PathBuf> {
    let path = project_dir.join(PROJECT_ROOT_METADATA);
    let metadata = fs::symlink_metadata(&path).with_context(|| {
        format!(
            "could not inspect project state metadata `{}`",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(anyhow!(
            "project state metadata `{}` is not a real file",
            path.display()
        ));
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("could not read project state metadata `{}`", path.display()))?;
    let project = PathBuf::from(OsString::from_vec(bytes));
    if !project.is_absolute() {
        return Err(anyhow!(
            "project state metadata `{}` does not contain an absolute path",
            path.display()
        ));
    }
    Ok(project)
}

fn valid_mount_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn render_mount_table(items: &[MountInfo]) -> String {
    let ambiguous_names =
        ambiguous_project_names(items.iter().filter_map(|item| item.0.project.as_deref()));
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(
            ["ID", "SCOPE", "NAME", "PROJECT", "SOURCE"]
                .into_iter()
                .map(|value| Cell::new(value).add_attribute(Attribute::Bold)),
        );
    for MountInfo(mount) in items {
        table.add_row([
            Cell::new(&mount.id),
            Cell::new(mount.scope.as_str()),
            Cell::new(&mount.name),
            Cell::new(mount.project.as_deref().map_or_else(
                || "-".to_string(),
                |project| display_project(project, &ambiguous_names),
            )),
            Cell::new(display_path(&mount.path)),
        ]);
    }
    table.to_string()
}

fn display_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    path.strip_prefix(&home).map_or_else(
        |_| path.display().to_string(),
        |relative| {
            if relative.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", relative.display())
            }
        },
    )
}

/// Finds basenames that identify more than one distinct canonical project.
fn ambiguous_project_names<'a>(
    projects: impl IntoIterator<Item = &'a Path>,
) -> BTreeSet<&'a OsStr> {
    let mut first_paths = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    for project in projects {
        let Some(name) = project.file_name() else {
            continue;
        };
        if let Some(first) = first_paths.get(name) {
            if *first != project {
                ambiguous.insert(name);
            }
        } else {
            first_paths.insert(name, project);
        }
    }
    ambiguous
}

fn display_project(project: &Path, ambiguous_names: &BTreeSet<&OsStr>) -> String {
    match project.file_name() {
        Some(name) if !ambiguous_names.contains(name) => name
            .to_str()
            .map_or_else(|| display_path(project), str::to_owned),
        Some(_) | None => display_path(project),
    }
}

fn select_mount<'a>(items: &'a [MountInfo], selector: &str) -> Result<&'a MountInfo> {
    if let Some(item) = items.iter().find(|item| item.0.id == selector) {
        return Ok(item);
    }
    let expanded = expand_selector_home(selector);
    let path_matches: Vec<_> = items
        .iter()
        .filter(|item| item.0.project.as_deref() == Some(&expanded))
        .collect();
    if !path_matches.is_empty() {
        return one_mount_match(selector, &path_matches);
    }
    let named_matches: Vec<_> = items
        .iter()
        .filter(|item| {
            item.0
                .project
                .as_deref()
                .and_then(Path::file_name)
                .is_some_and(|name| name == selector)
                || item.0.name == selector
        })
        .collect();
    if !named_matches.is_empty() {
        return one_mount_match(selector, &named_matches);
    }
    let id_matches: Vec<_> = items
        .iter()
        .filter(|item| item.0.id.starts_with(selector))
        .collect();
    if !id_matches.is_empty() {
        return one_mount_match(selector, &id_matches);
    }
    Err(anyhow!("no Silo managed state matches `{selector}`"))
}

fn one_mount_match<'a>(selector: &str, matches: &[&'a MountInfo]) -> Result<&'a MountInfo> {
    if let [item] = matches {
        return Ok(*item);
    }
    Err(anyhow!(
        "selector `{selector}` is ambiguous; matching managed state entries: {}",
        matches
            .iter()
            .map(|item| {
                let mount = &item.0;
                let project = mount
                    .project
                    .as_deref()
                    .map_or_else(|| "-".to_string(), display_path);
                format!(
                    "{} ({}, {}, {})",
                    mount.id,
                    mount.scope.as_str(),
                    mount.name,
                    project
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn populate_session_counts(inventory: &mut ContainerInventory) {
    const MAX_SESSION_WORKERS: usize = 8;

    let targets: Vec<(usize, String)> = inventory
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.state == ContainerState::Running && item.lifecycle == ContainerLifecycle::Shared
        })
        .map(|(index, item)| (index, item.id.clone()))
        .collect();
    if targets.is_empty() {
        return;
    }

    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..targets.len().min(MAX_SESSION_WORKERS) {
            let sender = sender.clone();
            let targets = &targets;
            let next = &next;
            scope.spawn(move || {
                loop {
                    let target = next.fetch_add(1, Ordering::Relaxed);
                    let Some((index, id)) = targets.get(target) else {
                        break;
                    };
                    if sender
                        .send((*index, id.clone(), inspect_session_count(id)))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for (index, id, result) in receiver {
            match result {
                Ok(count) => inventory.items[index].sessions = Some(count),
                Err(err) => inventory.warnings.push(format!(
                    "could not read sessions for container `{id}`: {err:#}"
                )),
            }
        }
    });
    inventory.warnings.sort();
}

fn list_container_ids() -> Result<Vec<String>> {
    let (ids, stderr) = probe_container_ids()?;
    if let Some(ids) = ids {
        return Ok(ids);
    }
    if system_not_started(&stderr) && start_container_system() {
        let (ids, stderr) = probe_container_ids()?;
        return ids.ok_or_else(|| anyhow!("could not list containers: {stderr}"));
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

fn silo_metadata(
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
    if labels.get(LABEL_SCHEMA).map(String::as_str) != Some(LABEL_SCHEMA_VALUE) {
        return None;
    }
    let digest = labels.get(LABEL_PROJECT)?;
    let spec = labels.get(LABEL_SPEC)?;
    if !is_digest(digest) || !is_digest(spec) {
        return None;
    }
    let project = PathBuf::from(labels.get(LABEL_PROJECT_ROOT)?);
    if project_digest(&project) != *digest {
        return None;
    }
    Some((lifecycle, project, spec.clone()))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn inspect_session_count(id: &str) -> Result<usize> {
    let output = Command::new(CONTAINER_BIN)
        .args(["exec", "--user", "silo", id, STATUS_COMMAND])
        .output()
        .map_err(spawn_error)?;
    if !output.status.success() {
        return Err(anyhow!(
            "{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("guest returned an invalid session count")
}

fn render_container_table(items: &[ContainerInfo]) -> String {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let ambiguous_names = ambiguous_project_names(items.iter().map(|item| item.project.as_path()));
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(
            [
                "CONTAINER",
                "TYPE",
                "STATE",
                "SESSIONS",
                "CPUS",
                "MEMORY",
                "PROJECT",
                "IMAGE",
            ]
            .into_iter()
            .map(|value| Cell::new(value).add_attribute(Attribute::Bold)),
        );
    for item in items {
        let mut state = Cell::new(item.state.as_str());
        if color {
            state = state.fg(match item.state {
                ContainerState::Running => Color::Green,
                ContainerState::Stopping => Color::Yellow,
                ContainerState::Stopped | ContainerState::Absent => Color::DarkGrey,
                ContainerState::Unknown => Color::Red,
            });
        }
        table.add_row([
            Cell::new(short_id(&item.id)),
            Cell::new(item.lifecycle.as_str()),
            state,
            Cell::new(
                item.sessions
                    .map_or_else(|| "?".to_string(), |count| count.to_string()),
            ),
            Cell::new(
                item.resources
                    .cpus
                    .map_or_else(|| "?".to_string(), |cpus| cpus.to_string()),
            ),
            Cell::new(
                item.resources
                    .memory_bytes
                    .map_or_else(|| "?".to_string(), format_memory),
            ),
            Cell::new(display_project(&item.project, &ambiguous_names)),
            Cell::new(&item.image),
        ]);
    }
    table.to_string()
}

fn format_memory(bytes: u64) -> String {
    const MEBIBYTE: u64 = 1024 * 1024;
    const GIBIBYTE: u64 = 1024 * MEBIBYTE;

    if bytes.is_multiple_of(GIBIBYTE) {
        format!("{} GiB", bytes / GIBIBYTE)
    } else if bytes.is_multiple_of(MEBIBYTE) {
        format!("{} MiB", bytes / MEBIBYTE)
    } else {
        format!("{bytes} B")
    }
}

fn short_id(id: &str) -> String {
    const DISPLAY_CHARS: usize = 16;
    if id.chars().count() <= DISPLAY_CHARS {
        id.to_string()
    } else {
        id.chars().take(DISPLAY_CHARS).collect()
    }
}

fn select_container<'a>(items: &'a [ContainerInfo], selector: &str) -> Result<&'a ContainerInfo> {
    if let Some(item) = items.iter().find(|item| item.id == selector) {
        return Ok(item);
    }
    let expanded = expand_selector_home(selector);
    let path_matches: Vec<_> = items
        .iter()
        .filter(|item| item.project == expanded)
        .collect();
    if !path_matches.is_empty() {
        return one_match(selector, &path_matches);
    }

    let name_matches: Vec<_> = items
        .iter()
        .filter(|item| {
            item.project
                .file_name()
                .is_some_and(|name| name == selector)
        })
        .collect();
    if !name_matches.is_empty() {
        return one_match(selector, &name_matches);
    }
    let id_matches: Vec<_> = items
        .iter()
        .filter(|item| item.id.starts_with(selector))
        .collect();
    if !id_matches.is_empty() {
        return one_match(selector, &id_matches);
    }
    Err(anyhow!("no Silo container matches `{selector}`"))
}

fn one_match<'a>(selector: &str, matches: &[&'a ContainerInfo]) -> Result<&'a ContainerInfo> {
    if let [item] = matches {
        return Ok(*item);
    }
    Err(anyhow!(
        "selector `{selector}` is ambiguous; matching containers: {}",
        matches
            .iter()
            .map(|item| {
                format!(
                    "{} ({}, {})",
                    item.id,
                    item.lifecycle.as_str(),
                    display_path(&item.project)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn expand_selector_home(selector: &str) -> PathBuf {
    if selector == "~" {
        return std::env::var_os("HOME").map_or_else(|| PathBuf::from(selector), PathBuf::from);
    }
    if let Some(relative) = selector.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(relative);
    }
    PathBuf::from(selector)
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
    let mut ready = String::new();
    BufReader::new(stdout)
        .read_line(&mut ready)
        .context("failed to read stop guard readiness")?;
    if ready.trim() != "ready" {
        drop(input);
        let status = guard.wait().context("failed to wait for stop guard")?;
        let mut stderr = String::new();
        if let Some(mut pipe) = guard.stderr.take() {
            pipe.read_to_string(&mut stderr)
                .context("failed to read stop guard error")?;
        }
        if status.code() == Some(75) {
            return Err(anyhow!(
                "container `{id}` became active while stopping; retry after its sessions finish or use `--force`"
            ));
        }
        if status.code() == Some(76) {
            let reservations = stderr.trim().parse::<usize>().unwrap_or(1);
            let noun = if reservations == 1 {
                "session is"
            } else {
                "sessions are"
            };
            return Err(anyhow!(
                "container `{id}` has {reservations} {noun} starting; retry after the session handoff finishes or use `--force`"
            ));
        }
        return Err(anyhow!(
            "could not guard container `{id}`: {}",
            stderr.trim()
        ));
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

/// Runs the guard embedded in this host binary rather than the container's
/// copy, so replacement always uses the current reservation checks.
fn stop_guard_command(id: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command
        .args(["exec", "--user", "silo", id, "sh", "-c"])
        .arg(STOP_GUARD);
    command
}

fn delete_stopped_container(container: &ContainerInfo) -> Result<()> {
    let Some(inspection) = revalidate_selected_container(container)? else {
        return Ok(());
    };
    if inspection.state != ContainerState::Stopped {
        return Err(anyhow!(
            "container `{}` is no longer stopped and was not deleted",
            container.id
        ));
    }
    let id = &container.id;
    let output = Command::new(CONTAINER_BIN)
        .args(["delete", id])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if delete_succeeded(output.status, &stderr) {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to delete container `{id}`: {}",
            stderr.trim()
        ))
    }
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
    if output.status.success() || stderr.to_lowercase().contains("not found") {
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
            ContainerState::Absent | ContainerState::Stopped => return Ok(()),
            ContainerState::Stopping => {}
            _ => {
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

/// Returns the one-shot container ID, `<prefix><pid>`.
fn isolated_container_id() -> String {
    format!("{CONTAINER_NAME_PREFIX}{}", std::process::id())
}

/// Returns the deterministic shared ID derived from a canonical project path.
fn project_container_id(project: &Path) -> String {
    let hex = hex_digest(Sha256::digest(project.as_os_str().as_bytes()));
    format!("{CONTAINER_NAME_PREFIX}{}", &hex[..PROJECT_DIGEST_HEX_LEN])
}

/// Signal received by silo while the container runs, recorded by the
/// handler and forwarded to the `container run` child. `0` means none.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Records the signal for the main loop, which forwards it to the
/// `container run` child. Only async-signal-safe operations.
extern "C" fn record_signal(signal: libc::c_int) {
    PENDING_SIGNAL.store(signal, Ordering::Relaxed);
}

/// Installs handlers for the signals that should stop the container
/// (SIGINT, SIGTERM, SIGHUP, SIGQUIT), so silo survives them long enough to
/// remove the container and restore the terminal after the child exits.
///
/// # Errors
///
/// Returns an error when a handler cannot be installed.
fn install_signal_handlers() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // The handler is a plain function pointer; `sighandler_t` is an
        // integer type on macOS, hence the two-step cast.
        if unsafe { libc::signal(signal, record_signal as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(anyhow!(
                "failed to install handler for signal {signal}: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Waits for the `container run` child to exit, forwarding any signal
/// received by silo to the child so the container shuts down. The child
/// also receives terminal signals directly, being in the same process
/// group; forwarding a signal the child already handled is a harmless
/// no-op, and the child's own handlers decide what to do.
///
/// # Errors
///
/// Returns an error when the child cannot be waited on.
fn wait_for_child(child: &mut Child, pid: libc::pid_t) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let signal = PENDING_SIGNAL.swap(0, Ordering::Relaxed);
        if signal != 0 {
            // Ignored: the child may have exited between `try_wait` and
            // here, in which case `kill` fails with ESRCH.
            unsafe { libc::kill(pid, signal) };
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Terminal state of stdin captured before the container child starts and
/// restored after it exits. The container CLI puts the terminal into raw
/// mode while the container runs and does not restore it when it dies from
/// a signal (e.g. Ctrl+C in interactive mode), so silo restores it itself.
struct SavedTerminal(libc::termios);

impl SavedTerminal {
    /// Captures the current terminal state of stdin, or `None` when stdin is
    /// not a terminal (nothing to restore).
    fn capture() -> Option<Self> {
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, attrs.as_mut_ptr()) } == 0 {
            Some(Self(unsafe { attrs.assume_init() }))
        } else {
            None
        }
    }

    /// Restores the captured terminal state of stdin.
    fn restore(&self) {
        // Best effort: restoring the same state twice (the CLI already
        // restored on a normal exit) is harmless.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.0) };
    }
}

/// Computes the full project digest stored in the runtime label.
fn project_digest(project: &Path) -> String {
    hex_digest(Sha256::digest(project.as_os_str().as_bytes()))
}

/// Encodes digest bytes in the lowercase format used by runtime identifiers.
fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

/// Builds the desired runtime identity from every creation-time input.
fn container_identity(
    project: &Project,
    image: &str,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
    lifecycle: &str,
    command: &[OsString],
) -> ContainerIdentity {
    let mut hasher = Sha256::new();
    hash_spec_field(&mut hasher, b"image", image.as_bytes());
    hash_spec_field(&mut hasher, b"lifecycle", lifecycle.as_bytes());
    hash_spec_field(&mut hasher, b"uid", host_ids.uid.as_bytes());
    hash_spec_field(&mut hasher, b"gid", host_ids.gid.as_bytes());
    hash_spec_field(&mut hasher, b"project", project.root.as_os_str().as_bytes());
    hash_spec_field(
        &mut hasher,
        b"workdir",
        project.workdir.as_os_str().as_bytes(),
    );
    for entry in &config_mounts.read_only {
        hash_spec_field(
            &mut hasher,
            b"read-only-source",
            entry.host.as_os_str().as_bytes(),
        );
        hash_spec_field(
            &mut hasher,
            b"read-only-target",
            entry.relative.as_os_str().as_bytes(),
        );
    }
    if let Some(forwarding) = &config_mounts.forwarding {
        hash_spec_field(
            &mut hasher,
            b"forward-assets",
            forwarding.source().as_os_str().as_bytes(),
        );
        for port in forwarding.ports() {
            hash_spec_field(&mut hasher, b"forward-port", port.to_string().as_bytes());
        }
    }
    for mount in effective_mounts(&config_mounts.named) {
        hash_spec_field(&mut hasher, b"mount-name", mount.name.as_bytes());
        match &mount.source {
            ResolvedMountSource::Host(host) => {
                hash_spec_field(&mut hasher, b"mount-kind", b"host");
                hash_spec_field(&mut hasher, b"mount-source", host.as_os_str().as_bytes());
            }
            ResolvedMountSource::Managed(mount) => {
                hash_spec_field(&mut hasher, b"mount-kind", mount.scope.as_str().as_bytes());
                hash_spec_field(
                    &mut hasher,
                    b"mount-source",
                    mount.path.as_os_str().as_bytes(),
                );
            }
        }
        hash_spec_field(
            &mut hasher,
            b"mount-dest",
            mount.dest.as_os_str().as_bytes(),
        );
        hash_spec_field(
            &mut hasher,
            b"mount-access",
            match mount.access {
                Permission::ReadOnly => b"ro",
                Permission::ReadWrite => b"rw",
            },
        );
    }
    if let Some(cpus) = resources.cpus {
        hash_spec_field(&mut hasher, b"cpus", cpus.to_string().as_bytes());
    }
    if let Some(memory) = &resources.memory {
        hash_spec_field(&mut hasher, b"memory", memory.as_bytes());
    }
    hash_spec_field(
        &mut hasher,
        b"sudo",
        if resources.sudo {
            b"enabled"
        } else {
            b"disabled"
        },
    );
    for argument in command {
        hash_spec_field(&mut hasher, b"command", argument.as_os_str().as_bytes());
    }
    ContainerIdentity {
        project: project_digest(&project.root),
        project_root: project.root.to_string_lossy().into_owned(),
        spec: hex_digest(hasher.finalize()),
    }
}

/// Length-prefixes both names and values so the specification hash has a
/// canonical, ambiguity-free serialization.
fn hash_spec_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) {
    hasher.update(u64::try_from(name.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(name);
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

fn append_identity_labels(run: &mut Command, identity: &ContainerIdentity, lifecycle: &str) {
    for (key, value) in [
        (LABEL_OWNER, LABEL_OWNER_VALUE),
        (LABEL_SCHEMA, LABEL_SCHEMA_VALUE),
        (LABEL_PROJECT, identity.project.as_str()),
        (LABEL_PROJECT_ROOT, identity.project_root.as_str()),
        (LABEL_LIFECYCLE, lifecycle),
        (LABEL_SPEC, identity.spec.as_str()),
    ] {
        run.arg("--label").arg(format!("{key}={value}"));
    }
}

/// Waits for entrypoint readiness and, when forwarding is enabled, networking.
fn wait_for_guest_ready(
    project: &Project,
    require_address: bool,
) -> Result<Option<ContainerInspection>> {
    let deadline = Instant::now() + GUEST_READY_TIMEOUT;
    loop {
        let output = guest_ready_command(project).output().map_err(spawn_error)?;
        if output.status.success() {
            let inspection = inspect_container(&project.id)?;
            match inspection.state {
                ContainerState::Running => validate_shared_ownership(&inspection, project)?,
                ContainerState::Absent | ContainerState::Stopped | ContainerState::Stopping => {
                    return Ok(None);
                }
                ContainerState::Unknown => {
                    return Err(anyhow!(
                        "container `{}` entered an unsupported state while initializing",
                        project.id
                    ));
                }
            }
            if !require_address || inspection.ipv4_address.is_some() {
                return Ok(Some(inspection));
            }
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let inspection = inspect_container(&project.id)?;
        match inspection.state {
            ContainerState::Running => validate_shared_ownership(&inspection, project)?,
            ContainerState::Absent | ContainerState::Stopped | ContainerState::Stopping => {
                return Ok(None);
            }
            ContainerState::Unknown => {
                return Err(anyhow!(
                    "container `{}` entered an unsupported state while initializing",
                    project.id
                ));
            }
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
fn reserve_shared_session(project: &Project, reservation: &str) -> Result<bool> {
    let deadline = Instant::now() + CONFLICT_RETRY_TIMEOUT;
    loop {
        let output = session_reserve_command(project, reservation)
            .output()
            .map_err(spawn_error)?;
        if output.status.success() {
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let inspection = inspect_container(&project.id)?;
        match inspection.state {
            ContainerState::Running => validate_shared_ownership(&inspection, project)?,
            ContainerState::Absent | ContainerState::Stopped | ContainerState::Stopping => {
                return Ok(false);
            }
            ContainerState::Unknown => {
                return Err(anyhow!(
                    "container `{}` entered an unsupported state during session handoff",
                    project.id
                ));
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

fn session_reserve_command(project: &Project, reservation: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command
        .args(["exec", "--user", "silo"])
        .arg(&project.id)
        .arg(SESSION_RESERVE_COMMAND)
        .arg(reservation);
    command
}

fn session_reservation_token(project: &Project) -> String {
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
/// Running owned containers remain usable when current creation settings
/// drift or cannot be prepared; absent and stopped containers are reconciled.
fn ensure_shared_container(project: &Project, config: &Config) -> Result<SharedContainerUse> {
    let deadline = Instant::now() + CONFLICT_RETRY_TIMEOUT;
    let mut last_conflict = None;

    loop {
        let inspection = inspect_container(&project.id)?;
        match inspection.state {
            ContainerState::Absent => {
                let PreparedSharedContainer {
                    ids,
                    config_mounts,
                    forwarding,
                } = prepare_shared_container(project, config)?;
                warn_mount_conflicts(project, &config_mounts);
                let image_digest = require_image_digest()?;
                let identity = shared_container_identity(
                    project,
                    &image_digest,
                    &ids,
                    &config_mounts,
                    &config.container,
                );
                let mount_lock = needs_mount_lock(&config_mounts.named)
                    .then(MountLock::acquire)
                    .transpose()?;
                ensure_managed_mounts(&config_mounts.named)?;
                let creation = create_shared_container(
                    project,
                    &image_digest,
                    &ids,
                    &config_mounts,
                    &config.container,
                    &identity,
                );
                drop(mount_lock);
                match creation {
                    Ok(()) => return Ok(SharedContainerUse::Current(forwarding)),
                    Err(err) => last_conflict = Some(err),
                }
            }
            ContainerState::Running => {
                validate_shared_ownership(&inspection, project)?;
                return Ok(running_container_use(project, config, &inspection));
            }
            ContainerState::Stopped => {
                let container = container_info_from_inspection(project, &inspection)?;
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
            ContainerState::Stopping => {}
            ContainerState::Unknown => {
                return Err(anyhow!(
                    "container '{}' is in an unsupported runtime state",
                    project.id
                ));
            }
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

/// Prepares creation-time inputs only after runtime inspection determines
/// that current settings are needed for creation or compatibility checking.
fn prepare_shared_container(project: &Project, config: &Config) -> Result<PreparedSharedContainer> {
    let forwarding = forward::Session::prepare(config, &project.root)?;
    let ids = host_ids()?;
    let config_mounts = resolve_config_mounts(config, &project.root, &forwarding)?;
    Ok(PreparedSharedContainer {
        ids,
        config_mounts,
        forwarding,
    })
}

/// Compares a running container without allowing current configuration or
/// image-store failures to interrupt an existing workspace.
fn running_container_use(
    project: &Project,
    config: &Config,
    inspection: &ContainerInspection,
) -> SharedContainerUse {
    running_container_use_with(
        project,
        config,
        inspection,
        || prepare_shared_container(project, config),
        require_image_digest,
    )
}

fn running_container_use_with(
    project: &Project,
    config: &Config,
    inspection: &ContainerInspection,
    prepare: impl FnOnce() -> Result<PreparedSharedContainer>,
    inspect_image: impl FnOnce() -> Result<String>,
) -> SharedContainerUse {
    let prepared = match prepare() {
        Ok(prepared) => prepared,
        Err(err) => {
            return SharedContainerUse::Existing(RunningContainerWarning::Unverified(err));
        }
    };
    let image_digest = match inspect_image() {
        Ok(digest) => digest,
        Err(err) => {
            return SharedContainerUse::Existing(RunningContainerWarning::Unverified(err));
        }
    };
    let identity = shared_container_identity(
        project,
        &image_digest,
        &prepared.ids,
        &prepared.config_mounts,
        &config.container,
    );
    if inspection.labels.get(LABEL_SPEC) != Some(&identity.spec) {
        return SharedContainerUse::Existing(RunningContainerWarning::Drift);
    }
    warn_mount_conflicts(project, &prepared.config_mounts);
    SharedContainerUse::Current(prepared.forwarding)
}

fn running_container_warning(id: &str, warning: &RunningContainerWarning) -> String {
    match warning {
        RunningContainerWarning::Drift => format!(
            "container `{id}` was created from a different image or configuration; connecting to the existing running container without applying current creation settings; recreate it to apply them"
        ),
        RunningContainerWarning::Unverified(err) => format!(
            "could not verify running container `{id}` against the current image and configuration: {err:#}; connecting to the existing container without applying current creation settings"
        ),
    }
}

fn shared_container_identity(
    project: &Project,
    image: &str,
    ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
) -> ContainerIdentity {
    container_identity(
        project,
        image,
        ids,
        config_mounts,
        resources,
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    )
}

fn container_info_from_inspection(
    project: &Project,
    inspection: &ContainerInspection,
) -> Result<ContainerInfo> {
    validate_shared_ownership(inspection, project)?;
    let spec = inspection.labels.get(LABEL_SPEC).cloned().ok_or_else(|| {
        anyhow!(
            "container '{}' is missing its Silo specification label",
            project.id
        )
    })?;
    Ok(ContainerInfo {
        id: project.id.clone(),
        lifecycle: ContainerLifecycle::Shared,
        state: inspection.state,
        sessions: Some(0),
        project: project.root.clone(),
        spec,
        image: inspection
            .image
            .clone()
            .unwrap_or_else(|| IMAGE_TAG.to_string()),
        resources: inspection.resources,
    })
}

/// Creates a detached shared container with a unique, automatically removed
/// cidfile beneath the current user's temporary directory.
fn create_shared_container(
    project: &Project,
    image_digest: &str,
    ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
    identity: &ContainerIdentity,
) -> Result<()> {
    let cid_dir = tempfile::Builder::new()
        .prefix("silo-cid-")
        .tempdir_in(std::env::temp_dir())
        .context("failed to create temporary cid directory")?;
    let cidfile = cid_dir.path().join("container.cid");
    let current_digest = require_image_digest()?;
    if current_digest != image_digest {
        return Err(anyhow!(
            "image `{IMAGE_TAG}` changed while creating container '{}'; retrying",
            project.id
        ));
    }
    let output = create_command(
        project,
        image_digest,
        ids,
        config_mounts,
        resources,
        &cidfile,
    )?
    .output()
    .map_err(spawn_error)?;

    if !output.status.success() {
        cleanup_partial_creation(project, identity, &cidfile);
        return Err(anyhow!(
            "failed to create shared container '{}': {}",
            project.id,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let recorded = fs::read_to_string(&cidfile)
        .with_context(|| format!("failed to read cidfile '{}'", cidfile.display()))?;
    if recorded.trim() != project.id {
        return Err(anyhow!(
            "container runtime wrote unexpected ID '{}' to '{}'",
            recorded.trim(),
            cidfile.display()
        ));
    }
    let inspection = inspect_container(&project.id)?;
    if inspection.image_digest.as_deref() != Some(image_digest) {
        cleanup_partial_creation(project, identity, &cidfile);
        return Err(anyhow!(
            "container '{}' was created from a different image than `{IMAGE_TAG}`; retrying",
            project.id
        ));
    }
    validate_shared_container(&inspection, project, identity)?;
    Ok(())
}

/// Deletes a failed creation only if this invocation's cidfile and the
/// runtime's ownership labels independently confirm the target.
fn cleanup_partial_creation(project: &Project, identity: &ContainerIdentity, cidfile: &Path) {
    if !fs::read_to_string(cidfile).is_ok_and(|value| value.trim() == project.id) {
        return;
    }
    let owned = inspect_container(&project.id).is_ok_and(|inspection| {
        inspection.state != ContainerState::Absent
            && validate_shared_ownership(&inspection, project).is_ok()
            && inspection.labels.get(LABEL_SPEC) == Some(&identity.spec)
    });
    if owned && let Err(err) = delete_container(&project.id) {
        eprintln!(
            "warning: could not remove partially created container '{}': {err:#}",
            project.id
        );
    }
}

/// Refuses to adopt containers that are not unambiguously owned by Silo and
/// associated with the full canonical project digest.
fn validate_shared_ownership(inspection: &ContainerInspection, project: &Project) -> Result<()> {
    let expected_project = project_digest(&project.root);
    for (key, expected) in [
        (LABEL_OWNER, LABEL_OWNER_VALUE),
        (LABEL_SCHEMA, LABEL_SCHEMA_VALUE),
        (LABEL_PROJECT, expected_project.as_str()),
        (LABEL_PROJECT_ROOT, project.root.to_string_lossy().as_ref()),
        (LABEL_LIFECYCLE, LABEL_SHARED_VALUE),
    ] {
        let actual = inspection.labels.get(key).map(String::as_str);
        if actual != Some(expected) {
            return Err(anyhow!(
                "refusing to manage container '{}': label '{}' is {:?}, expected '{}'",
                project.id,
                key,
                actual,
                expected
            ));
        }
    }
    Ok(())
}

fn validate_shared_container(
    inspection: &ContainerInspection,
    project: &Project,
    identity: &ContainerIdentity,
) -> Result<()> {
    if !shared_container_matches(inspection, project, identity)? {
        return Err(anyhow!(
            "container '{}' was created from a different Silo specification; run 'silo containers delete {}' and retry",
            project.id,
            project.id
        ));
    }
    Ok(())
}

/// Checks ownership separately from the creation specification so ownership
/// always remains strict when a running container is reused.
fn shared_container_matches(
    inspection: &ContainerInspection,
    project: &Project,
    identity: &ContainerIdentity,
) -> Result<bool> {
    validate_shared_ownership(inspection, project)?;
    Ok(inspection.labels.get(LABEL_SPEC) == Some(&identity.spec))
}

/// Inspects a container, booting the container system and retrying once when needed.
fn inspect_container(id: &str) -> Result<ContainerInspection> {
    let (inspection, stderr) = probe_container(id)?;
    if inspection.is_none() && system_not_started(&stderr) && start_container_system() {
        let (inspection, stderr) = probe_container(id)?;
        return inspection.ok_or_else(|| container_inspect_error(id, &stderr));
    }
    inspection.ok_or_else(|| container_inspect_error(id, &stderr))
}

fn mount_directory_in_use(path: &Path) -> Result<bool> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("could not resolve managed state `{}`", path.display()))?;
    for id in list_container_ids()? {
        let inspection = inspect_container(&id)?;
        if inspection.mount_sources.iter().any(|source| {
            source == path || fs::canonicalize(source).is_ok_and(|source| source == canonical)
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Runs one raw container inspection; not-found is represented as absence.
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
    if stderr.to_lowercase().contains("not found") {
        return Ok((Some(ContainerInspection::absent()), stderr));
    }
    Ok((None, stderr))
}

fn container_inspect_error(id: &str, stderr: &str) -> anyhow::Error {
    anyhow!("could not inspect container `{id}`: {stderr}")
}

/// Parses a container inspection using the current Apple Container JSON shape.
fn parse_container_inspection(stdout: &[u8], id: &str) -> Result<ContainerInspection> {
    let items: Vec<Value> =
        serde_json::from_slice(stdout).context("invalid container inspect JSON")?;
    let item = items
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| anyhow!("container inspect did not return `{id}`"))?;
    let status = item
        .pointer("/status/state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("container inspect omitted the state for `{id}`"))?;
    let state = match status {
        "running" => ContainerState::Running,
        "stopped" => ContainerState::Stopped,
        "stopping" => ContainerState::Stopping,
        _ => ContainerState::Unknown,
    };
    let ipv4_address = item
        .pointer("/status/networks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|network| network.get("ipv4Address").and_then(Value::as_str))
        .filter_map(|address| address.split('/').next())
        .find_map(|address| address.parse().ok());
    let labels = item
        .pointer("/configuration/labels")
        .map(parse_inspect_labels)
        .transpose()?
        .unwrap_or_default();
    let image = item
        .pointer("/configuration/image/reference")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let image_digest = item
        .pointer("/configuration/image/descriptor/digest")
        .and_then(Value::as_str)
        .filter(|digest| !digest.is_empty())
        .map(ToString::to_string);
    let mount_sources = item
        .pointer("/configuration/mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|mount| mount.get("source").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect();
    let resources = ContainerResources {
        cpus: item
            .pointer("/configuration/resources/cpus")
            .and_then(Value::as_u64),
        memory_bytes: item
            .pointer("/configuration/resources/memoryInBytes")
            .and_then(Value::as_u64),
    };
    Ok(ContainerInspection {
        state,
        ipv4_address,
        labels,
        image,
        image_digest,
        mount_sources,
        resources,
    })
}

fn parse_inspect_labels(value: &Value) -> Result<HashMap<String, String>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("container inspect labels are not an object"))?;
    object
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_string()))
                .ok_or_else(|| anyhow!("container inspect label `{key}` is not a string"))
        })
        .collect()
}

/// Returns whether the `container delete` result means the container is
/// gone: a successful exit, or a failure because it was already removed
/// ("not found"), which is the normal case after `--rm` cleaned up.
fn delete_succeeded(status: ExitStatus, stderr: &str) -> bool {
    status.success() || stderr.to_lowercase().contains("not found")
}

/// Force-deletes the container `id`, killing it first if it is still
/// running. A container that is already gone ("not found") counts as
/// success, since `--rm` normally removes the container when it exits.
///
/// # Errors
///
/// Returns an error when the container CLI is missing or deletion fails.
fn delete_container(id: &str) -> Result<()> {
    let output = Command::new(CONTAINER_BIN)
        .args(["delete", "--force", id])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if delete_succeeded(output.status, &stderr) {
        return Ok(());
    }
    Err(anyhow!(
        "`{CONTAINER_BIN} delete --force {id}` failed: {}",
        stderr.trim()
    ))
}

/// Removes isolated containers whose PID-named owner no longer exists. The
/// runtime labels are the durable ownership record; listing is used only to
/// discover candidates, and each candidate is inspected again before deletion.
fn sweep_orphaned_isolated_containers(current_id: Option<&str>) {
    let output = match Command::new(CONTAINER_BIN)
        .args(["list", "--all", "--format", "json"])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            eprintln!("warning: could not enumerate isolated containers: {err}");
            return;
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !system_not_started(&stderr) {
            eprintln!(
                "warning: could not enumerate isolated containers: {}",
                stderr.trim()
            );
        }
        return;
    }
    let ids = match parse_container_ids(&output.stdout) {
        Ok(ids) => ids,
        Err(err) => {
            eprintln!("warning: could not parse isolated containers: {err:#}");
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
        let Ok(inspection) = inspect_container(&id) else {
            continue;
        };
        if inspection.state == ContainerState::Absent || !is_owned_isolated(&inspection) {
            continue;
        }
        if let Err(err) = delete_container(&id) {
            eprintln!("warning: could not remove orphaned isolated container `{id}`: {err:#}");
        }
    }
}

fn parse_container_ids(stdout: &[u8]) -> Result<Vec<String>> {
    let items: Vec<Value> =
        serde_json::from_slice(stdout).context("invalid container list JSON")?;
    items
        .iter()
        .map(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .ok_or_else(|| anyhow!("container list item omitted its ID"))
        })
        .collect()
}

fn isolated_owner_pid(id: &str) -> Option<libc::pid_t> {
    id.strip_prefix(CONTAINER_NAME_PREFIX)?
        .parse()
        .ok()
        .filter(|pid| *pid > 0)
}

fn is_owned_isolated(inspection: &ContainerInspection) -> bool {
    silo_metadata(inspection)
        .is_some_and(|(lifecycle, _, _)| lifecycle == ContainerLifecycle::Isolated)
}

fn owner_alive(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Removes an isolated container after its foreground process exits. `--rm`
/// normally did this already; force deletion is a best-effort crash safety net.
fn cleanup_isolated_container(id: &str) {
    if let Err(err) = delete_container(id) {
        eprintln!("warning: could not remove isolated container `{id}`: {err:#}");
    }
}

/// Builds the error reported when the image check failed, treating a missing
/// image ("not found" in the probe's stderr) separately from other probe
/// failures (e.g. the container system service is not running).
fn inspect_error(stderr: &str) -> anyhow::Error {
    if stderr.to_lowercase().contains("not found") {
        anyhow!("image `{IMAGE_TAG}` not built yet; run `silo image build` first")
    } else {
        anyhow!(
            "could not check for image `{IMAGE_TAG}`; \
             `{CONTAINER_BIN} image inspect` reported:\n{stderr}"
        )
    }
}

/// Returns whether the container CLI's stderr indicates the container system
/// (the VM behind the CLI) has not been started, using the CLI's documented
/// startup hint.
fn system_not_started(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    stderr.contains("container system service has been started")
        && stderr.contains("container system start")
}

/// Boots the container system with `container system start`, passing its
/// output through so the user sees the boot. Returns whether it started
/// successfully.
fn start_container_system() -> bool {
    Command::new(CONTAINER_BIN)
        .args(["system", "start"])
        .status()
        .is_ok_and(|status| status.success())
}

/// Runs `container image inspect`, returning the image's OCI digest when it
/// exists and the probe's stderr otherwise. When the probe fails because the
/// container system has not been started, boots it first and probes again
/// once; if the boot fails, the original failure remains available.
fn inspect_image() -> Result<(Option<String>, String)> {
    inspect_image_with(probe_image, start_container_system)
}

/// Requires the built image for operations that only need its presence.
fn require_image() -> Result<()> {
    require_image_digest().map(|_| ())
}

/// Returns the built image's immutable identity for shared-container reuse.
fn require_image_digest() -> Result<String> {
    let (digest, stderr) = inspect_image()?;
    digest.ok_or_else(|| inspect_error(&stderr))
}

/// The probe/boot/reprobe logic behind [`inspect_image`], separated so tests
/// can substitute fakes for the `container` CLI.
fn inspect_image_with(
    probe: impl Fn() -> Result<(Option<String>, String)>,
    boot: impl Fn() -> bool,
) -> Result<(Option<String>, String)> {
    let (digest, stderr) = probe()?;
    if digest.is_none() && system_not_started(&stderr) && boot() {
        return probe();
    }
    Ok((digest, stderr))
}

/// Runs the raw `container image inspect` probe, without booting anything.
fn probe_image() -> Result<(Option<String>, String)> {
    let output = Command::new(CONTAINER_BIN)
        .args(["image", "inspect", IMAGE_TAG])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Ok((None, stderr));
    }
    Ok((Some(parse_image_digest(&output.stdout)?), stderr))
}

/// Reads the immutable index digest from the current image inspection schema.
fn parse_image_digest(output: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(output)
        .context("could not parse image inspection output as JSON")?;
    let image = value
        .as_array()
        .and_then(|images| images.first())
        .ok_or_else(|| anyhow!("image inspection output did not contain an image"))?;
    image
        .pointer("/configuration/descriptor/digest")
        .and_then(Value::as_str)
        .filter(|digest| !digest.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("image inspection output did not contain an OCI image digest"))
}

fn execute(command: &mut Command) -> Result<ExitCode> {
    let status = command.status().map_err(spawn_error)?;
    Ok(exit_code(status))
}

/// Runs a build command, booting the container system first when it has not
/// been started, so the routine first-build case does not fail and get
/// retried. The system's state is detected with the same probe `silo run`
/// uses; the boot is best effort, and the build itself reports whatever is
/// wrong if it fails. The build's stderr is forwarded to silo's stderr
/// live, so progress and errors stay visible, while a bounded copy of its
/// tail is kept for inspection: if the build still fails because the system
/// was not started — the probe missed — the system is booted and the build
/// retried once.
///
/// # Errors
///
/// Returns an error when the container CLI is missing.
fn execute_build(command: &mut Command) -> Result<ExitCode> {
    execute_build_with(command, probe_image, start_container_system)
}

/// The probe/boot/retry logic behind [`execute_build`], separated so tests
/// can substitute fakes for the `container` CLI.
fn execute_build_with(
    command: &mut Command,
    probe: impl Fn() -> Result<(Option<String>, String)>,
    boot: impl Fn() -> bool,
) -> Result<ExitCode> {
    // Boot before building when the probe says the system is not started.
    // The boot's outcome is deliberately ignored: if it failed, the build
    // itself fails and reports why, and the retry below boots again.
    if let Ok((_, stderr)) = probe()
        && system_not_started(&stderr)
    {
        boot();
    }
    let captured = run_captured(command)?;
    if !captured.status.success()
        && system_not_started(&String::from_utf8_lossy(&captured.stderr))
        && boot()
    {
        // The system is up now; retry with normal stdio. If the boot
        // failed, the first attempt's error was already shown and its exit
        // code is passed through below.
        command.stderr(Stdio::inherit());
        return execute(command);
    }
    Ok(exit_code(captured.status))
}

/// Result of a command run with [`run_captured`]: the exit status and the
/// stderr it produced (bounded to a tail large enough to recognize the
/// not-started hint).
struct CapturedOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

/// Runs `command` with stderr piped, forwarding every chunk to silo's own
/// stderr as it arrives while keeping a bounded copy of the tail for later
/// inspection. Only stderr is piped — stdout stays inherited — so reading it
/// to EOF before waiting cannot deadlock: the child only ever blocks on a
/// full stderr pipe, which this loop keeps drained.
///
/// # Errors
///
/// Returns an error when the command cannot be spawned or waited on.
fn run_captured(command: &mut Command) -> Result<CapturedOutput> {
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error)?;
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let mut captured = Vec::new();
    // `io::stderr()` is line-buffered, so flush after every chunk or
    // newline-free progress output would not show up as it arrives.
    let mut out = io::stderr();
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &chunk[..n];
                let _ = out.write_all(chunk);
                let _ = out.flush();
                captured.extend_from_slice(chunk);
                trim_captured(&mut captured);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let status = child.wait().map_err(spawn_error)?;
    Ok(CapturedOutput {
        status,
        stderr: captured,
    })
}

/// Keeps only the tail of the captured stderr: the retry decision only looks
/// at the final error text, and a chatty build must not accumulate unbounded
/// output in memory.
fn trim_captured(captured: &mut Vec<u8>) {
    const MAX_CAPTURED_STDERR: usize = 256 * 1024;
    if captured.len() > MAX_CAPTURED_STDERR {
        captured.drain(..captured.len() - MAX_CAPTURED_STDERR);
    }
}

fn exit_code(status: ExitStatus) -> ExitCode {
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
        .clamp(0, 255);
    // `code` is clamped to 0..=255 above, so the conversion never fails.
    ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
}

fn build_dir() -> PathBuf {
    let pid = std::process::id();
    std::env::temp_dir().join(format!("silo-build-{pid}"))
}

fn build_command(dockerfile: &Path, context: &Path) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command
        .arg("build")
        .arg("--file")
        .arg(dockerfile)
        .arg("--tag")
        .arg(IMAGE_TAG)
        .arg("--pull")
        .arg("--no-cache")
        .arg(context);
    command
}

/// Returns the host user's uid and gid, forwarded to the container so files
/// created there are owned by the host user.
///
/// # Errors
///
/// Returns an error when `id` cannot be run or one of the ids cannot be read.
fn host_ids() -> Result<HostIds> {
    Ok(HostIds {
        uid: id_of("-u")?,
        gid: id_of("-g")?,
    })
}

fn id_of(flag: &str) -> Result<String> {
    let output = Command::new("id")
        .arg(flag)
        .output()
        .with_context(|| format!("failed to run `id {flag}`"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`id {flag}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Builds the create half of an isolated one-shot container lifecycle.
///
/// # Errors
///
/// Returns an error when the project root has no name (e.g. `/`), or its path
/// or a mount's path cannot be expressed in a mount specification.
#[allow(clippy::too_many_arguments)]
fn isolated_create_command(
    interactive: bool,
    project_root: &Path,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
    id: &str,
    command: &[OsString],
    shell: Option<Shell>,
) -> Result<Command> {
    let shared_dir = shared_dir_name(project_root)?;
    let project = Project {
        root: project_root.to_path_buf(),
        workdir: shared_dir.clone(),
        id: id.to_string(),
    };
    let identity = container_identity(
        &project,
        IMAGE_TAG,
        host_ids,
        config_mounts,
        resources,
        LABEL_ISOLATED_VALUE,
        command,
    );
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("create")
        .arg("--name")
        .arg(id)
        .arg("--rm")
        .arg("-i");
    if interactive {
        // Allocating a pty without a terminal fails with ENOTTY.
        run.arg("-t");
    }
    append_resources(&mut run, resources);
    append_identity_labels(&mut run, &identity, LABEL_ISOLATED_VALUE);
    append_creation_mounts(&mut run, project_root, &shared_dir, config_mounts)?;
    append_host_ids(&mut run, host_ids);
    append_sudo_access(&mut run, resources.sudo);
    if let Some(shell) = shell {
        run.arg("--env").arg(format!("SHELL={}", shell.path()));
    }
    run.arg(IMAGE_TAG);
    if command.is_empty() {
        if let Some(shell) = shell {
            run.arg(shell.path());
        }
    } else {
        run.args(command);
    }
    Ok(run)
}

fn isolated_start_command(id: &str) -> Command {
    let mut start = Command::new(CONTAINER_BIN);
    start.arg("start").arg("--attach").arg("--interactive");
    start.arg(id);
    start
}

/// Builds the detached creation command for a shared project container.
fn create_command(
    project: &Project,
    image_digest: &str,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    resources: &Container,
    cidfile: &Path,
) -> Result<Command> {
    let identity = container_identity(
        project,
        image_digest,
        host_ids,
        config_mounts,
        resources,
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("run")
        .arg("--name")
        .arg(&project.id)
        .arg("--cidfile")
        .arg(cidfile)
        .arg("--rm")
        .arg("-d");
    append_resources(&mut run, resources);
    append_identity_labels(&mut run, &identity, LABEL_SHARED_VALUE);
    append_creation_mounts(&mut run, &project.root, &project.workdir, config_mounts)?;
    append_host_ids(&mut run, host_ids);
    append_sudo_access(&mut run, resources.sudo);
    // Creation verifies the tag immediately before and the resolved digest
    // immediately after this command, closing concurrent retag races.
    run.arg(IMAGE_TAG);
    run.arg(SHARED_INIT_COMMAND);
    Ok(run)
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
    let host_dir = bind_host_path(project_root)?;
    run.arg("-v")
        .arg(format!("{host_dir}:{}", shared_dir.display()))
        .arg("-w")
        .arg(shared_dir);
    // Overlay protected project paths before named mounts, which retain their
    // established ability to override earlier project mounts.
    for entry in &config_mounts.read_only {
        let host = mount_host_path(&entry.host)?;
        let target = project_path_target(shared_dir, &entry.relative);
        run.arg("-v").arg(format!("{host}:{}:ro", target.display()));
    }
    for entry in effective_mounts(&config_mounts.named) {
        let target = mount_argument_path(&entry.dest)?;
        let source = match &entry.source {
            ResolvedMountSource::Host(host) => mount_argument_path(host)?,
            ResolvedMountSource::Managed(mount) => mount_argument_path(&mount.path)?,
        };
        let readonly = match entry.access {
            Permission::ReadOnly => ",readonly",
            Permission::ReadWrite => "",
        };
        run.arg("--mount").arg(format!(
            "type=bind,source={source},target={target}{readonly}"
        ));
    }
    if let Some(forwarding) = &config_mounts.forwarding {
        let source = mount_argument_path(forwarding.source())?;
        run.arg("--env")
            .arg("SILO_INTERNAL_SSH_FORWARDING=1")
            .arg("--mount")
            .arg(format!(
                "type=bind,source={source},target=/run/silo-ssh,readonly"
            ));
    }
    Ok(())
}

/// Adds only the stable IDs required by the shared container's entrypoint.
fn append_host_ids(run: &mut Command, host_ids: &HostIds) {
    run.arg("--env")
        .arg(format!("SILO_UID={}", host_ids.uid))
        .arg("--env")
        .arg(format!("SILO_GID={}", host_ids.gid));
}

/// Grants the built-in user elevation only for containers that request it.
fn append_sudo_access(run: &mut Command, enabled: bool) {
    if enabled {
        run.arg("--env").arg("SILO_SUDO=1");
    }
}

/// Builds one session attachment command for the running shared container.
fn exec_command(
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
    exec.arg(SESSION_WRAPPER_COMMAND).arg(reservation);
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
fn resolve_shell(configured: Option<Shell>, host_shell: Option<&OsStr>) -> Shell {
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

/// Returns the host side of the shared-directory bind specification, rejecting
/// paths the container CLI cannot parse: `:` separates host and container
/// paths, and the spec is built with `format!`, so the path must be valid
/// UTF-8.
///
/// # Errors
///
/// Returns an error when the path is not valid UTF-8 or contains `:`.
fn bind_host_path(project_root: &Path) -> Result<&str> {
    spec_host_path(project_root, "share")
}

/// Like [`bind_host_path`], for configured mount paths.
fn mount_host_path(path: &Path) -> Result<&str> {
    spec_host_path(path, "mount")
}

/// Places a normalized project-relative path below its container project
/// root, treating `.` as the root itself rather than emitting a trailing dot.
fn project_path_target(project_dest: &Path, relative: &Path) -> PathBuf {
    if relative == Path::new(".") {
        project_dest.to_path_buf()
    } else {
        project_dest.join(relative)
    }
}

/// Returns a path suitable for Apple's comma-delimited `--mount` syntax.
fn mount_argument_path(path: &Path) -> Result<&str> {
    path.to_str()
        .filter(|value| !value.contains([',', '=']))
        .ok_or_else(|| {
            anyhow!(
                "cannot mount `{}`: the path must be valid UTF-8 without `,` or `=`",
                path.display()
            )
        })
}

fn spec_host_path<'a>(path: &'a Path, verb: &str) -> Result<&'a str> {
    path.to_str().filter(|p| !p.contains(':')).ok_or_else(|| {
        anyhow!(
            "cannot {verb} `{}`: the path must be valid UTF-8 without `:`",
            path.display()
        )
    })
}

/// Returns where the shared directory lands in the container, i.e. the
/// project root's last component placed inside the `silo` user's home.
///
/// # Errors
///
/// Returns an error when the project root has no name (e.g. `/`).
fn shared_dir_name(project_root: &Path) -> Result<PathBuf> {
    let name = project_root.file_name().ok_or_else(|| {
        anyhow!(
            "cannot share the root directory `{}`",
            project_root.display()
        )
    })?;
    Ok(Path::new(CONTAINER_HOME).join(name))
}

fn spawn_error(err: io::Error) -> anyhow::Error {
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
