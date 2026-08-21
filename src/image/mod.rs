use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::apple::{CONTAINER_BIN, probe_with_system_start, spawn_error, start_container_system};
use crate::config::Config;
use crate::digest::hex as hex_digest;

mod build;
mod dockerfile;
pub(crate) mod runtime_contract;
#[cfg(test)]
mod tests;

pub(crate) use build::build;
#[cfg(test)]
use dockerfile::{compose_derivative, validate_dockerfile};

/// Stable derivative selected when no custom Dockerfile is configured.
const DEFAULT_IMAGE_TAG: &str = "silo:latest";

/// Stable foundation inherited by every image Silo manages.
const BASE_IMAGE_TAG: &str = "silo-base:latest";

const CUSTOM_IMAGE_DIGEST_HEX_LEN: usize = 24;
const MAX_COMPOSED_DOCKERFILE_BYTES: usize = 16 * 1024;

/// Runtime foundation embedded into the executable at compile time.
const BASE_DOCKERFILE: &str = include_str!("assets/silo-base.dockerfile");

/// Default agent and developer-tool layer built on the runtime foundation.
const EXTRAS_DOCKERFILE: &str = include_str!("assets/silo-extras.dockerfile");

pub(crate) fn validate_config(config: &Config) -> Result<()> {
    if let Some(dockerfile) = &config.image.dockerfile {
        dockerfile::validate_dockerfile(dockerfile)?;
    }
    Ok(())
}

/// Resolves the stable local tag selected by the effective configuration.
pub(crate) fn reference(config: &Config) -> Result<String> {
    config
        .image
        .dockerfile
        .as_deref()
        .map_or_else(|| Ok(DEFAULT_IMAGE_TAG.to_string()), custom_image_reference)
}

/// Gives each Dockerfile, context, and ignore-rule selection a stable tag.
fn custom_image_reference(dockerfile: &Path) -> Result<String> {
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
    hasher.update(canonical_dockerfile.as_os_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_context.as_os_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(configured_name.as_bytes());
    let digest = hex_digest(hasher.finalize());
    Ok(format!(
        "silo:custom-{}",
        &digest[..CUSTOM_IMAGE_DIGEST_HEX_LEN]
    ))
}
fn dockerfile_context(dockerfile: &Path) -> &Path {
    dockerfile
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Requires the selected image and returns its immutable identity.
pub(crate) fn require_digest(image: &str) -> Result<String> {
    let (digest, stderr) = inspect_image(image)?;
    digest.ok_or_else(|| inspect_error(image, &stderr))
}

fn inspect_error(image: &str, stderr: &str) -> anyhow::Error {
    if image_not_found(stderr, image) {
        anyhow!("image `{image}` not built yet; run `silo image build` first")
    } else {
        anyhow!(
            "could not check for image `{image}`; `{CONTAINER_BIN} image inspect` reported:\n{stderr}"
        )
    }
}

/// Matches only the runtime's explicit missing-image diagnostic.
fn image_not_found(stderr: &str, image: &str) -> bool {
    let message = stderr
        .trim()
        .strip_prefix("Error: ")
        .unwrap_or_else(|| stderr.trim());
    message == format!("image not found: {image}")
}

fn inspect_image(image: &str) -> Result<(Option<String>, String)> {
    probe_with_system_start(|| probe_image(image), start_container_system)
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

fn parse_image_digest(output: &[u8]) -> Result<String> {
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
