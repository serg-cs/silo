//! Configuration schema and source loading.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use config::{Config as ConfigLoader, File};
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_CONFIG: &str = include_str!("default.toml");

/// User configuration, loaded once at startup and passed to commands.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) image: Image,
    /// Creation settings for containers managed by Silo.
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) container: Container,
    /// Host loopback TCP ports exposed on the same ports in shared containers.
    /// Project arrays replace global arrays.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) host_ports: BTreeSet<u16>,
    /// Interactive shell supplied by the Silo base image. When omitted, Silo
    /// mirrors a supported host `$SHELL` and falls back to Zsh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) shell: Option<Shell>,
    /// Settings for the project workspace mounted into the container.
    pub(crate) workspace: Workspace,
    /// Existing host directories exposed inside the container.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) binds: BTreeMap<String, Bind>,
    /// Writable storage managed by Silo.
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) state: State,
    /// Quick commands: `silo <name>` runs this command inside the container
    /// without typing `silo run --` every time. The key is what you type;
    /// the value is the command (and any fixed arguments) executed inside
    /// the container. Extra arguments from the invocation are appended.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) quick: BTreeMap<String, Vec<String>>,
}

/// Image settings.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Image {
    /// Path to a Dockerfile with a literal `FROM silo-base:latest`; `None`
    /// uses Silo's embedded development-extras layer.
    pub(crate) dockerfile: Option<PathBuf>,
}

/// Settings applied when creating a container.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Container {
    /// Number of CPUs allocated to the container. `None` uses Apple
    /// container's configured default.
    pub(crate) cpus: Option<usize>,
    /// Memory allocated to the container, using Apple container's accepted
    /// syntax (for example `4G`). `None` uses its configured default.
    pub(crate) memory: Option<String>,
    /// Grants the base image's `silo` user passwordless sudo access.
    pub(crate) sudo: bool,
}

/// Settings for the project workspace mounted into the container.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Workspace {
    /// Project-relative directories overlaid read-only in the container.
    pub(crate) read_only: Vec<PathBuf>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            read_only: vec![PathBuf::from(".git")],
        }
    }
}

/// Shells guaranteed to be available in every Silo-compatible image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Shell {
    Bash,
    Zsh,
    Fish,
    Nu,
}

/// One existing host directory exposed inside the container.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bind {
    pub(crate) source: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) access: Permission,
}

/// Configured writable state grouped by persistence scope.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct State {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) project: BTreeMap<String, StateEntry>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) user: BTreeMap<String, StateEntry>,
}

/// One writable state directory managed by Silo.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateEntry {
    pub(crate) target: PathBuf,
}

/// Effective access passed to the container runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Permission {
    /// The container can read the source but not modify it.
    ReadOnly,
    /// The container can read and modify the source.
    ReadWrite,
}

impl Config {
    /// Loads the optional global config followed by the optional project
    /// `.silo.toml`.
    ///
    /// # Errors
    ///
    /// Returns an error when a source cannot be read, merged, or deserialized.
    pub(crate) fn load_for_project(project_root: &Path) -> Result<Self> {
        let mut builder = ConfigLoader::builder();
        if let Some(path) = config_path() {
            builder = builder.add_source(File::from(path).required(false));
        }
        builder = builder.add_source(File::from(project_root.join(".silo.toml")).required(false));
        let mut config = builder.build()?.try_deserialize::<Self>()?;
        config.resolve_paths(project_root);
        Ok(config)
    }

    fn resolve_paths(&mut self, base: &Path) {
        if let Some(dockerfile) = &mut self.image.dockerfile
            && !dockerfile.as_os_str().is_empty()
            && dockerfile.is_relative()
        {
            *dockerfile = base.join(&*dockerfile);
        }
        for bind in self.binds.values_mut() {
            if !bind.source.as_os_str().is_empty()
                && bind.source.is_relative()
                && !bind.source.as_os_str().as_encoded_bytes().starts_with(b"~")
            {
                bind.source = base.join(&bind.source);
            }
        }
    }
}

fn is_default<T>(value: &T) -> bool
where
    T: Default + PartialEq,
{
    value == &T::default()
}

/// Keeps configured and discovered mount names on one shared storage contract.
pub(crate) fn valid_mount_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Returns the config file path: `$XDG_CONFIG_HOME/silo/config.toml` when the
/// variable is set, otherwise `~/.config/silo/config.toml`. Returns `None`
/// when no home directory can be determined.
fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

pub(crate) fn config_path_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = match xdg {
        Some(dir) if !dir.is_empty() && Path::new(dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(home.filter(|dir| !dir.is_empty() && Path::new(dir).is_absolute())?)
            .join(".config"),
    };
    Some(base.join("silo").join("config.toml"))
}

#[cfg(test)]
mod tests;
