use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{Config, Permission, Shared};

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

const CONTAINER_BIN: &str = "container";

/// File that explicitly marks a directory as a Silo project root.
const PROJECT_MARKER: &str = ".silo.toml";

/// Directory that implicitly marks a project root when no Silo marker exists.
const GIT_DIR: &str = ".git";

/// Prefix of every `--name` silo passes to `container run`.
const CONTAINER_NAME_PREFIX: &str = "silo-";

/// Amount of the SHA-256 digest used in a shared container ID.
const PROJECT_DIGEST_HEX_LEN: usize = 24;

/// Default command for a shared session when the user supplies none.
const DEFAULT_SESSION_COMMAND: &str = "nu";

/// PID 1 for shared containers; it exits when the final guest-side session
/// lease closes.
const SHARED_INIT_COMMAND: &str = "/usr/local/bin/silo-supervisor";

/// Guest wrapper that holds a shared lease for the command and all children.
const SESSION_WRAPPER_COMMAND: &str = "/usr/local/bin/silo-session";
const SESSION_RESERVE_COMMAND: &str = "/usr/local/bin/silo-reserve";
const GUEST_READY_PATH: &str = "/run/silo/ready";

const LABEL_OWNER: &str = "dev.silo.owner";
const LABEL_SCHEMA: &str = "dev.silo.schema";
const LABEL_PROJECT: &str = "dev.silo.project";
const LABEL_LIFECYCLE: &str = "dev.silo.lifecycle";
const LABEL_SPEC: &str = "dev.silo.spec";
const LABEL_OWNER_VALUE: &str = "silo";
const LABEL_SCHEMA_VALUE: &str = "1";
const LABEL_SHARED_VALUE: &str = "shared";
const LABEL_ISOLATED_VALUE: &str = "isolated";
const LIFECYCLE_PROTOCOL_VERSION: &str = "2";

/// Runtime races are retried only for this bounded interval.
const CONFLICT_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const CONFLICT_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const GUEST_READY_TIMEOUT: Duration = Duration::from_mins(1);

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
        let cwd = fs::canonicalize(cwd)
            .with_context(|| format!("failed to resolve project directory `{}`", cwd.display()))?;
        let root = discover_project_root(&cwd);
        // Validate before persisting the path in JSON or a volume spec.
        volume_host_path(&root)?;
        let workdir = shared_dir_name(&root)?;
        let id = project_container_id(&root);
        Ok(Self { root, workdir, id })
    }
}

/// Selects a project root from an already-canonical starting directory.
///
/// An explicit Silo marker anywhere in the ancestor chain takes precedence
/// over every Git directory. Within each marker type, the nearest ancestor
/// wins. Without either marker, the exact starting directory is the project.
fn discover_project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(PROJECT_MARKER).is_file())
        .or_else(|| cwd.ancestors().find(|dir| dir.join(GIT_DIR).is_dir()))
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
    labels: HashMap<String, String>,
}

impl ContainerInspection {
    fn absent() -> Self {
        Self {
            state: ContainerState::Absent,
            labels: HashMap::new(),
        }
    }
}

/// Runtime labels expected on the deterministic shared container.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerIdentity {
    project: String,
    spec: String,
}

/// Returns the host side of the read-only `.git` mount: the canonical path
/// of the project's `.git` when the config enables the mount and `.git`
/// resolves to a real path inside the project. A symlink escaping the
/// project is not mounted, so no host path outside it becomes visible.
fn git_mount_host(project_root: &Path, read_only_git: bool) -> Option<PathBuf> {
    if !read_only_git {
        return None;
    }
    let root = fs::canonicalize(project_root).ok()?;
    mount_host(&project_root.join(GIT_DIR), &root)
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

    fn supervisor(&self) -> PathBuf {
        self.0.join("silo-supervisor.sh")
    }

    fn session_wrapper(&self) -> PathBuf {
        self.0.join("silo-session.sh")
    }

    fn session_reserver(&self) -> PathBuf {
        self.0.join("silo-reserve.sh")
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
    fs::write(build_dir.supervisor(), SUPERVISOR).context("failed to write supervisor")?;
    fs::write(build_dir.session_wrapper(), SESSION_WRAPPER)
        .context("failed to write session wrapper")?;
    fs::write(build_dir.session_reserver(), SESSION_RESERVER)
        .context("failed to write session reserver")?;
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
    if uses_isolated_lifecycle(config, isolated) {
        require_image()?;
        return run_isolated(config, project, command);
    }
    run_shared(config, project, command)
}

/// Custom images cannot rely on the built-in image's shared-container user,
/// home, shell, or supervisor, so preserve their image-agnostic lifecycle.
fn uses_isolated_lifecycle(config: &Config, isolated: bool) -> bool {
    isolated || config.image.dockerfile.is_some()
}

/// Runs the existing ephemeral `container run --rm` lifecycle.
fn run_isolated(config: &Config, project: &Project, command: &[OsString]) -> Result<ExitCode> {
    let ids = host_ids()?;
    let shared = resolve_shared(
        &config.shared,
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?;
    let id = isolated_container_id();
    // Build the command first: it only fails on path validation, before any
    // container exists.
    let git_mount = git_mount_host(&project.root, config.read_only_git);
    let config_mounts = ConfigMounts {
        git: git_mount,
        shared,
    };
    let mut run = isolated_run_command(
        std::io::stdin().is_terminal(),
        &project.root,
        &ids,
        &config_mounts,
        &id,
        command,
    )?;
    // Warn about shared mounts whose intent a later mount defeats, e.g. a
    // read-write shared mount overlapping the read-only `.git`.
    for warning in mount_conflicts(
        &project.workdir,
        config_mounts.git.is_some(),
        &config_mounts.shared,
    ) {
        eprintln!("warning: {warning}");
    }
    sweep_orphaned_isolated_containers(Some(&id));
    install_signal_handlers()?;
    // Captured before the child starts, so it holds the pre-raw-mode state.
    let terminal = SavedTerminal::capture();
    let mut child = run.spawn().map_err(spawn_error)?;
    let pid = libc::pid_t::try_from(child.id()).expect("child pid fits in pid_t");
    let status = wait_for_child(&mut child, pid);
    if let Some(terminal) = &terminal {
        terminal.restore();
    }
    cleanup_isolated_container(&id);
    status.map(exit_code)
}

/// Ensures the shared project container and attaches one exec session.
fn run_shared(config: &Config, project: &Project, command: &[OsString]) -> Result<ExitCode> {
    sweep_orphaned_isolated_containers(None);
    let deadline = Instant::now() + GUEST_READY_TIMEOUT + CONFLICT_RETRY_TIMEOUT;
    let reservation = loop {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "container `{}` repeatedly stopped during session handoff",
                project.id
            ));
        }
        ensure_shared_container(project, config)?;
        if !wait_for_guest_ready(project)? {
            continue;
        }
        let reservation = session_reservation_token(project);
        if reserve_shared_session(project, &reservation)? {
            break reservation;
        }
    };

    // The attached process owns the terminal, but not the shared container.
    let mut exec = exec_command(
        std::io::stdin().is_terminal(),
        project,
        &reservation,
        command,
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
fn resolve_config_mounts(config: &Config, project_root: &Path) -> Result<ConfigMounts> {
    Ok(ConfigMounts {
        git: git_mount_host(project_root, config.read_only_git),
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
pub fn stop_image(project: &Project) -> Result<ExitCode> {
    stop_shared_container(project)?;
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

/// Computes the full project digest stored in the runtime label.
fn project_digest(project: &Path) -> String {
    format!("{:x}", Sha256::digest(project.as_os_str().as_bytes()))
}

/// Builds the desired runtime identity from every creation-time input.
fn container_identity(
    project: &Project,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    lifecycle: &str,
    command: &[OsString],
) -> ContainerIdentity {
    let mut hasher = Sha256::new();
    hash_spec_field(
        &mut hasher,
        b"protocol",
        LIFECYCLE_PROTOCOL_VERSION.as_bytes(),
    );
    hash_spec_field(&mut hasher, b"image", IMAGE_TAG.as_bytes());
    hash_spec_field(&mut hasher, b"lifecycle", lifecycle.as_bytes());
    hash_spec_field(&mut hasher, b"uid", host_ids.uid.as_bytes());
    hash_spec_field(&mut hasher, b"gid", host_ids.gid.as_bytes());
    hash_spec_field(&mut hasher, b"project", project.root.as_os_str().as_bytes());
    hash_spec_field(
        &mut hasher,
        b"workdir",
        project.workdir.as_os_str().as_bytes(),
    );
    if let Some(git) = &config_mounts.git {
        hash_spec_field(&mut hasher, b"git", git.as_os_str().as_bytes());
    } else {
        hash_spec_field(&mut hasher, b"git", b"");
    }
    for mount in &config_mounts.shared {
        hash_spec_field(
            &mut hasher,
            b"mount-host",
            mount.host.as_os_str().as_bytes(),
        );
        hash_spec_field(
            &mut hasher,
            b"mount-dest",
            mount.dest.as_os_str().as_bytes(),
        );
        hash_spec_field(
            &mut hasher,
            b"mount-permission",
            match mount.permission {
                Permission::ReadOnly => b"ro",
                Permission::ReadWrite => b"rw",
            },
        );
    }
    for argument in command {
        hash_spec_field(&mut hasher, b"command", argument.as_os_str().as_bytes());
    }
    ContainerIdentity {
        project: project_digest(&project.root),
        spec: format!("{:x}", hasher.finalize()),
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
        (LABEL_LIFECYCLE, lifecycle),
        (LABEL_SPEC, identity.spec.as_str()),
    ] {
        run.arg("--label").arg(format!("{key}={value}"));
    }
}

/// Waits for the entrypoint to publish its explicit readiness marker. Runtime
/// `running` alone is insufficient because UID/GID and filesystem setup may
/// still be in progress.
fn wait_for_guest_ready(project: &Project) -> Result<bool> {
    let deadline = Instant::now() + GUEST_READY_TIMEOUT;
    loop {
        let output = guest_ready_command(project).output().map_err(spawn_error)?;
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
                    "container `{}` entered an unsupported state while initializing",
                    project.id
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "container `{}` did not publish guest readiness within {} seconds: {}",
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
    format!("{:x}", hasher.finalize())
}

/// Ensures the deterministic shared container exists, belongs to this
/// project, matches the requested creation specification, and is running.
fn ensure_shared_container(project: &Project, config: &Config) -> Result<()> {
    let ids = host_ids()?;
    let config_mounts = resolve_config_mounts(config, &project.root)?;
    warn_mount_conflicts(project, &config_mounts);
    let identity = container_identity(
        project,
        &ids,
        &config_mounts,
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );
    let deadline = Instant::now() + CONFLICT_RETRY_TIMEOUT;
    let mut checked_image = false;
    let mut last_conflict = None;

    loop {
        let inspection = inspect_container(&project.id)?;
        match inspection.state {
            ContainerState::Absent => {
                if !checked_image {
                    require_image()?;
                    checked_image = true;
                }
                match create_shared_container(project, &ids, &config_mounts, &identity) {
                    Ok(()) => {}
                    Err(err) => last_conflict = Some(err),
                }
            }
            ContainerState::Running => {
                validate_shared_container(&inspection, project, &identity)?;
                return Ok(());
            }
            ContainerState::Stopped => {
                validate_shared_container(&inspection, project, &identity)?;
                let output = Command::new(CONTAINER_BIN)
                    .args(["start", &project.id])
                    .output()
                    .map_err(spawn_error)?;
                if !output.status.success() {
                    last_conflict = Some(anyhow!(
                        "failed to start shared container '{}': {}",
                        project.id,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
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

/// Creates a detached shared container with a unique, automatically removed
/// cidfile beneath the current user's temporary directory.
fn create_shared_container(
    project: &Project,
    ids: &HostIds,
    config_mounts: &ConfigMounts,
    identity: &ContainerIdentity,
) -> Result<()> {
    let cid_dir = tempfile::Builder::new()
        .prefix("silo-cid-")
        .tempdir_in(std::env::temp_dir())
        .context("failed to create temporary cid directory")?;
    let cidfile = cid_dir.path().join("container.cid");
    let output = create_command(project, ids, config_mounts, &cidfile)?
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
    validate_shared_ownership(inspection, project)?;
    let actual = inspection.labels.get(LABEL_SPEC).map(String::as_str);
    if actual != Some(identity.spec.as_str()) {
        return Err(anyhow!(
            "container '{}' was created from a different Silo specification; run 'silo stop' and retry",
            project.id
        ));
    }
    Ok(())
}

/// Stops and deletes only the inspect-validated shared container for this
/// project. Conflicting runtime transitions are retried for a bounded time.
fn stop_shared_container(project: &Project) -> Result<()> {
    let deadline = Instant::now() + CONFLICT_RETRY_TIMEOUT;
    let mut last_conflict = None;
    loop {
        let inspection = inspect_container(&project.id)?;
        match inspection.state {
            ContainerState::Absent => return Ok(()),
            ContainerState::Running => {
                validate_shared_ownership(&inspection, project)?;
                let output = Command::new(CONTAINER_BIN)
                    .args(["stop", &project.id])
                    .output()
                    .map_err(spawn_error)?;
                if !output.status.success() {
                    last_conflict = Some(anyhow!(
                        "failed to stop shared container '{}': {}",
                        project.id,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
            }
            ContainerState::Stopping => {
                validate_shared_ownership(&inspection, project)?;
            }
            ContainerState::Stopped => {
                validate_shared_ownership(&inspection, project)?;
                let output = Command::new(CONTAINER_BIN)
                    .args(["delete", &project.id])
                    .output()
                    .map_err(spawn_error)?;
                if output.status.success()
                    || String::from_utf8_lossy(&output.stderr)
                        .to_lowercase()
                        .contains("not found")
                {
                    return Ok(());
                }
                last_conflict = Some(anyhow!(
                    "failed to delete shared container '{}': {}",
                    project.id,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            ContainerState::Unknown => {
                return Err(anyhow!(
                    "container '{}' is in an unsupported runtime state and was not deleted",
                    project.id
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err(last_conflict.unwrap_or_else(|| {
                anyhow!(
                    "container '{}' did not stop within {} seconds",
                    project.id,
                    CONFLICT_RETRY_TIMEOUT.as_secs()
                )
            }));
        }
        thread::sleep(CONFLICT_RETRY_INTERVAL);
    }
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

/// Parses the state and labels from current and older inspect JSON shapes.
fn parse_container_inspection(stdout: &[u8], id: &str) -> Result<ContainerInspection> {
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
    let state = match status {
        "running" => ContainerState::Running,
        "stopped" => ContainerState::Stopped,
        "stopping" => ContainerState::Stopping,
        _ => ContainerState::Unknown,
    };
    let labels = item
        .pointer("/configuration/labels")
        .or_else(|| item.get("labels"))
        .map(parse_inspect_labels)
        .transpose()?
        .unwrap_or_default();
    Ok(ContainerInspection { state, labels })
}

fn parse_inspect_labels(value: &Value) -> Result<HashMap<String, String>> {
    if let Some(object) = value.as_object() {
        return object
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_string()))
                    .ok_or_else(|| anyhow!("container inspect label `{key}` is not a string"))
            })
            .collect();
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(|item| {
                let key = item
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("container inspect label omitted its key"))?;
                let value = item
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("container inspect label `{key}` omitted its value"))?;
                Ok((key.to_string(), value.to_string()))
            })
            .collect();
    }
    Err(anyhow!(
        "container inspect labels are not an object or array"
    ))
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
                .or_else(|| item.pointer("/configuration/id").and_then(Value::as_str))
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
    [
        (LABEL_OWNER, LABEL_OWNER_VALUE),
        (LABEL_SCHEMA, LABEL_SCHEMA_VALUE),
        (LABEL_LIFECYCLE, LABEL_ISOLATED_VALUE),
    ]
    .into_iter()
    .all(|(key, value)| inspection.labels.get(key).map(String::as_str) == Some(value))
        && [LABEL_PROJECT, LABEL_SPEC].into_iter().all(|key| {
            inspection.labels.get(key).is_some_and(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
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
/// Returns an error when the project root has no name (e.g. `/`), or its path
/// or a mount's path cannot be expressed in a volume spec.
fn isolated_run_command(
    interactive: bool,
    project_root: &Path,
    host_ids: &HostIds,
    config_mounts: &ConfigMounts,
    id: &str,
    command: &[OsString],
) -> Result<Command> {
    let shared_dir = shared_dir_name(project_root)?;
    let project = Project {
        root: project_root.to_path_buf(),
        workdir: shared_dir.clone(),
        id: id.to_string(),
    };
    let identity = container_identity(
        &project,
        host_ids,
        config_mounts,
        LABEL_ISOLATED_VALUE,
        command,
    );
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("run").arg("--name").arg(id).arg("--rm").arg("-i");
    if interactive {
        // Allocating a pty without a terminal fails with ENOTTY.
        run.arg("-t");
    }
    append_identity_labels(&mut run, &identity, LABEL_ISOLATED_VALUE);
    append_creation_mounts(&mut run, project_root, &shared_dir, config_mounts)?;
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
    let identity = container_identity(
        project,
        host_ids,
        config_mounts,
        LABEL_SHARED_VALUE,
        &[OsString::from(SHARED_INIT_COMMAND)],
    );
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("run")
        .arg("--name")
        .arg(&project.id)
        .arg("--cidfile")
        .arg(cidfile)
        .arg("-d");
    append_identity_labels(&mut run, &identity, LABEL_SHARED_VALUE);
    append_creation_mounts(&mut run, &project.root, &project.workdir, config_mounts)?;
    append_host_ids(&mut run, host_ids);
    run.arg(IMAGE_TAG).arg(SHARED_INIT_COMMAND);
    Ok(run)
}

/// Adds mounts in their established override order and selects the workdir.
fn append_creation_mounts(
    run: &mut Command,
    project_root: &Path,
    shared_dir: &Path,
    config_mounts: &ConfigMounts,
) -> Result<()> {
    let host_dir = volume_host_path(project_root)?;
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
fn exec_command(
    interactive: bool,
    project: &Project,
    reservation: &str,
    command: &[OsString],
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
        .arg(&project.id);
    exec.arg(SESSION_WRAPPER_COMMAND).arg(reservation);
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
fn volume_host_path(project_root: &Path) -> Result<&str> {
    spec_host_path(project_root, "share")
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
