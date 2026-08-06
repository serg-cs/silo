use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use anyhow::{anyhow, Context, Result};

/// Name of the image this tool builds and runs.
pub const IMAGE_TAG: &str = "silo:latest";

/// Dockerfile embedded into the executable at compile time.
pub const DOCKERFILE: &str = include_str!("silo.dockerfile");

const CONTAINER_BIN: &str = "container";

/// Home directory of the container's default user (root on Alpine).
const CONTAINER_HOME: &str = "/root";

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

/// Builds the image from the embedded Dockerfile.
///
/// # Errors
///
/// Returns an error when the container CLI is missing, the Dockerfile cannot
/// be written, or the build itself fails.
pub fn build_image() -> Result<ExitCode> {
    let build_dir = BuildDir::create()?;
    fs::write(build_dir.dockerfile(), DOCKERFILE).context("failed to write Dockerfile")?;
    execute(&mut build_command(&build_dir.dockerfile(), build_dir.path()))
}

/// Runs the container from the built image, replacing this process so that
/// signals sent to `silo` reach the container and its exit status is passed
/// through directly.
///
/// # Errors
///
/// Returns an error when the image is not built yet or the container CLI is
/// missing.
pub fn run_image() -> Result<ExitCode> {
    let (exists, stderr) = inspect_image()?;
    if !exists {
        return Err(inspect_error(&stderr));
    }
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    Err(spawn_error(
        run_command(std::io::stdin().is_terminal(), &cwd)?.exec(),
    ))
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
    ExitCode::from(u8::try_from(code).expect("clamped to u8 range"))
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

/// Builds the `container run` command that shares the current directory with
/// the container, mounting it inside the container's home so the shell starts
/// there with all files reachable.
///
/// # Errors
///
/// Returns an error when the current directory has no name (e.g. `/`) or its
/// path cannot be expressed in a volume spec.
fn run_command(interactive: bool, cwd: &Path) -> Result<Command> {
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
        .arg(shared_dir)
        .arg(IMAGE_TAG);
    Ok(command)
}

/// Returns the host side of the volume spec, rejecting paths the container
/// CLI cannot parse: `:` separates host and container paths, and the spec is
/// built with `format!`, so the path must be valid UTF-8.
///
/// # Errors
///
/// Returns an error when the path is not valid UTF-8 or contains `:`.
fn volume_host_path(cwd: &Path) -> Result<&str> {
    cwd.to_str()
        .filter(|path| !path.contains(':'))
        .ok_or_else(|| {
            anyhow!(
                "cannot share `{}`: the path must be valid UTF-8 without `:`",
                cwd.display()
            )
        })
}

/// Returns where the shared directory lands in the container, i.e. the
/// current directory's last component placed inside the container home.
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
