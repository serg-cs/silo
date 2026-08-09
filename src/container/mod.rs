use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{Config, Permission, Shared};

/// Name of the image this tool builds and runs.
pub const IMAGE_TAG: &str = "silo:latest";

/// Dockerfile embedded into the executable at compile time.
pub const DOCKERFILE: &str = include_str!("silo.dockerfile");

const CONTAINER_BIN: &str = "container";

/// Prefix of every `--name` silo passes to `container run`.
const CONTAINER_NAME_PREFIX: &str = "silo-";

/// Config subdirectory holding markers, locks, and transient cidfiles.
const CONTAINER_STATE_DIR: &str = "containers";

/// Current on-disk marker schema version.
const MARKER_VERSION: u8 = 1;

/// Amount of the SHA-256 digest used in a shared container ID.
const PROJECT_DIGEST_HEX_LEN: usize = 24;

/// Default command for a shared session when the user supplies none.
const DEFAULT_SESSION_COMMAND: &str = "nu";

/// Init command that keeps a shared container available for exec sessions.
const SHARED_INIT_COMMAND: [&str; 2] = ["sleep", "infinity"];

/// Home directory of the container's `silo` user; the shared project
/// directory is mounted into it as `<home>/<project-name>`.
const CONTAINER_HOME: &str = "/home/silo";

/// One configured shared mount resolved for this run: `host` is the
/// canonical source path on this machine, `dest` where it is mounted inside
/// the container, and `permission` how it may be used.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedShared {
    host: PathBuf,
    dest: PathBuf,
    permission: Permission,
}

/// Config-driven mounts applied on top of the shared project directory: the
/// optional read-only `.git` and the configured shared mounts.
#[derive(Default)]
struct ConfigMounts {
    git: Option<PathBuf>,
    shared: Vec<ResolvedShared>,
}

/// Stable identity and container path for the current project.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Project {
    root: PathBuf,
    workdir: PathBuf,
    id: String,
}

impl Project {
    /// Resolves the current project through symlinks before deriving its ID.
    fn current() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to determine current directory")?;
        Self::from_path(&cwd)
    }

    /// Pure-path entry point used to validate canonical identity behavior.
    fn from_path(cwd: &Path) -> Result<Self> {
        let root = fs::canonicalize(cwd)
            .with_context(|| format!("failed to resolve project directory `{}`", cwd.display()))?;
        // Validate before persisting the path in JSON or a volume spec.
        volume_host_path(&root)?;
        let workdir = shared_dir_name(&root)?;
        let id = project_container_id(&root);
        Ok(Self { root, workdir, id })
    }
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

/// Durable ownership metadata for a shared container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ContainerMarker {
    version: u8,
    container_id: String,
    project: PathBuf,
    creator_pid: u32,
}

impl ContainerMarker {
    fn new(project: &Project) -> Self {
        Self {
            version: MARKER_VERSION,
            container_id: project.id.clone(),
            project: project.root.clone(),
            creator_pid: std::process::id(),
        }
    }
}

/// Exclusive per-project file lock held only while container state changes.
struct ProjectLock(File);

impl ProjectLock {
    /// Acquires the project lock, optionally returning immediately when busy.
    fn acquire(path: &Path, nonblocking: bool) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open project lock `{}`", path.display()))?;
        let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(Some(Self(file)));
        }
        let err = io::Error::last_os_error();
        let code = err.raw_os_error();
        if nonblocking && (code == Some(libc::EWOULDBLOCK) || code == Some(libc::EAGAIN)) {
            return Ok(None);
        }
        Err(err).with_context(|| format!("failed to lock `{}`", path.display()))
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        // Closing the file also releases the lock; unlock explicitly for clarity.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Returns the host side of the read-only `.git` mount: the canonical path
/// of the project's `.git` when the config enables the mount and `.git`
/// resolves to a real path inside the project. A symlink escaping the
/// project is not mounted, so no host path outside it becomes visible.
fn git_mount_host(cwd: &Path, read_only_git: bool) -> Option<PathBuf> {
    if !read_only_git {
        return None;
    }
    let root = fs::canonicalize(cwd).ok()?;
    mount_host(&cwd.join(".git"), &root)
}

/// Returns the canonical path of `path` when it resolves to a real path
/// inside `root_canonical`, or `None` otherwise (a symlink pointing away
/// must not become a mount, since the container runtime would mount the
/// link's target).
fn mount_host(path: &Path, root_canonical: &Path) -> Option<PathBuf> {
    let host = fs::canonicalize(path).ok()?;
    host.starts_with(root_canonical).then_some(host)
}

/// Resolves the configured shared mounts into mounts for this run: expands
/// a leading `~` in each source to the home directory and canonicalizes it
/// (symlinks mount their target), keeping the
/// configured container target and permission. Entries keep their config
/// order, so a later shared mount can override an earlier one at the same
/// target.
///
/// # Errors
///
/// Returns an error naming the shared mount when its source does not exist
/// or cannot be resolved (e.g. a broken symlink).
fn resolve_shared(shared: &[Shared], home: Option<&Path>) -> Result<Vec<ResolvedShared>> {
    let mut resolved = Vec::with_capacity(shared.len());
    for entry in shared {
        let host = fs::canonicalize(expand_tilde(&entry.source, home)).with_context(|| {
            format!(
                "cannot resolve source `{}` for the shared mount at `{}` (missing path or broken symlink?)",
                entry.source.display(),
                entry.target.display()
            )
        })?;
        resolved.push(ResolvedShared {
            host,
            dest: entry.target.clone(),
            permission: entry.permission,
        });
    }
    Ok(resolved)
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

/// Returns warnings for shared mounts whose intent a later mount silently
/// defeats: a read-write shared mount at or under the `.git` target lets
/// tools in the container modify version control state despite the
/// read-only `.git` mount.
fn mount_conflicts(
    project_dest: &Path,
    git_mounted: bool,
    shared: &[ResolvedShared],
) -> Vec<String> {
    let git_dest = git_mounted.then(|| project_dest.join(".git"));
    let mut warnings = Vec::new();
    for entry in shared {
        if entry.permission == Permission::ReadWrite
            && git_dest
                .as_deref()
                .is_some_and(|git_dest| entry.dest.starts_with(git_dest))
        {
            warnings.push(format!(
                "the read-write shared mount of `{}` at `{}` overlaps the read-only `.git` mount, so tools in the container can modify version control state",
                entry.host.display(),
                entry.dest.display()
            ));
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
}

impl Drop for BuildDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Builds the image: the embedded Dockerfile by default, or the Dockerfile
/// configured in `[image] dockerfile`, which is then the user's own image.
/// When the container system has not been started, boots it first so the
/// build does not fail and get retried for that.
///
/// # Errors
///
/// Returns an error when the configured Dockerfile path is empty, missing or
/// not a file, when the container CLI is missing, when the Dockerfile cannot
/// be written, or when the build itself fails.
pub fn build_image(config: &Config) -> Result<ExitCode> {
    if let Some(dockerfile) = &config.image.dockerfile {
        validate_dockerfile(dockerfile)?;
        return execute_build(&mut build_command(
            dockerfile,
            dockerfile_context(dockerfile),
        ));
    }
    let build_dir = BuildDir::create()?;
    fs::write(build_dir.dockerfile(), DOCKERFILE).context("failed to write Dockerfile")?;
    execute_build(&mut build_command(
        &build_dir.dockerfile(),
        build_dir.path(),
    ))
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

/// Runs a command in this project's shared container, or in a one-shot
/// foreground container when `isolated` is set or the configured image is
/// custom. Custom images retain the image-defined user, command, and runtime
/// filesystem contract through the established one-shot lifecycle. Shared
/// runs serialize only ensure operations, then release the project lock before
/// attaching an exec session so any number of sessions can run concurrently.
///
/// # Errors
///
/// Returns an error when setup, state inspection, container creation, or the
/// attached process fails.
pub fn run_image(config: &Config, command: &[OsString], isolated: bool) -> Result<ExitCode> {
    if uses_isolated_lifecycle(config, isolated) {
        require_image()?;
        return run_isolated(config, command);
    }
    run_shared(config, command)
}

/// Custom images cannot rely on the built-in image's shared-container user,
/// home, shell, or keeper process, so preserve their image-agnostic lifecycle.
fn uses_isolated_lifecycle(config: &Config, isolated: bool) -> bool {
    isolated || config.image.dockerfile.is_some()
}

/// Runs the existing ephemeral `container run --rm` lifecycle.
fn run_isolated(config: &Config, command: &[OsString]) -> Result<ExitCode> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let ids = host_ids()?;
    let shared = resolve_shared(
        &config.shared,
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?;
    let id = isolated_container_id();
    // Build the command first: it only fails on path validation, before any
    // container exists or a marker is written.
    let git_mount = git_mount_host(&cwd, config.read_only_git);
    let config_mounts = ConfigMounts {
        git: git_mount,
        shared,
    };
    let mut run = isolated_run_command(
        std::io::stdin().is_terminal(),
        &cwd,
        &ids,
        &config_mounts,
        &id,
        command,
    )?;
    // Warn about shared mounts whose intent a later mount defeats, e.g. a
    // read-write shared mount overlapping the read-only `.git`.
    let shared_dir = shared_dir_name(&cwd)?;
    for warning in mount_conflicts(
        &shared_dir,
        config_mounts.git.is_some(),
        &config_mounts.shared,
    ) {
        eprintln!("warning: {warning}");
    }
    sweep_stale_containers(None);
    install_signal_handlers()?;
    register_container(&id);
    // Captured before the child starts, so it holds the pre-raw-mode state.
    let terminal = SavedTerminal::capture();
    let mut child = run.spawn().map_err(spawn_error)?;
    let pid = libc::pid_t::try_from(child.id()).expect("child pid fits in pid_t");
    let status = wait_for_child(&mut child, pid);
    if let Some(terminal) = &terminal {
        terminal.restore();
    }
    cleanup_container(&id);
    status.map(exit_code)
}

/// Ensures the shared project container and attaches one exec session.
fn run_shared(config: &Config, command: &[OsString]) -> Result<ExitCode> {
    let project = Project::current()?;

    // Sweep unrelated leftovers before locking this project's ensure path.
    sweep_stale_containers(Some(&project.id));
    let state_dir = runtime_state_dir()?;
    let lock_path = lock_path(&state_dir, &project.id);
    let lock = ProjectLock::acquire(&lock_path, false)?
        .ok_or_else(|| anyhow!("project lock unexpectedly unavailable"))?;
    ensure_shared_container(&project, config, &state_dir)?;
    drop(lock);

    // The attached process owns the terminal, but not the shared container.
    let mut exec = exec_command(std::io::stdin().is_terminal(), &project, command);
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
fn resolve_config_mounts(config: &Config, cwd: &Path) -> Result<ConfigMounts> {
    Ok(ConfigMounts {
        git: git_mount_host(cwd, config.read_only_git),
        shared: resolve_shared(
            &config.shared,
            std::env::var_os("HOME").as_deref().map(Path::new),
        )?,
    })
}

/// Reports mount-order conflicts before creating a container.
fn warn_mount_conflicts(project: &Project, config_mounts: &ConfigMounts) {
    for warning in mount_conflicts(
        &project.workdir,
        config_mounts.git.is_some(),
        &config_mounts.shared,
    ) {
        eprintln!("warning: {warning}");
    }
}

/// Stops and deletes the shared container for the current project.
///
/// An absent container is an idempotent success.
pub fn stop_image() -> Result<ExitCode> {
    let project = Project::current()?;
    let state_dir = runtime_state_dir()?;
    let lock = ProjectLock::acquire(&lock_path(&state_dir, &project.id), false)?
        .ok_or_else(|| anyhow!("project lock unexpectedly unavailable"))?;
    if let Some(marker) = read_marker(&state_dir, &project.id)?
        && marker.project != project.root
    {
        return Err(anyhow!(
            "refusing to stop `{}` because its marker belongs to `{}`",
            project.id,
            marker.project.display()
        ));
    }
    let state = settled_container_state(&project.id)?;

    // Stop gracefully when needed, then delete the inert container record.
    if state == ContainerState::Running {
        run_checked(
            Command::new(CONTAINER_BIN).args(["stop", &project.id]),
            "stop shared container",
        )?;
    }
    if matches!(state, ContainerState::Stopping | ContainerState::Unknown) {
        return Err(anyhow!(
            "container `{}` is in an unusable state and was not deleted",
            project.id
        ));
    }
    if state != ContainerState::Absent {
        run_checked(
            Command::new(CONTAINER_BIN).args(["delete", &project.id]),
            "delete shared container",
        )?;
    }
    remove_shared_state(&state_dir, &project.id);
    drop(lock);
    Ok(ExitCode::SUCCESS)
}

/// Returns the legacy one-shot ID, `<prefix><pid>`.
fn isolated_container_id() -> String {
    format!("{CONTAINER_NAME_PREFIX}{}", std::process::id())
}

/// Returns the deterministic shared ID derived from a canonical project path.
fn project_container_id(project: &Path) -> String {
    let digest = Sha256::digest(project.as_os_str().as_bytes());
    let hex = format!("{digest:x}");
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

/// Returns the state directory holding the container markers, derived from
/// the config directory. `None` when no home directory can be determined.
fn container_state_dir() -> Option<PathBuf> {
    container_state_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Pure version of [`container_state_dir`], taking the environment values
/// as arguments so the resolution rules are testable without mutating the
/// process environment.
fn container_state_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    crate::config::config_dir_from(xdg, home).map(|dir| dir.join(CONTAINER_STATE_DIR))
}

/// Returns a writable state directory even when no home directory is known.
fn runtime_state_dir() -> Result<PathBuf> {
    let dir = container_state_dir().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("silo-{}", unsafe { libc::geteuid() }))
    });
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir `{}`", dir.display()))?;
    Ok(dir)
}

fn marker_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn lock_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.lock"))
}

fn cidfile_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.cid"))
}

fn marker_temp_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json.tmp"))
}

/// Persists a shared marker while its project lock is held.
fn write_marker(dir: &Path, marker: &ContainerMarker) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(marker).context("failed to encode container marker")?;
    let path = marker_path(dir, &marker.container_id);
    let temp = marker_temp_path(dir, &marker.container_id);

    // Fully persist the replacement before the atomic rename, so interruption
    // cannot leave the live marker truncated or partially encoded.
    let write_result = (|| -> Result<()> {
        let mut file = File::create(&temp)
            .with_context(|| format!("failed to create marker `{}`", temp.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("failed to write marker `{}`", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync marker `{}`", temp.display()))?;
        fs::rename(&temp, &path).with_context(|| {
            format!(
                "failed to replace marker `{}` from temporary `{}`",
                path.display(),
                temp.display()
            )
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

/// Loads and validates a shared marker when one exists.
fn read_marker(dir: &Path, id: &str) -> Result<Option<ContainerMarker>> {
    let path = marker_path(dir, id);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read marker `{}`", path.display()));
        }
    };
    let marker: ContainerMarker = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid container marker `{}`", path.display()))?;
    if marker.version != MARKER_VERSION || marker.container_id != id {
        return Err(anyhow!(
            "container marker `{}` is inconsistent",
            path.display()
        ));
    }
    Ok(Some(marker))
}

/// Removes the marker and temporary cidfile after confirmed deletion.
fn remove_shared_state(dir: &Path, id: &str) {
    let _ = fs::remove_file(marker_path(dir, id));
    let _ = fs::remove_file(marker_temp_path(dir, id));
    let _ = fs::remove_file(cidfile_path(dir, id));
}

/// Ensures the deterministic container exists and is running.
fn ensure_shared_container(project: &Project, config: &Config, state_dir: &Path) -> Result<()> {
    let marker = read_marker(state_dir, &project.id)?;
    if let Some(marker) = &marker
        && marker.project != project.root
    {
        return Err(anyhow!(
            "container ID `{}` belongs to project `{}`, not `{}`",
            project.id,
            marker.project.display(),
            project.root.display()
        ));
    }

    match settled_container_state(&project.id)? {
        ContainerState::Absent => create_missing_shared_container(project, config, state_dir),
        ContainerState::Stopped => {
            require_marker(marker.as_ref(), project)?;
            run_checked(
                Command::new(CONTAINER_BIN).args(["start", &project.id]),
                "start shared container",
            )?;
            if inspect_container(&project.id)? == ContainerState::Running {
                Ok(())
            } else {
                Err(anyhow!("shared container `{}` did not start", project.id))
            }
        }
        ContainerState::Running => {
            require_marker(marker.as_ref(), project)?;
            Ok(())
        }
        ContainerState::Stopping | ContainerState::Unknown => Err(anyhow!(
            "container `{}` did not reach a usable state",
            project.id
        )),
    }
}

/// Resolves creation-only inputs and creates an absent shared container.
fn create_missing_shared_container(
    project: &Project,
    config: &Config,
    state_dir: &Path,
) -> Result<()> {
    // Existing containers do not need their original image tag or mounts;
    // only the absent branch performs these creation checks.
    require_image()?;
    let ids = host_ids()?;
    let config_mounts = resolve_config_mounts(config, &project.root)?;
    warn_mount_conflicts(project, &config_mounts);
    create_shared_container(project, &ids, &config_mounts, state_dir)
}

/// Rejects markerless deterministic containers instead of adopting them.
fn require_marker(marker: Option<&ContainerMarker>, project: &Project) -> Result<()> {
    if marker.is_some() {
        return Ok(());
    }
    Err(anyhow!(
        "container `{}` exists without silo state; run `silo stop` to remove it safely",
        project.id
    ))
}

/// Waits briefly for a stopping container before ensure decides what to do.
fn settled_container_state(id: &str) -> Result<ContainerState> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = inspect_container(id)?;
        if state != ContainerState::Stopping || Instant::now() >= deadline {
            return Ok(state);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Creates and verifies a detached shared container.
fn create_shared_container(
    project: &Project,
    ids: &HostIds,
    config_mounts: &ConfigMounts,
    state_dir: &Path,
) -> Result<()> {
    let cidfile = cidfile_path(state_dir, &project.id);
    let _ = fs::remove_file(&cidfile);
    let mut create = create_command(project, ids, config_mounts, &cidfile)?;
    write_marker(state_dir, &ContainerMarker::new(project))?;

    // The provisional marker makes an interrupted creation discoverable.
    let output = match create.output() {
        Ok(output) => output,
        Err(err) => {
            remove_shared_state(state_dir, &project.id);
            return Err(spawn_error(err));
        }
    };
    if !output.status.success() {
        cleanup_failed_run(state_dir, &project.id, &cidfile);
        return Err(anyhow!(
            "failed to create shared container `{}`: {}",
            project.id,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let recorded = match fs::read_to_string(&cidfile) {
        Ok(recorded) => recorded,
        Err(err) => {
            cleanup_failed_creation(state_dir, &project.id);
            return Err(err)
                .with_context(|| format!("failed to read cidfile `{}`", cidfile.display()));
        }
    };
    if recorded.trim() != project.id {
        cleanup_failed_creation(state_dir, &project.id);
        return Err(anyhow!(
            "container runtime wrote unexpected ID `{}` to `{}`",
            recorded.trim(),
            cidfile.display()
        ));
    }
    let _ = fs::remove_file(&cidfile);
    let state = match inspect_container(&project.id) {
        Ok(state) => state,
        Err(err) => {
            cleanup_failed_creation(state_dir, &project.id);
            return Err(err);
        }
    };
    if state != ContainerState::Running {
        cleanup_failed_creation(state_dir, &project.id);
        return Err(anyhow!(
            "shared container `{}` stopped during startup",
            project.id
        ));
    }
    Ok(())
}

/// Cleans a failed `run` only when its cidfile proves this invocation created it.
fn cleanup_failed_run(state_dir: &Path, id: &str, cidfile: &Path) {
    let created_here = fs::read_to_string(cidfile).is_ok_and(|value| value.trim() == id);
    if created_here {
        cleanup_failed_creation(state_dir, id);
    } else {
        remove_shared_state(state_dir, id);
    }
}

/// Removes a partially created container and clears state only once gone.
fn cleanup_failed_creation(state_dir: &Path, id: &str) {
    match delete_container(id) {
        Ok(()) => remove_shared_state(state_dir, id),
        Err(err) => {
            eprintln!("warning: could not remove partially created container `{id}`: {err:#}");
        }
    }
}

/// Inspects a container, booting the container system and retrying once when needed.
fn inspect_container(id: &str) -> Result<ContainerState> {
    let (state, stderr) = probe_container(id)?;
    if state.is_none() && system_not_started(&stderr) && start_container_system() {
        let (state, stderr) = probe_container(id)?;
        return state.ok_or_else(|| container_inspect_error(id, &stderr));
    }
    state.ok_or_else(|| container_inspect_error(id, &stderr))
}

/// Runs one raw container inspection; not-found is represented as absence.
fn probe_container(id: &str) -> Result<(Option<ContainerState>, String)> {
    let output = Command::new(CONTAINER_BIN)
        .args(["inspect", id])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        return Ok((Some(parse_container_state(&output.stdout, id)?), stderr));
    }
    if stderr.to_lowercase().contains("not found") {
        return Ok((Some(ContainerState::Absent), stderr));
    }
    Ok((None, stderr))
}

fn container_inspect_error(id: &str, stderr: &str) -> anyhow::Error {
    anyhow!("could not inspect container `{id}`: {stderr}")
}

/// Parses current `status.state` and legacy flat `status` inspect shapes.
fn parse_container_state(stdout: &[u8], id: &str) -> Result<ContainerState> {
    let items: Vec<Value> =
        serde_json::from_slice(stdout).context("invalid container inspect JSON")?;
    let item = items
        .iter()
        .find(|item| {
            item.get("id").and_then(Value::as_str) == Some(id)
                || item.pointer("/configuration/id").and_then(Value::as_str) == Some(id)
        })
        .ok_or_else(|| anyhow!("container inspect did not return `{id}`"))?;
    let status = item
        .pointer("/status/state")
        .and_then(Value::as_str)
        .or_else(|| item.get("status").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("container inspect omitted the state for `{id}`"))?;
    Ok(match status {
        "running" => ContainerState::Running,
        "stopped" => ContainerState::Stopped,
        "stopping" => ContainerState::Stopping,
        _ => ContainerState::Unknown,
    })
}

/// Executes a non-interactive lifecycle command and includes stderr on failure.
fn run_checked(command: &mut Command, action: &str) -> Result<()> {
    let output = command.output().map_err(spawn_error)?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "failed to {action}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Records this run's container ID in the state directory before the child
/// starts, so a silo killed hard (e.g. `kill -9`) leaves a marker the next
/// run sweeps. Best effort: a warning is printed instead of failing the
/// run.
fn register_container(id: &str) {
    let Some(dir) = container_state_dir() else {
        return;
    };
    if let Err(err) = register_container_in(&dir, id) {
        eprintln!(
            "warning: could not record container `{id}` at `{}`: {err:#}",
            dir.join(id).display()
        );
    }
}

/// Writes the marker file for container `id` into `dir`.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or the marker
/// cannot be written.
fn register_container_in(dir: &Path, id: &str) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create state dir `{}`", dir.display()))?;
    fs::write(dir.join(id), format!("{id}\n"))
        .with_context(|| format!("failed to write marker for container `{id}`"))
}

/// Removes the state marker of a container that is confirmed gone.
fn unregister_container(id: &str) {
    if let Some(dir) = container_state_dir() {
        unregister_container_in(&dir, id);
    }
}

/// Removes the marker file for container `id` from `dir`, if present.
fn unregister_container_in(dir: &Path, id: &str) {
    let _ = fs::remove_file(dir.join(id));
}

/// Sweeps dead-owner shared markers without touching running containers.
fn sweep_stale_containers(skip_id: Option<&str>) {
    let Ok(dir) = runtime_state_dir() else {
        return;
    };
    sweep_shared_in(&dir, skip_id, inspect_container, delete_container);
    sweep_legacy_in(&dir, delete_container);
}

/// Testable shared-marker sweep implementation.
fn sweep_shared_in(
    dir: &Path,
    skip_id: Option<&str>,
    mut inspect: impl FnMut(&str) -> Result<ContainerState>,
    mut delete: impl FnMut(&str) -> Result<()>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(marker) = serde_json::from_slice::<ContainerMarker>(&bytes) else {
            eprintln!(
                "warning: ignoring invalid container marker `{}`",
                path.display()
            );
            continue;
        };
        let id = marker.container_id.as_str();
        if marker.version != MARKER_VERSION
            || path.file_stem().and_then(OsStr::to_str) != Some(id)
            || !id.starts_with(CONTAINER_NAME_PREFIX)
        {
            eprintln!(
                "warning: ignoring inconsistent container marker `{}`",
                path.display()
            );
            continue;
        }
        if skip_id == Some(id) {
            continue;
        }
        let Ok(pid) = libc::pid_t::try_from(marker.creator_pid) else {
            continue;
        };
        if owner_alive(pid) {
            continue;
        }
        let Ok(Some(_lock)) = ProjectLock::acquire(&lock_path(dir, id), true) else {
            continue;
        };
        match inspect(id) {
            Ok(ContainerState::Absent) => remove_shared_state(dir, id),
            Ok(ContainerState::Stopped) => match delete(id) {
                Ok(()) => remove_shared_state(dir, id),
                Err(err) => {
                    eprintln!("warning: could not remove stale container `{id}`: {err:#}");
                }
            },
            Ok(ContainerState::Running | ContainerState::Stopping | ContainerState::Unknown) => {}
            Err(err) => eprintln!("warning: could not inspect stale container `{id}`: {err:#}"),
        }
    }
}

/// Preserves cleanup for PID-named markers written by older isolated runs.
fn sweep_legacy_in(dir: &Path, mut delete: impl FnMut(&str) -> Result<()>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let id = entry.file_name();
        let Some(id) = id.to_str() else {
            continue;
        };
        let Some(pid) = id
            .strip_prefix(CONTAINER_NAME_PREFIX)
            .and_then(|rest| rest.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if owner_alive(pid) {
            continue;
        }
        match delete(id) {
            Ok(()) => unregister_container_in(dir, id),
            Err(err) => eprintln!("warning: could not remove stale container `{id}`: {err:#}"),
        }
    }
}

/// Returns whether the process `pid` is alive, i.e. the silo that started
/// the container is still running and its container must not be touched.
fn owner_alive(pid: libc::pid_t) -> bool {
    // Signal 0 only checks for existence; no signal is delivered.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // The process exists but belongs to another user.
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

/// Removes this run's container after the `container run` child has exited,
/// whatever the reason, and forgets its ID once it is gone. `--rm` already
/// removes the container on a normal exit, so this is a safety net for runs
/// killed or crashed; a leftover stays marked and is swept by the next run.
fn cleanup_container(id: &str) {
    match delete_container(id) {
        Ok(()) => unregister_container(id),
        Err(err) => eprintln!(
            "warning: could not remove container `{id}`: {err:#} (it will be removed at the next `silo run`)"
        ),
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
/// (the VM behind the CLI) has not been started. The CLI's own hint is
/// "Ensure container system service has been started with `container system
/// start`."; older releases worded it as "container system start has not
/// been run".
fn system_not_started(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    stderr.contains("container system start")
        && (stderr.contains("has been started") || stderr.contains("has not been run"))
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

/// Runs `container image inspect`, returning whether the image exists and the
/// probe's stderr (which explains why the check failed, when it did). When
/// the probe fails because the container system has not been started, boots
/// it first and probes again once; if the boot fails, the probe's stderr is
/// reported unchanged so the user still sees the original failure.
fn inspect_image() -> Result<(bool, String)> {
    inspect_image_with(probe_image, start_container_system)
}

/// Requires the built image only for operations that create a container.
fn require_image() -> Result<()> {
    let (exists, stderr) = inspect_image()?;
    if exists {
        Ok(())
    } else {
        Err(inspect_error(&stderr))
    }
}

/// The probe/boot/reprobe logic behind [`inspect_image`], separated so tests
/// can substitute fakes for the `container` CLI.
fn inspect_image_with(
    probe: impl Fn() -> Result<(bool, String)>,
    boot: impl Fn() -> bool,
) -> Result<(bool, String)> {
    let (exists, stderr) = probe()?;
    if !exists && system_not_started(&stderr) && boot() {
        return probe();
    }
    Ok((exists, stderr))
}

/// Runs the raw `container image inspect` probe, without booting anything.
fn probe_image() -> Result<(bool, String)> {
    let output = Command::new(CONTAINER_BIN)
        .args(["image", "inspect", IMAGE_TAG])
        .stdout(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Ok((output.status.success(), stderr))
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
    probe: impl Fn() -> Result<(bool, String)>,
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

/// Builds the legacy one-shot `container run --rm` command.
///
/// # Errors
///
/// Returns an error when the current directory has no name (e.g. `/`), or
/// its path or a mount's path cannot be expressed in a volume spec.
fn isolated_run_command(
    interactive: bool,
    cwd: &Path,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    id: &str,
    command: &[OsString],
) -> Result<Command> {
    let shared_dir = shared_dir_name(cwd)?;
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("run").arg("--name").arg(id).arg("--rm").arg("-i");
    if interactive {
        // Allocating a pty without a terminal fails with ENOTTY.
        run.arg("-t");
    }
    append_creation_mounts(&mut run, cwd, &shared_dir, config_mounts)?;
    append_host_ids(&mut run, host_ids);
    run.arg(IMAGE_TAG).args(command);
    Ok(run)
}

/// Builds the detached creation command for a shared project container.
fn create_command(
    project: &Project,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    cidfile: &Path,
) -> Result<Command> {
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("run")
        .arg("--name")
        .arg(&project.id)
        .arg("--cidfile")
        .arg(cidfile)
        .arg("-d");
    append_creation_mounts(&mut run, &project.root, &project.workdir, config_mounts)?;
    append_host_ids(&mut run, host_ids);
    run.arg(IMAGE_TAG).args(SHARED_INIT_COMMAND);
    Ok(run)
}

/// Adds mounts in their established override order and selects the workdir.
fn append_creation_mounts(
    run: &mut Command,
    cwd: &Path,
    shared_dir: &Path,
    config_mounts: &ConfigMounts,
) -> Result<()> {
    let host_dir = volume_host_path(cwd)?;
    run.arg("-v")
        .arg(format!("{host_dir}:{}", shared_dir.display()))
        .arg("-w")
        .arg(shared_dir);
    if let Some(host) = &config_mounts.git {
        // The project's `.git` is mounted read-only on top of the
        // read-write project mount, so tools in the container cannot modify
        // version control state.
        let host = mount_host_path(host)?;
        run.arg("-v")
            .arg(format!("{host}:{}:ro", shared_dir.join(".git").display()));
    }
    for entry in &config_mounts.shared {
        // Configured shared mounts: read-only mounts get the `:ro` suffix,
        // read-write mounts use the volume default.
        let host = mount_host_path(&entry.host)?;
        let spec = match entry.permission {
            Permission::ReadOnly => format!("{host}:{}:ro", entry.dest.display()),
            Permission::ReadWrite => format!("{host}:{}", entry.dest.display()),
        };
        run.arg("-v").arg(spec);
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

/// Builds one session attachment command for the running shared container.
fn exec_command(interactive: bool, project: &Project, command: &[OsString]) -> Command {
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
        .arg(&project.id);
    if command.is_empty() {
        exec.arg(DEFAULT_SESSION_COMMAND);
    } else {
        exec.args(command);
    }
    exec
}

/// Returns the host side of the shared-directory volume spec, rejecting
/// paths the container CLI cannot parse: `:` separates host and container
/// paths, and the spec is built with `format!`, so the path must be valid
/// UTF-8.
///
/// # Errors
///
/// Returns an error when the path is not valid UTF-8 or contains `:`.
fn volume_host_path(cwd: &Path) -> Result<&str> {
    spec_host_path(cwd, "share")
}

/// Like [`volume_host_path`], for the shared and `.git` mount paths.
fn mount_host_path(path: &Path) -> Result<&str> {
    spec_host_path(path, "mount")
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
/// current directory's last component placed inside the `silo` user's home.
///
/// # Errors
///
/// Returns an error when the current directory has no name (e.g. `/`).
fn shared_dir_name(cwd: &Path) -> Result<PathBuf> {
    let name = cwd
        .file_name()
        .ok_or_else(|| anyhow!("cannot share the root directory `{}`", cwd.display()))?;
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
