//! Image build orchestration, publication, and process handling.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use anyhow::{Context, Result, anyhow};
use base64::prelude::*;
use tempfile::TempDir;

use super::dockerfile::compose_derivative;
#[cfg(test)]
use super::dockerfile::validate_dockerfile;
use super::{
    BASE_DOCKERFILE, BASE_IMAGE_TAG, DEFAULT_IMAGE_TAG, EXTRAS_DOCKERFILE, dockerfile_context,
    probe_image, reference, validate_config,
};
use crate::apple::{
    CONTAINER_BIN, SystemStart, execute, exit_code, spawn_error, start_container_system,
    start_system_for_error,
};
use crate::config::Config;
use crate::image::runtime_contract::{RUNTIME_ASSETS, append_runtime_contract};
use crate::storage::{Lock, effective_uid, ensure_owned_private_directory};

const BUILD_LOCK_PARENT: &str = "/tmp";
/// Scratch tag protected by the user-global build lock.
const STAGING_IMAGE_TAG: &str = "silo-build:staging";
const IMAGE_SMOKE_COMMAND: &str = r#"set -eu
test "$(id -un)" = silo
test "${HOME:-}" = /home/silo
for helper in \
    /usr/local/bin/silo-lifecycle; do
    test -x "$helper"
done
for shell in /bin/bash /home/linuxbrew/.linuxbrew/bin/zsh /home/linuxbrew/.linuxbrew/bin/fish /home/linuxbrew/.linuxbrew/bin/nu; do
    test -x "$shell"
    "$shell" -c 'exit 0'
done"#;

/// Serializes operations that replace Apple's user-global image builder.
fn acquire_build_lock() -> Result<Lock> {
    let root = global_build_lock_root();
    ensure_owned_private_directory(&root, "image build lock root")?;
    Lock::acquire(&root.join(".lock"), "image build")
}

#[cfg(test)]
fn acquire_build_lock_at(state_root: &Path) -> Result<Lock> {
    Lock::acquire_in(&state_root.join("build"), "image build")
}

fn global_build_lock_root() -> PathBuf {
    Path::new(BUILD_LOCK_PARENT).join(format!("silo-build-{}", effective_uid()))
}

/// Temporary build directory removed on every completion path.
struct BuildDir {
    temporary: TempDir,
}

impl BuildDir {
    fn create() -> Result<Self> {
        let root = global_build_lock_root();
        Self::create_in(&root)
    }

    fn create_in(root: &Path) -> Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix("context-")
            .tempdir_in(root)
            .with_context(|| {
                format!(
                    "failed to create image build context in `{}`",
                    root.display()
                )
            })?;
        Ok(Self { temporary })
    }

    #[cfg(test)]
    fn create_for_test(root: &Path) -> Result<Self> {
        Self::create_in(root)
    }

    fn path(&self) -> &Path {
        self.temporary.path()
    }

    fn base_dockerfile(&self) -> PathBuf {
        self.path().join("silo-base.dockerfile")
    }

    fn derivative_dockerfile(&self) -> PathBuf {
        self.path().join("silo-derivative.dockerfile")
    }

    fn derivative_dockerignore(&self) -> PathBuf {
        self.path().join("silo-derivative.dockerfile.dockerignore")
    }
}

/// Removes the staging reference after publication or validation failure.
struct StagedImage;

impl Drop for StagedImage {
    fn drop(&mut self) {
        if let Err(err) = execute_maintenance(
            image_delete_command(STAGING_IMAGE_TAG),
            "remove the temporary image tag",
        ) {
            eprintln!("warning: image staging cleanup failed: {err:#}");
        }
    }
}

/// Rebuilds the runtime base and lets the derivative reuse only those layers.
pub(crate) fn build(config: &Config) -> Result<ExitCode> {
    validate_config(config)?;
    let target = reference(config)?;
    let _build_lock = acquire_build_lock()?;
    ensure_container_system_started()?;
    run_build_lifecycle(
        delete_builder,
        || build_configured_image(config, &target),
        cleanup_build_storage,
    )
}
fn build_configured_image(config: &Config, target: &str) -> Result<ExitCode> {
    let build_dir = BuildDir::create()?;
    write_build_context(&build_dir)?;
    let (derivative, source, context) = match &config.image.dockerfile {
        Some(dockerfile) => (
            fs::read_to_string(dockerfile).with_context(|| {
                format!("failed to read image dockerfile `{}`", dockerfile.display())
            })?,
            dockerfile.as_path(),
            dockerfile_context(dockerfile),
        ),
        None => (
            EXTRAS_DOCKERFILE.to_string(),
            Path::new("embedded silo-extras.dockerfile"),
            build_dir.path(),
        ),
    };
    let combined = compose_derivative(BASE_DOCKERFILE, &derivative, source)?;
    fs::write(build_dir.derivative_dockerfile(), combined)
        .context("failed to write derivative Dockerfile")?;
    if config.image.dockerfile.is_some() {
        copy_dockerignore(source, &build_dir.derivative_dockerignore())?;
    }
    let build_args = runtime_asset_build_args();

    // Publish the base as a stable user-visible output. The derivative reuses
    // this invocation's cached base stage without resolving the published tag.
    let base = build_and_publish_image(
        &build_dir.base_dockerfile(),
        build_dir.path(),
        BASE_IMAGE_TAG,
        true,
        BuildCache::Disabled,
        &build_args,
    )?;
    if base != ExitCode::SUCCESS {
        return Ok(base);
    }

    build_and_publish_image(
        &build_dir.derivative_dockerfile(),
        context,
        target,
        false,
        BuildCache::Reuse,
        &build_args,
    )
}

/// Publishes the embedded base input used by the standalone base build.
fn write_build_context(build_dir: &BuildDir) -> Result<()> {
    fs::write(build_dir.base_dockerfile(), BASE_DOCKERFILE)
        .context("failed to write base Dockerfile")
}

/// Builds and smoke-tests a temporary image before replacing its stable tag.
fn build_and_publish_image(
    dockerfile: &Path,
    context: &Path,
    target: &str,
    pull: bool,
    cache: BuildCache,
    build_args: &[String],
) -> Result<ExitCode> {
    // A prior interrupted build may have left this Silo-owned scratch tag.
    let _ = image_delete_command(STAGING_IMAGE_TAG).status();
    let status = execute_build(&mut build_command(
        dockerfile,
        context,
        STAGING_IMAGE_TAG,
        pull,
        cache,
        build_args,
    ))?;
    if status != ExitCode::SUCCESS {
        return Ok(status);
    }
    let staged = StagedImage;

    let check = execute(&mut image_runtime_check_command(STAGING_IMAGE_TAG))?;
    if check != ExitCode::SUCCESS {
        return Err(anyhow!(
            "built image for `{target}` failed Silo's startup check; keep the inherited Silo user, entrypoint, helpers, and supported shells available"
        ));
    }
    execute_maintenance(
        image_tag_command(STAGING_IMAGE_TAG, target),
        &format!("publish image `{target}`"),
    )?;
    drop(staged);
    Ok(ExitCode::SUCCESS)
}

/// Preserves build failures while always attempting storage cleanup.
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
    let (_, stderr) = probe_image(DEFAULT_IMAGE_TAG)?;
    if start_system_for_error(&stderr, start_container_system) == SystemStart::Failed {
        return Err(anyhow!(
            "could not start the Apple container system before cleaning build storage"
        ));
    }
    Ok(())
}

fn delete_builder() -> Result<()> {
    execute_maintenance(builder_delete_command(), "delete the global image builder")
}

fn prune_images() -> Result<()> {
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

fn image_tag_command(source: &str, target: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "tag", source, target]);
    command
}

fn image_delete_command(image: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "delete", "--force", image]);
    command
}

fn image_runtime_check_command(image: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["run", "--rm"]);
    append_runtime_contract(&mut command, false, false);
    command
        .arg(image)
        .args(["/bin/sh", "-c", IMAGE_SMOKE_COMMAND]);
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

/// Copies the ignore rules associated with a configured Dockerfile.
fn copy_dockerignore(dockerfile: &Path, destination: &Path) -> Result<()> {
    let mut name = dockerfile
        .file_name()
        .map_or_else(|| OsString::from("Dockerfile"), OsString::from);
    name.push(".dockerignore");
    let source = dockerfile.with_file_name(name);
    if !source.exists() {
        return Ok(());
    }
    if !source.is_file() {
        return Err(anyhow!(
            "image Dockerfile ignore path `{}` is not a file",
            source.display()
        ));
    }
    fs::copy(&source, destination).with_context(|| {
        format!(
            "failed to copy image Dockerfile ignore file `{}`",
            source.display()
        )
    })?;
    Ok(())
}
fn execute_build(command: &mut Command) -> Result<ExitCode> {
    execute_build_with(
        command,
        || probe_image(DEFAULT_IMAGE_TAG),
        start_container_system,
    )
}

fn execute_build_with(
    command: &mut Command,
    probe: impl Fn() -> Result<(Option<String>, String)>,
    boot: impl Fn() -> bool,
) -> Result<ExitCode> {
    // Boot before building when the probe reports a stopped system.
    if let Ok((_, stderr)) = probe() {
        let _ = start_system_for_error(&stderr, &boot);
    }
    let captured = run_captured(command)?;
    if !captured.status.success()
        && start_system_for_error(&String::from_utf8_lossy(&captured.stderr), boot)
            == SystemStart::Started
    {
        command.stderr(Stdio::inherit());
        return execute(command);
    }
    Ok(exit_code(captured.status))
}

struct CapturedOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

/// Forwards build stderr live while retaining a bounded tail for retry logic.
fn run_captured(command: &mut Command) -> Result<CapturedOutput> {
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error)?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture builder stderr"))?;
    let mut captured = Vec::new();
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
            Err(err) => {
                // Close the unread pipe before stopping the child so a
                // producer cannot remain blocked while this process waits.
                drop(stderr);
                let _ = child.kill();
                let _ = child.wait();
                return Err(err).context("failed to read image build stderr");
            }
        }
    }
    let status = child.wait().map_err(spawn_error)?;
    Ok(CapturedOutput {
        status,
        stderr: captured,
    })
}

fn trim_captured(captured: &mut Vec<u8>) {
    const MAX_CAPTURED_STDERR: usize = 256 * 1024;
    if captured.len() > MAX_CAPTURED_STDERR {
        captured.drain(..captured.len() - MAX_CAPTURED_STDERR);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildCache {
    Disabled,
    Reuse,
}

fn build_command(
    dockerfile: &Path,
    context: &Path,
    image: &str,
    pull: bool,
    cache: BuildCache,
    build_args: &[String],
) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command
        .arg("build")
        .arg("--file")
        .arg(dockerfile)
        .arg("--tag")
        .arg(image);
    if pull {
        command.arg("--pull");
    }
    if cache == BuildCache::Disabled {
        command.arg("--no-cache");
    }
    for build_arg in build_args {
        command.arg("--build-arg").arg(build_arg);
    }
    command.arg(context);
    command
}

/// Encodes runtime files so the base stage is independent of custom contexts.
fn runtime_asset_build_args() -> Vec<String> {
    RUNTIME_ASSETS
        .iter()
        .map(|asset| {
            format!(
                "{}={}",
                asset.build_arg,
                BASE64_STANDARD.encode(asset.contents.as_bytes())
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
