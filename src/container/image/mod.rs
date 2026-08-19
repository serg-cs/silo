use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use anyhow::{Context, Result, anyhow};
use base64::prelude::*;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    CONTAINER_BIN, MountLock, RUNTIME_ASSETS, append_silo_launch_contract,
    ensure_owned_private_directory, exit_code, hash_spec_field, hex_digest, spawn_error,
    start_container_system, system_not_started,
};
use crate::config::Config;

mod dockerfile;

pub(super) use dockerfile::compose_derivative;

/// Stable derivative selected when no custom Dockerfile is configured.
pub(super) const DEFAULT_IMAGE_TAG: &str = "silo:latest";

/// Stable foundation inherited by every image Silo manages.
pub(super) const BASE_IMAGE_TAG: &str = "silo-base:latest";

/// Scratch tag protected by the user-global build lock.
pub(super) const STAGING_IMAGE_TAG: &str = "silo-build:staging";
pub(super) const CUSTOM_IMAGE_DIGEST_HEX_LEN: usize = 24;
pub(super) const MAX_COMPOSED_DOCKERFILE_BYTES: usize = 16 * 1024;

/// Runtime foundation embedded into the executable at compile time.
pub(super) const BASE_DOCKERFILE: &str = include_str!("../silo-base.dockerfile");

/// Default agent and developer-tool layer built on the runtime foundation.
pub(super) const EXTRAS_DOCKERFILE: &str = include_str!("../silo-extras.dockerfile");

pub(super) const BUILD_LOCK_PARENT: &str = "/tmp";
const IMAGE_SMOKE_COMMAND: &str = r#"set -eu
test "$(id -un)" = silo
test "${HOME:-}" = /home/silo
for helper in \
    /usr/local/bin/silo-supervisor \
    /usr/local/bin/silo-session \
    /usr/local/bin/silo-reserve \
    /usr/local/bin/silo-status \
    /usr/local/bin/silo-stop-guard; do
    test -x "$helper"
done
test "$(/usr/local/bin/silo-status)" = 0
for shell in /bin/bash /home/linuxbrew/.linuxbrew/bin/zsh /home/linuxbrew/.linuxbrew/bin/fish /home/linuxbrew/.linuxbrew/bin/nu; do
    test -x "$shell"
    "$shell" -c 'exit 0'
done"#;

/// Cross-process serialization for operations that replace Apple's
/// user-global image builder.
pub(super) struct BuildLock {
    _lock: MountLock,
}

impl BuildLock {
    fn acquire() -> Result<Self> {
        let root = global_build_lock_root();
        ensure_owned_private_directory(&root, "image build lock root")?;
        MountLock::acquire_at_for(&root, "image build").map(|lock| Self { _lock: lock })
    }

    #[cfg(test)]
    pub(super) fn acquire_at(state_root: &Path) -> Result<Self> {
        MountLock::acquire_at_for(&state_root.join("build"), "image build")
            .map(|lock| Self { _lock: lock })
    }
}

pub(super) fn global_build_lock_root() -> PathBuf {
    Path::new(BUILD_LOCK_PARENT).join(format!("silo-build-{}", unsafe { libc::geteuid() }))
}

/// Temporary build directory removed on every completion path.
pub(super) struct BuildDir(PathBuf);

impl BuildDir {
    fn create() -> Result<Self> {
        Self::create_at(build_dir())
    }

    pub(super) fn create_at(dir: PathBuf) -> Result<Self> {
        // Remove leftovers from an interrupted build before publishing assets.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create build dir `{}`", dir.display()))?;
        Ok(Self(dir))
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }

    fn base_dockerfile(&self) -> PathBuf {
        self.0.join("silo-base.dockerfile")
    }

    fn derivative_dockerfile(&self) -> PathBuf {
        self.0.join("silo-derivative.dockerfile")
    }

    fn derivative_dockerignore(&self) -> PathBuf {
        self.0.join("silo-derivative.dockerfile.dockerignore")
    }
}

impl Drop for BuildDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Removes the staging reference after publication or validation failure.
struct StagedImage {
    exists: bool,
}

impl StagedImage {
    const fn new() -> Self {
        Self { exists: false }
    }
}

impl Drop for StagedImage {
    fn drop(&mut self) {
        if self.exists
            && let Err(err) = execute_maintenance(
                image_delete_command(STAGING_IMAGE_TAG),
                "remove the temporary image tag",
            )
        {
            eprintln!("warning: image staging cleanup failed: {err:#}");
        }
    }
}

/// Rebuilds the runtime base and lets the derivative reuse only those layers.
pub(super) fn build(config: &Config) -> Result<ExitCode> {
    validate_config(config)?;
    let target = reference(config)?;
    let _build_lock = BuildLock::acquire()?;
    ensure_container_system_started()?;
    run_build_lifecycle(
        delete_builder,
        || build_configured_image(config, &target),
        cleanup_build_storage,
    )
}

/// Validates image configuration without accessing the container runtime.
pub(super) fn validate_config(config: &Config) -> Result<()> {
    if let Some(dockerfile) = &config.image.dockerfile {
        validate_dockerfile(dockerfile)?;
    }
    Ok(())
}

/// Resolves the stable local tag selected by the effective configuration.
pub(super) fn reference(config: &Config) -> Result<String> {
    config
        .image
        .dockerfile
        .as_deref()
        .map_or_else(|| Ok(DEFAULT_IMAGE_TAG.to_string()), custom_image_reference)
}

/// Gives each Dockerfile, context, and ignore-rule selection a stable tag.
pub(super) fn custom_image_reference(dockerfile: &Path) -> Result<String> {
    let canonical_dockerfile = fs::canonicalize(dockerfile).with_context(|| {
        format!(
            "could not resolve image dockerfile `{}`",
            dockerfile.display()
        )
    })?;
    let configured_name = dockerfile.file_name().ok_or_else(|| {
        anyhow!(
            "image dockerfile `{}` has no file name",
            dockerfile.display()
        )
    })?;
    let context = dockerfile_context(dockerfile);
    let canonical_context = fs::canonicalize(context).with_context(|| {
        format!(
            "could not resolve image build context `{}`",
            context.display()
        )
    })?;

    // The configured name selects `<name>.dockerignore` independently of a
    // symlinked Dockerfile's canonical contents.
    let mut hasher = Sha256::new();
    hash_spec_field(
        &mut hasher,
        b"dockerfile",
        canonical_dockerfile.as_os_str().as_bytes(),
    );
    hash_spec_field(
        &mut hasher,
        b"context",
        canonical_context.as_os_str().as_bytes(),
    );
    hash_spec_field(&mut hasher, b"configured-name", configured_name.as_bytes());
    let digest = hex_digest(hasher.finalize());
    Ok(format!(
        "silo:custom-{}",
        &digest[..CUSTOM_IMAGE_DIGEST_HEX_LEN]
    ))
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
pub(super) fn write_build_context(build_dir: &BuildDir) -> Result<()> {
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
    let mut staged = StagedImage::new();
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
    staged.exists = true;

    let check = super::execute(&mut image_runtime_check_command(STAGING_IMAGE_TAG))?;
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
pub(super) fn run_build_lifecycle(
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
    if system_not_started(&stderr) && !start_container_system() {
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

pub(super) fn cleanup_build_storage_with(
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

pub(super) fn builder_delete_command() -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["builder", "delete", "--force"]);
    command
}

pub(super) fn image_prune_command() -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "prune"]);
    command
}

pub(super) fn image_tag_command(source: &str, target: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "tag", source, target]);
    command
}

fn image_delete_command(image: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "delete", "--force", image]);
    command
}

pub(super) fn image_runtime_check_command(image: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["run", "--rm"]);
    append_silo_launch_contract(&mut command, false, false);
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

pub(super) fn validate_dockerfile(dockerfile: &Path) -> Result<()> {
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
    let content = fs::read_to_string(dockerfile)
        .with_context(|| format!("failed to read image dockerfile `{}`", dockerfile.display()))?;
    compose_derivative(BASE_DOCKERFILE, &content, dockerfile).map(|_| ())
}

/// Copies the ignore rules associated with a configured Dockerfile.
pub(super) fn copy_dockerignore(dockerfile: &Path, destination: &Path) -> Result<()> {
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

pub(super) fn dockerfile_context(dockerfile: &Path) -> &Path {
    dockerfile
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Requires the selected image and returns its immutable identity.
pub(super) fn require_digest(image: &str) -> Result<String> {
    let (digest, stderr) = inspect_image(image)?;
    digest.ok_or_else(|| inspect_error(image, &stderr))
}

/// Requires the selected image when only its presence matters.
pub(super) fn require(image: &str) -> Result<()> {
    require_digest(image).map(|_| ())
}

pub(super) fn inspect_error(image: &str, stderr: &str) -> anyhow::Error {
    if stderr.to_lowercase().contains("not found") {
        anyhow!("image `{image}` not built yet; run `silo image build` first")
    } else {
        anyhow!(
            "could not check for image `{image}`; `{CONTAINER_BIN} image inspect` reported:\n{stderr}"
        )
    }
}

fn inspect_image(image: &str) -> Result<(Option<String>, String)> {
    inspect_image_with(|| probe_image(image), start_container_system)
}

pub(super) fn inspect_image_with(
    probe: impl Fn() -> Result<(Option<String>, String)>,
    boot: impl Fn() -> bool,
) -> Result<(Option<String>, String)> {
    let (digest, stderr) = probe()?;
    if digest.is_none() && system_not_started(&stderr) && boot() {
        return probe();
    }
    Ok((digest, stderr))
}

fn probe_image(image: &str) -> Result<(Option<String>, String)> {
    let output = Command::new(CONTAINER_BIN)
        .args(["image", "inspect", image])
        .output()
        .map_err(spawn_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Ok((None, stderr));
    }
    Ok((Some(parse_image_digest(&output.stdout)?), stderr))
}

pub(super) fn parse_image_digest(output: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(output)
        .context("could not parse image inspection output as JSON")?;
    value
        .as_array()
        .and_then(|images| images.first())
        .and_then(|image| image.pointer("/configuration/descriptor/digest"))
        .and_then(Value::as_str)
        .filter(|digest| !digest.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("image inspection output did not contain an OCI image digest"))
}

fn execute_build(command: &mut Command) -> Result<ExitCode> {
    execute_build_with(
        command,
        || probe_image(DEFAULT_IMAGE_TAG),
        start_container_system,
    )
}

pub(super) fn execute_build_with(
    command: &mut Command,
    probe: impl Fn() -> Result<(Option<String>, String)>,
    boot: impl Fn() -> bool,
) -> Result<ExitCode> {
    // Boot before building when the probe reports a stopped system.
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
        command.stderr(Stdio::inherit());
        return super::execute(command);
    }
    Ok(exit_code(captured.status))
}

pub(super) struct CapturedOutput {
    pub(super) status: ExitStatus,
    pub(super) stderr: Vec<u8>,
}

/// Forwards build stderr live while retaining a bounded tail for retry logic.
pub(super) fn run_captured(command: &mut Command) -> Result<CapturedOutput> {
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error)?;
    let mut stderr = child.stderr.take().expect("stderr was piped");
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

pub(super) fn trim_captured(captured: &mut Vec<u8>) {
    const MAX_CAPTURED_STDERR: usize = 256 * 1024;
    if captured.len() > MAX_CAPTURED_STDERR {
        captured.drain(..captured.len() - MAX_CAPTURED_STDERR);
    }
}

fn build_dir() -> PathBuf {
    let pid = std::process::id();
    std::env::temp_dir().join(format!("silo-build-{pid}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BuildCache {
    Disabled,
    Reuse,
}

pub(super) fn build_command(
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
pub(super) fn runtime_asset_build_args() -> Vec<String> {
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
