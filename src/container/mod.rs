use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::config::{Config, Permission, Shared};

/// Name of the image this tool builds and runs.
pub const IMAGE_TAG: &str = "silo:latest";

/// Dockerfile embedded into the executable at compile time.
pub const DOCKERFILE: &str = include_str!("silo.dockerfile");

const CONTAINER_BIN: &str = "container";

/// Prefix of the `--name` silo passes to `container run`; the container CLI
/// uses the name as the container ID, so silo knows its container's ID
/// before the run starts and leftovers are recognizable.
const CONTAINER_NAME_PREFIX: &str = "silo-";

/// Name of the directory under the config directory holding one marker file
/// per container silo has started but not yet confirmed removed. The file
/// name is the container ID, which embeds the pid of the silo that started
/// it; a marker whose silo is dead means the run was killed and the
/// container is swept by the next run.
const CONTAINER_STATE_DIR: &str = "containers";

/// Home directory of the container's `silo` user; the shared project
/// directory is mounted into it as `<home>/<project-name>`.
const CONTAINER_HOME: &str = "/home/silo";

/// Name of the `env` file in the run directory whose `KEY=VALUE` lines are
/// passed to the container via `--env-file`. It is never mounted.
const RUN_ENV_FILE: &str = "env";

/// Commented `env` template written into a fresh run directory; every line
/// starts with `#`, so the container CLI's `--env-file` parser skips them.
const RUN_ENV_TEMPLATE: &str = "\
# Environment variables injected into every container at start.
# Write one KEY=VALUE per line; blank lines and # comments are ignored.
# A bare KEY without `=` inherits the host's value of that variable.
#   OPENAI_API_KEY=sk-...
#
# Every other entry in this directory is bind-mounted read-only into the
# container at the matching path under /home/silo/, so the layout mirrors the
# container's home directory:
#   .agents/foo.json     -> /home/silo/.agents/foo.json
#   .config/opencode/    -> /home/silo/.config/opencode/
#   .gitconfig           -> /home/silo/.gitconfig
# Create a new file, directory, or symlink and it appears in every container
# at the next `silo run`. Symlinks are resolved to their target, so a link to
# a directory like ~/.agents shares its live contents. Links nested inside a
# mounted directory are served as links and will not resolve; keep them at
# the top level of this directory instead.
";

/// Files and environment variables injected into the container at start,
/// discovered in the run directory (`~/.config/silo/run`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RunFiles {
    /// The `env` file, passed to the container via `--env-file`.
    env_file: Option<PathBuf>,
    /// Top-level entries of the run directory, mounted read-only into the
    /// container home.
    mounts: Vec<RunMount>,
}

/// One run directory entry: the host path to mount and where it lands in the
/// container.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunMount {
    host: PathBuf,
    dest: PathBuf,
}

/// One configured shared mount resolved for this run: `host` is the
/// canonical source path on this machine, `dest` where it is mounted inside
/// the container, and `permission` how it may be used.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedShared {
    host: PathBuf,
    dest: PathBuf,
    permission: Permission,
}

/// Config-driven mounts applied on top of the shared project directory and
/// the run directory files: the optional read-only `.git` and the
/// configured shared mounts.
#[derive(Default)]
struct ConfigMounts {
    git: Option<PathBuf>,
    shared: Vec<ResolvedShared>,
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

impl RunFiles {
    /// Scans the run directory for what to inject at container start: the
    /// `env` file becomes the `--env-file`, and every other top-level entry
    /// (files, directories, dotfiles, symlinks) is mounted read-only at the
    /// matching path under the container home. Symlinks are resolved to
    /// their target, so the mounted content is the target's live content.
    ///
    /// A missing directory yields an empty [`RunFiles`]; the caller creates
    /// it on first use.
    ///
    /// # Errors
    ///
    /// Returns an error when the run directory exists but cannot be read, or
    /// an entry cannot be mounted (name not usable in a volume spec, `env`
    /// not a file, broken symlink).
    fn discover(run_dir: &Path) -> Result<Self> {
        let mut env_file = None;
        let mut mounts = Vec::new();
        if run_dir.is_dir() {
            for entry in read_dir_entries(run_dir)? {
                let name = entry.file_name();
                if name.to_str() == Some(RUN_ENV_FILE) {
                    env_file = Some(env_file_path(run_dir)?);
                } else {
                    mounts.push(mount_for(run_dir, &name)?);
                }
            }
        } else if run_dir.exists() {
            return Err(anyhow!(
                "run directory `{}` is not a directory",
                run_dir.display()
            ));
        }
        // Symlink resolution can reorder entries; sort for deterministic
        // command lines.
        mounts.sort_by(|a, b| a.host.cmp(&b.host));
        Ok(Self { env_file, mounts })
    }
}

/// Returns the entries of the run directory sorted by name, so the mount
/// order is deterministic.
///
/// # Errors
///
/// Returns an error when the directory cannot be read.
fn read_dir_entries(run_dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(run_dir)
        .with_context(|| format!("failed to read run directory `{}`", run_dir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("failed to read run directory `{}`", run_dir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

/// Validates the `env` entry is a file (following symlinks) and returns its
/// path.
///
/// # Errors
///
/// Returns an error when the entry is not a file, e.g. a directory.
fn env_file_path(run_dir: &Path) -> Result<PathBuf> {
    let path = run_dir.join(RUN_ENV_FILE);
    if !path.is_file() {
        return Err(anyhow!(
            "`{RUN_ENV_FILE}` in run directory `{}` is not a file",
            run_dir.display()
        ));
    }
    Ok(path)
}

/// Builds the mount for one run directory entry: the host side is the entry
/// itself with symlinks resolved, the container side is the entry's name
/// placed in the container home.
///
/// # Errors
///
/// Returns an error when the name is not usable in a volume spec or the
/// entry's symlink target cannot be resolved.
fn mount_for(run_dir: &Path, name: &OsStr) -> Result<RunMount> {
    let name = name
        .to_str()
        .filter(|name| !name.contains(':'))
        .ok_or_else(|| {
            anyhow!(
                "cannot mount `{}` in run directory `{}`: the name must be valid UTF-8 without `:`",
                Path::new(name).display(),
                run_dir.display()
            )
        })?;
    let host = fs::canonicalize(run_dir.join(name)).with_context(|| {
        format!(
            "cannot resolve `{}` in run directory `{}` (broken symlink?)",
            name,
            run_dir.display()
        )
    })?;
    Ok(RunMount {
        host,
        dest: Path::new(CONTAINER_HOME).join(name),
    })
}

/// Resolves the configured shared mounts into mounts for this run: expands
/// a leading `~` in each source to the home directory and canonicalizes it
/// (symlinks mount their target, like the run directory), keeping the
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
/// defeats: a run directory entry at the same or a deeper target replaces
/// part or all of the shared mount's content (the run directory is mounted
/// after the shared mounts and is always read-only), or a read-write shared
/// mount at or under the `.git` target lets tools in the container modify
/// version control state despite the read-only `.git` mount. An ancestor
/// mount does not hide a child mount, so only these directions conflict.
fn mount_conflicts(
    project_dest: &Path,
    git_mounted: bool,
    shared: &[ResolvedShared],
    run_files: &RunFiles,
) -> Vec<String> {
    let git_dest = git_mounted.then(|| project_dest.join(".git"));
    let mut warnings = Vec::new();
    for entry in shared {
        for mount in &run_files.mounts {
            if mount.dest.starts_with(&entry.dest) {
                warnings.push(format!(
                    "the run directory entry at `{}` replaces the shared mount of `{}` at `{}` or part of it: the run directory is mounted after the shared mounts and is always read-only",
                    mount.dest.display(),
                    entry.host.display(),
                    entry.dest.display()
                ));
            }
        }
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

/// Runs the container from the built image, executing `command` inside it
/// (empty runs the default shell). When the container system has not been
/// started yet, boots it first with `container system start`. `container
/// run --rm` is spawned
/// as a child instead of replacing this process, so silo outlives it and
/// always cleans up afterwards: signals (SIGINT, SIGTERM, SIGHUP, SIGQUIT)
/// are forwarded to the child so the container shuts down, and once the
/// child exits for any reason the container is force-deleted (killed if
/// still running), the terminal is restored, and the run's exit status is
/// passed through. If silo itself is killed hard (e.g. `kill -9`), the
/// container ID stays marked in the state directory and the next `silo run`
/// sweeps it. Files and env vars from the run directory
/// (`~/.config/silo/run`) are injected at start, the configured `[[shared]]`
/// mounts are resolved and applied, and the project's `.git` directory is
/// mounted read-only on top of the read-write project mount when the config
/// enables it.
///
/// # Errors
///
/// Returns an error when the image is not built yet, the host ids cannot be
/// determined, the run directory cannot be scanned, a signal handler cannot
/// be installed, or the container CLI is missing.
pub fn run_image(config: &Config, command: &[OsString]) -> Result<ExitCode> {
    let (exists, stderr) = inspect_image()?;
    if !exists {
        return Err(inspect_error(&stderr));
    }
    install_signal_handlers()?;
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let ids = host_ids()?;
    let run_files = run_files()?;
    let shared = resolve_shared(
        &config.shared,
        std::env::var_os("HOME").as_deref().map(Path::new),
    )?;
    let id = container_id();
    // Build the command first: it only fails on path validation, before any
    // container exists or a marker is written.
    let git_mount = git_mount_host(&cwd, config.read_only_git);
    let config_mounts = ConfigMounts {
        git: git_mount,
        shared,
    };
    let mut run = run_command(
        std::io::stdin().is_terminal(),
        &cwd,
        &ids,
        &run_files,
        &config_mounts,
        &id,
        command,
    )?;
    // Warn about shared mounts whose intent a later mount defeats, e.g. a
    // run directory entry shadowing a read-write shared mount.
    let shared_dir = shared_dir_name(&cwd)?;
    for warning in mount_conflicts(
        &shared_dir,
        config_mounts.git.is_some(),
        &config_mounts.shared,
        &run_files,
    ) {
        eprintln!("warning: {warning}");
    }
    sweep_stale_containers();
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

/// Returns the ID of this run's container, `<prefix><pid>`. The container
/// CLI uses the `--name` value as the container ID, so silo knows it before
/// the run starts and can remove the container afterwards without parsing
/// any output.
fn container_id() -> String {
    format!("{CONTAINER_NAME_PREFIX}{}", std::process::id())
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

/// Force-deletes containers left behind by silo runs that died before
/// cleaning up (killed, crashed): every marked container whose silo process
/// is no longer alive. Containers of concurrent, still-running silo
/// sessions are left alone (their marker's pid is alive). Markers are
/// removed once their container is gone.
fn sweep_stale_containers() {
    let Some(dir) = container_state_dir() else {
        return;
    };
    sweep_stale_in(&dir, delete_container);
}

/// Pure version of [`sweep_stale_containers`], taking the state directory
/// and the delete operation so the sweep logic is testable without a
/// container CLI.
fn sweep_stale_in(dir: &Path, mut delete: impl FnMut(&str) -> Result<()>) {
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

/// Discovers the run directory contents to inject into the container,
/// creating the directory with a commented `env` template on first use. With
/// no home directory the feature is skipped silently.
///
/// # Errors
///
/// Returns an error when the run directory cannot be scanned.
fn run_files() -> Result<RunFiles> {
    let Some(run_dir) = crate::config::run_dir() else {
        return Ok(RunFiles::default());
    };
    ensure_run_dir(&run_dir);
    RunFiles::discover(&run_dir)
}

/// Creates the run directory and its commented `env` template on first use,
/// warning instead of failing so an unwritable home directory never breaks a
/// command.
fn ensure_run_dir(run_dir: &Path) {
    if run_dir.exists() {
        return;
    }
    if let Err(err) = fs::create_dir_all(run_dir)
        .and_then(|()| fs::write(run_dir.join(RUN_ENV_FILE), RUN_ENV_TEMPLATE))
    {
        eprintln!(
            "warning: could not create run directory at `{}`: {err:#}",
            run_dir.display()
        );
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

/// Builds the `container run` command that shares the current directory with
/// the container, mounting it inside the `silo` user's home so the shell
/// starts there with all files reachable, forwards the host user's ids for
/// uid remapping, injects the run directory's files and env vars
/// ([`RunFiles`]), applies the config-driven mounts ([`ConfigMounts`]: the
/// project's `.git` read-only and the configured shared mounts, read-only
/// or read-write), names the container [`container_id`] so its ID is known up
/// front for cleanup, and runs `command` inside it (empty runs the image's
/// default command, the shell).
///
/// # Errors
///
/// Returns an error when the current directory has no name (e.g. `/`), or
/// its path or a mount's path cannot be expressed in a volume spec.
fn run_command(
    interactive: bool,
    cwd: &Path,
    host_ids: &HostIds,
    run_files: &RunFiles,
    config_mounts: &ConfigMounts,
    id: &str,
    command: &[OsString],
) -> Result<Command> {
    let shared_dir = shared_dir_name(cwd)?;
    let host_dir = volume_host_path(cwd)?;
    let mut run = Command::new(CONTAINER_BIN);
    run.arg("run").arg("--name").arg(id).arg("--rm").arg("-i");
    if interactive {
        // Allocating a pty without a terminal fails with ENOTTY.
        run.arg("-t");
    }
    run.arg("-v")
        .arg(format!("{host_dir}:{}", shared_dir.display()))
        .arg("-w")
        .arg(&shared_dir);
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
    for mount in &run_files.mounts {
        let host = mount_host_path(&mount.host)?;
        run.arg("-v")
            .arg(format!("{host}:{}:ro", mount.dest.display()));
    }
    if let Some(env_file) = &run_files.env_file {
        run.arg("--env-file").arg(env_file);
    }
    run.arg("--env")
        .arg(format!("SILO_UID={}", host_ids.uid))
        .arg("--env")
        .arg(format!("SILO_GID={}", host_ids.gid))
        .arg(IMAGE_TAG)
        .args(command);
    Ok(run)
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

/// Like [`volume_host_path`], for the run directory mounts.
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
