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
    BASE_DOCKERFILE, BASE_IMAGE_TAG, DEFAULT_IMAGE_TAG, EXTRAS_DOCKERFILE, configured_dockerfile,
    dockerfile_context, probe_image, reference, validate_config,
};
use crate::apple::{
    CONTAINER_BIN, SystemStart, execute, exit_code, spawn_error, start_container_system,
    start_system_for_error,
};
use crate::config::Config;
use crate::image::runtime_contract::{RUNTIME_ASSETS, append_runtime_contract};
use crate::storage::{Lock, effective_uid, ensure_owned_private_directory};

const BUILD_LOCK_PARENT: &str = "/tmp";
/// Scratch tags protected by the user-global build lock.
const BASE_STAGING_IMAGE_TAG: &str = "silo-build:base-staging";
const STAGING_IMAGE_TAG: &str = "silo-build:staging";
const OBSOLETE_BASE_IMAGE_TAG: &str = "silo-build:obsolete-base";
const OBSOLETE_IMAGE_TAG: &str = "silo-build:obsolete";
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

/// Removes the staging reference after success or any ordinary failure.
struct StagedImage(&'static str);

impl Drop for StagedImage {
    fn drop(&mut self) {
        if let Err(err) = remove_staged_image(self.0) {
            eprintln!("warning: image staging cleanup failed: {err:#}");
        }
    }
}

/// Rebuilds the runtime base and lets the derivative reuse only those layers.
pub(crate) fn build(config: &Config, project_root: &Path) -> Result<ExitCode> {
    validate_config(config, project_root)?;
    let target = reference(config, project_root)?;
    let _build_lock = acquire_build_lock()?;
    ensure_container_system_started()?;
    run_build_lifecycle(
        delete_builder,
        || build_configured_image(config, project_root, &target),
        delete_builder,
    )
}

/// Rebuilds the selected tool layer on the published local base.
///
/// The tool Dockerfile is sent to the builder unchanged, with the local
/// `silo-base:latest` tag as its parent. The base tag is left in place.
pub(crate) fn update(config: &Config, project_root: &Path) -> Result<ExitCode> {
    validate_config(config, project_root)?;
    let target = reference(config, project_root)?;
    let _build_lock = acquire_build_lock()?;
    ensure_container_system_started()?;
    require_published_base()?;
    build_updated_image(config, project_root, &target)
}

fn build_updated_image(config: &Config, project_root: &Path, target: &str) -> Result<ExitCode> {
    let build_dir = BuildDir::create()?;
    let (dockerfile, context) = update_dockerfile(config, project_root, &build_dir)?;
    let _staged = StagedImage(STAGING_IMAGE_TAG);
    let status = build_and_validate_image(
        update_build_command(&dockerfile, &context),
        STAGING_IMAGE_TAG,
        target,
    )?;
    if status != ExitCode::SUCCESS {
        return Ok(status);
    }
    publish_updated_image(target)?;
    Ok(ExitCode::SUCCESS)
}

/// Selects the tool Dockerfile without inlining the runtime base.
fn update_dockerfile(
    config: &Config,
    project_root: &Path,
    build_dir: &BuildDir,
) -> Result<(PathBuf, PathBuf)> {
    if let Some(dockerfile) = configured_dockerfile(config, project_root)? {
        let context = dockerfile_context(&dockerfile).to_path_buf();
        return Ok((dockerfile, context));
    }
    let dockerfile = build_dir.derivative_dockerfile();
    fs::write(&dockerfile, EXTRAS_DOCKERFILE).context("failed to write extras Dockerfile")?;
    Ok((dockerfile, build_dir.path().to_path_buf()))
}

fn update_build_command(dockerfile: &Path, context: &Path) -> Command {
    let build_args: &[String] = &[];
    build_command(
        dockerfile,
        context,
        STAGING_IMAGE_TAG,
        false,
        BuildCache::Disabled,
        build_args,
    )
}

fn require_published_base() -> Result<()> {
    require_base_digest(current_image_digest(BASE_IMAGE_TAG))?;
    Ok(())
}

fn require_base_digest(digest: Result<Option<String>>) -> Result<String> {
    match digest? {
        Some(digest) => Ok(digest),
        None => Err(anyhow!(
            "image `{BASE_IMAGE_TAG}` not built yet; run `silo image build` first"
        )),
    }
}

/// Tags that move when an update replaces the selected derivative.
struct UpdatePublication {
    reclaim_tag: &'static str,
    publish: Command,
}

fn update_publication(target: &str) -> UpdatePublication {
    UpdatePublication {
        reclaim_tag: OBSOLETE_IMAGE_TAG,
        publish: image_tag_command(STAGING_IMAGE_TAG, target),
    }
}

/// Publishes the staged tool layer and leaves the runtime base tag in place.
///
/// A running container can keep the previous derivative alive, so reclaim
/// deletion must not undo a tag that has already moved.
fn publish_updated_image(target: &str) -> Result<()> {
    let publication = update_publication(target);
    let previous = retain_previous_image(target, publication.reclaim_tag)?;
    let published = execute_maintenance(publication.publish, &format!("publish image `{target}`"));
    let cleaned = remove_reclaimed_image(publication.reclaim_tag, previous.as_deref());
    finish_updated_publication(published, cleaned)
}

fn finish_updated_publication(published: Result<()>, cleaned: Result<()>) -> Result<()> {
    if let Err(err) = cleaned {
        eprintln!("warning: replaced image cleanup failed: {err:#}");
    }
    published
}

fn build_configured_image(config: &Config, project_root: &Path, target: &str) -> Result<ExitCode> {
    let build_dir = BuildDir::create()?;
    write_build_context(&build_dir)?;
    let resolved_dockerfile = configured_dockerfile(config, project_root)?;
    let (derivative, source, context) = match &resolved_dockerfile {
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
    if resolved_dockerfile.is_some() {
        copy_dockerignore(source, &build_dir.derivative_dockerignore())?;
    }
    let build_args = runtime_asset_build_args();

    // Keep both candidates alive until publication. The derivative uses the
    // cached internal base stage, so it does not need the stable base tag updated.
    let _base = StagedImage(BASE_STAGING_IMAGE_TAG);
    let _derivative = StagedImage(STAGING_IMAGE_TAG);
    run_image_publication(
        || {
            build_and_validate_image(
                build_command(
                    &build_dir.base_dockerfile(),
                    build_dir.path(),
                    BASE_STAGING_IMAGE_TAG,
                    true,
                    BuildCache::Disabled,
                    &build_args,
                ),
                BASE_STAGING_IMAGE_TAG,
                BASE_IMAGE_TAG,
            )
        },
        || {
            build_and_validate_image(
                build_command(
                    &build_dir.derivative_dockerfile(),
                    context,
                    STAGING_IMAGE_TAG,
                    false,
                    BuildCache::Reuse,
                    &build_args,
                ),
                STAGING_IMAGE_TAG,
                target,
            )
        },
        || {
            let previous_base = retain_previous_image(BASE_IMAGE_TAG, OBSOLETE_BASE_IMAGE_TAG)?;
            let previous_target = retain_previous_image(target, OBSOLETE_IMAGE_TAG)?;
            let published = execute_maintenance(
                image_tag_command(BASE_STAGING_IMAGE_TAG, BASE_IMAGE_TAG),
                &format!("publish image `{BASE_IMAGE_TAG}`"),
            )
            .and_then(|()| {
                execute_maintenance(
                    image_tag_command(STAGING_IMAGE_TAG, target),
                    &format!("publish image `{target}`"),
                )
                .with_context(|| {
                    format!(
                        "derivative publication failed; `{BASE_IMAGE_TAG}` has already been published"
                    )
                })
            });
            let cleaned_base =
                remove_reclaimed_image(OBSOLETE_BASE_IMAGE_TAG, previous_base.as_deref());
            let cleaned_target =
                remove_reclaimed_image(OBSOLETE_IMAGE_TAG, previous_target.as_deref());
            match published {
                Ok(()) => {
                    cleaned_base?;
                    cleaned_target
                }
                Err(err) => {
                    if let Err(clean) = cleaned_base {
                        eprintln!("warning: replaced image cleanup failed: {clean:#}");
                    }
                    if let Err(clean) = cleaned_target {
                        eprintln!("warning: replaced image cleanup failed: {clean:#}");
                    }
                    Err(err)
                }
            }
        },
    )
}

/// Publishes the embedded base input used by the standalone base build.
fn write_build_context(build_dir: &BuildDir) -> Result<()> {
    fs::write(build_dir.base_dockerfile(), BASE_DOCKERFILE)
        .context("failed to write base Dockerfile")
}

/// Builds and smoke-tests a candidate without changing its stable tag.
fn build_and_validate_image(
    mut command: Command,
    staging_tag: &str,
    target: &str,
) -> Result<ExitCode> {
    // A prior interrupted build may have left this Silo-owned scratch tag.
    remove_staged_image(staging_tag)?;
    let status = execute_build(&mut command)?;
    if status != ExitCode::SUCCESS {
        return Ok(status);
    }
    if execute(&mut image_runtime_check_command(staging_tag))? != ExitCode::SUCCESS {
        return Err(anyhow!(
            "built image for `{target}` failed Silo's startup check; keep the inherited Silo user, entrypoint, helpers, and supported shells available"
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// Neither stable tag is published until both candidates build and validate.
fn run_image_publication(
    base: impl FnOnce() -> Result<ExitCode>,
    derivative: impl FnOnce() -> Result<ExitCode>,
    publish: impl FnOnce() -> Result<()>,
) -> Result<ExitCode> {
    let status = base()?;
    if status != ExitCode::SUCCESS {
        return Ok(status);
    }
    let status = derivative()?;
    if status != ExitCode::SUCCESS {
        return Ok(status);
    }
    publish()?;
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
            "could not start the Apple container system before building an image"
        ));
    }
    Ok(())
}

fn delete_builder() -> Result<()> {
    execute_maintenance(builder_delete_command(), "delete the global image builder")
}

fn builder_delete_command() -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["builder", "delete", "--force"]);
    command
}

fn image_tag_command(source: &str, target: &str) -> Command {
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "tag", source, target]);
    command
}

fn current_image_digest(image: &str) -> Result<Option<String>> {
    let (digest, stderr) = probe_image(image)?;
    match digest {
        Some(digest) => Ok(Some(digest)),
        None if super::image_not_found(&stderr, image) => Ok(None),
        None => Err(anyhow!(
            "could not check for image `{image}`; `{CONTAINER_BIN} image inspect` reported:\n{stderr}"
        )),
    }
}

fn retain_previous_image(live_tag: &str, reclaim_tag: &str) -> Result<Option<String>> {
    let Some(digest) = current_image_digest(live_tag)? else {
        return Ok(None);
    };
    execute_maintenance(
        image_tag_command(live_tag, reclaim_tag),
        &format!("keep replaced image `{live_tag}` as `{reclaim_tag}`"),
    )?;
    Ok(Some(digest))
}

fn remove_reclaimed_image(reclaim_tag: &str, expected_digest: Option<&str>) -> Result<()> {
    let Some(expected_digest) = expected_digest else {
        return Ok(());
    };
    let output = Command::new(CONTAINER_BIN)
        .args(["image", "inspect", reclaim_tag])
        .output()
        .map_err(spawn_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if super::image_not_found(&stderr, reclaim_tag) {
            return Ok(());
        }
        return Err(anyhow!(
            "could not inspect replaced image `{reclaim_tag}` for cleanup: {}",
            stderr.trim()
        ));
    }
    if let Some(command) =
        superseded_image_delete_command(reclaim_tag, expected_digest, &output.stdout)?
    {
        execute_maintenance(command, "remove the replaced image")?;
    }
    Ok(())
}

fn superseded_image_delete_command(
    reclaim_tag: &str,
    expected_digest: &str,
    inspection: &[u8],
) -> Result<Option<Command>> {
    let inspection: serde_json::Value = serde_json::from_slice(inspection)
        .context("could not parse replaced image inspection; leaving it untouched")?;
    let resolved_name = inspection
        .pointer("/0/configuration/name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow!(
                "replaced image inspection did not contain configuration.name; leaving it untouched"
            )
        })?;
    let resolved_digest = inspection
        .pointer("/0/configuration/descriptor/digest")
        .and_then(serde_json::Value::as_str)
        .filter(|digest| !digest.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "replaced image inspection did not contain an OCI image digest; leaving it untouched"
            )
        })?;
    // `image tag` stores a registry-normalized name. Delete that stored
    // reference only when it is still the reclaim tag and the replaced digest.
    if !short_image_name_matches(resolved_name, reclaim_tag) || resolved_digest != expected_digest {
        return Ok(None);
    }
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "delete", "--force", resolved_name]);
    Ok(Some(command))
}

fn short_image_name_matches(resolved: &str, short: &str) -> bool {
    resolved == short || resolved.ends_with(&format!("/{short}"))
}

fn remove_staged_image(image: &str) -> Result<()> {
    let output = Command::new(CONTAINER_BIN)
        .args(["image", "inspect", image])
        .output()
        .map_err(spawn_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if super::image_not_found(&stderr, image) {
            return Ok(());
        }
        return Err(anyhow!(
            "could not inspect staging image `{image}` for cleanup: {}",
            stderr.trim()
        ));
    }
    if let Some(command) = image_delete_command(image, &output.stdout)? {
        execute_maintenance(command, "remove the temporary image tag")?;
    }
    Ok(())
}

fn image_delete_command(image: &str, inspection: &[u8]) -> Result<Option<Command>> {
    // Apple resolves absent staging names through retained build annotations,
    // possibly selecting a published image. Only delete the actual staging tag.
    let inspection: serde_json::Value = serde_json::from_slice(inspection)
        .context("could not parse staging image inspection; leaving it untouched")?;
    let resolved = inspection
        .pointer("/0/configuration/name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow!(
                "staging image inspection did not contain configuration.name; leaving it untouched"
            )
        })?;
    // `container build` keeps these short tags; it does not add a registry.
    if resolved != image {
        return Ok(None);
    }
    let mut command = Command::new(CONTAINER_BIN);
    command.args(["image", "delete", "--force", image]);
    Ok(Some(command))
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
