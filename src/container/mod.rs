use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use anyhow::{anyhow, Context, Result};

use crate::config::Config;

/// Name of the image this tool builds and runs.
pub const IMAGE_TAG: &str = "silo:latest";

/// Dockerfile embedded into the executable at compile time.
pub const DOCKERFILE: &str = include_str!("silo.dockerfile");

const CONTAINER_BIN: &str = "container";

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
///
/// # Errors
///
/// Returns an error when the configured Dockerfile path is empty, missing or
/// not a file, when the container CLI is missing, when the Dockerfile cannot
/// be written, or when the build itself fails.
pub fn build_image(config: &Config) -> Result<ExitCode> {
    if let Some(dockerfile) = &config.image.dockerfile {
        validate_dockerfile(dockerfile)?;
        return execute(&mut build_command(dockerfile, dockerfile_context(dockerfile)));
    }
    let build_dir = BuildDir::create()?;
    fs::write(build_dir.dockerfile(), DOCKERFILE).context("failed to write Dockerfile")?;
    execute(&mut build_command(&build_dir.dockerfile(), build_dir.path()))
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

/// Runs the container from the built image, replacing this process so that
/// signals sent to `silo` reach the container and its exit status is passed
/// through directly. Files and env vars from the run directory
/// (`~/.config/silo/run`) are injected at start.
///
/// # Errors
///
/// Returns an error when the image is not built yet, the host ids cannot be
/// determined, the run directory cannot be scanned, or the container CLI is
/// missing.
pub fn run_image() -> Result<ExitCode> {
    let (exists, stderr) = inspect_image()?;
    if !exists {
        return Err(inspect_error(&stderr));
    }
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let ids = host_ids()?;
    let run_files = run_files()?;
    Err(spawn_error(
        run_command(std::io::stdin().is_terminal(), &cwd, &ids, &run_files)?.exec(),
    ))
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

/// Runs `container image inspect`, returning whether the image exists and the
/// probe's stderr (which explains why the check failed, when it did).
fn inspect_image() -> Result<(bool, String)> {
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
/// uid remapping, and injects the run directory's files and env vars
/// ([`RunFiles`]).
///
/// # Errors
///
/// Returns an error when the current directory has no name (e.g. `/`), or
/// its path or a run mount's path cannot be expressed in a volume spec.
fn run_command(
    interactive: bool,
    cwd: &Path,
    host_ids: &HostIds,
    run_files: &RunFiles,
) -> Result<Command> {
    let shared_dir = shared_dir_name(cwd)?;
    let host_dir = volume_host_path(cwd)?;
    let mut command = Command::new(CONTAINER_BIN);
    command.arg("run").arg("--rm").arg("-i");
    if interactive {
        // Allocating a pty without a terminal fails with ENOTTY.
        command.arg("-t");
    }
    command
        .arg("-v")
        .arg(format!("{host_dir}:{}", shared_dir.display()))
        .arg("-w")
        .arg(shared_dir);
    for mount in &run_files.mounts {
        let host = mount_host_path(&mount.host)?;
        command
            .arg("-v")
            .arg(format!("{host}:{}:ro", mount.dest.display()));
    }
    if let Some(env_file) = &run_files.env_file {
        command.arg("--env-file").arg(env_file);
    }
    command
        .arg("--env")
        .arg(format!("SILO_UID={}", host_ids.uid))
        .arg("--env")
        .arg(format!("SILO_GID={}", host_ids.gid))
        .arg(IMAGE_TAG);
    Ok(command)
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
    path.to_str()
        .filter(|p| !p.contains(':'))
        .ok_or_else(|| {
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
