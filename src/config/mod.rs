//! User configuration, read from `~/.config/silo/config.toml` (or
//! `$XDG_CONFIG_HOME/silo/config.toml` when that variable is set), then
//! overridden by the discovered project's `.silo.toml` file.
//!
//! The global file is optional: a default one is written on first use and
//! every key has a default, so a missing or partial file always yields a
//! usable [`Config`]. Project settings are partial and replace explicitly
//! present global options. Precedence is `defaults < global config < project
//! config < CLI flags`. Named binds, state entries, forwards, and quick
//! commands overlay global entries by name; structured entries merge by field.
//!
//! `workspace.read_only` protects selected project directories. `binds`
//! exposes existing host directories with explicit access, while `state`
//! provides Silo-managed writable storage with project or user scope.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, anyhow};
use config_rs::{Config as LayeredConfig, File, FileFormat, FileSourceString};
use serde::{Deserialize, Deserializer, Serialize};

/// Contents of the concise starter config file created on first use.
const DEFAULT_CONFIG: &str = include_str!("default.toml");
const CONTAINER_CPUS_PATH: &str = "container.cpus";
const CONTAINER_MEMORY_PATH: &str = "container.memory";
const CONTAINER_SUDO_PATH: &str = "container.sudo";

type LayeredConfigBuilder = config_rs::builder::ConfigBuilder<config_rs::builder::DefaultState>;

/// User configuration, loaded once at startup and passed to commands.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub image: Image,
    /// Creation settings for containers managed by Silo.
    pub container: Container,
    /// Named host loopback ports exposed to this project's container.
    /// Project config may add entries or overlay global entries by name and
    /// field.
    pub forward: BTreeMap<String, Forward>,
    /// Interactive shell supplied by the Silo base image. When omitted, Silo
    /// mirrors a supported host `$SHELL` and falls back to Zsh.
    pub shell: Option<Shell>,
    /// Settings for the project workspace mounted into the container.
    pub workspace: Workspace,
    /// Normalized binds and managed state used by the container layer.
    /// Project configuration overlays entries by logical name and field.
    pub mounts: BTreeMap<String, Mount>,
    /// Quick commands: `silo <name>` runs this command inside the container
    /// without typing `silo run --` every time. The key is what you type;
    /// the value is the command (and any fixed arguments) executed inside
    /// the container. Extra arguments from the invocation are appended.
    pub quick: BTreeMap<String, Vec<String>>,
}

/// One strict, partial TOML layer prepared before generic merging.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct ConfigLayer {
    image: LayerImage,
    container: LayerContainer,
    forward: BTreeMap<String, LayerForward>,
    shell: Option<Shell>,
    workspace: LayerWorkspace,
    binds: BTreeMap<String, Bind>,
    state: StateTables,
    quick: BTreeMap<String, Vec<String>>,
}

/// User-facing global configuration before binds and scoped state are
/// normalized for runtime processing.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct ConfigDocument {
    image: Image,
    container: Container,
    forward: BTreeMap<String, Forward>,
    shell: Option<Shell>,
    workspace: Workspace,
    binds: BTreeMap<String, Bind>,
    state: StateTables,
    quick: BTreeMap<String, Vec<String>>,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document = ConfigDocument::deserialize(deserializer)?;
        Ok(Self {
            image: document.image,
            container: document.container,
            forward: document.forward,
            shell: document.shell,
            workspace: document.workspace,
            mounts: normalize_mounts(document.binds, document.state)?,
            quick: document.quick,
        })
    }
}

/// Image values supplied by one configuration layer.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct LayerImage {
    dockerfile: Option<PathBuf>,
}

/// Container values supplied by one configuration layer.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct LayerContainer {
    cpus: Option<usize>,
    memory: Option<String>,
    sudo: Option<bool>,
}

/// Partial forward supplied by one configuration layer.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct LayerForward {
    port: Option<u16>,
    enabled: Option<bool>,
}

/// Typed invocation values applied after every file-backed layer.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    cpus: Option<usize>,
    memory: Option<String>,
    /// `false` means the one-way CLI flag was not supplied, not an override.
    sudo: bool,
}

impl ConfigOverrides {
    pub fn for_run(cpus: Option<usize>, memory: Option<String>, sudo: bool) -> Self {
        Self { cpus, memory, sudo }
    }

    fn apply(&self, mut builder: LayeredConfigBuilder) -> Result<LayeredConfigBuilder> {
        let cpus = self
            .cpus
            .map(u64::try_from)
            .transpose()
            .context("CLI CPU count does not fit the configuration value range")?;
        builder = builder
            .set_override_option(CONTAINER_CPUS_PATH, cpus)
            .context("failed to apply the CLI CPU override")?
            .set_override_option(CONTAINER_MEMORY_PATH, self.memory.clone())
            .context("failed to apply the CLI memory override")?;
        if self.sudo {
            builder = builder
                .set_override(CONTAINER_SUDO_PATH, true)
                .context("failed to apply the CLI sudo override")?;
        }
        Ok(builder)
    }
}

impl Config {
    /// Renders the merged configuration in the compact style used by the
    /// starter file. Runtime-delegated values and empty sections are omitted,
    /// while concrete bind and state defaults are made explicit.
    ///
    /// # Errors
    ///
    /// Returns an error if an internal entry has no user-facing category or
    /// a TOML value cannot be serialized.
    pub(crate) fn effective_toml(&self) -> Result<String> {
        // Keep scalar settings in the same order and dotted form as the
        // starter config.
        let mut output = String::new();
        if let Some(dockerfile) = &self.image.dockerfile {
            append_toml_value(&mut output, "image.dockerfile", dockerfile)?;
        }
        if let Some(cpus) = self.container.cpus {
            append_toml_value(&mut output, "container.cpus", &cpus)?;
        }
        if let Some(memory) = &self.container.memory {
            append_toml_value(&mut output, "container.memory", memory)?;
        }
        if self.container.sudo {
            append_toml_value(&mut output, "container.sudo", &true)?;
        }
        if let Some(shell) = self.shell {
            append_toml_value(&mut output, "shell", &shell)?;
        }
        append_toml_value(
            &mut output,
            "workspace.read_only",
            &self.workspace.read_only,
        )?;

        // Keep forwards compact and aligned with named-entry syntax.
        let forwards = self
            .forward
            .iter()
            .map(|(name, entry)| {
                (
                    name.clone(),
                    EffectiveForward {
                        port: entry.port,
                        enabled: entry.is_enabled(),
                    },
                )
            })
            .collect();
        append_toml_section(&mut output, "forward", &forwards)?;

        // Group normalized runtime entries back into the user-facing model.
        let mut entries = EffectiveEntryTables::default();
        for (name, entry) in &self.mounts {
            match entry.kind() {
                Some(MountKind::Host) => {
                    entries.binds.insert(
                        name.clone(),
                        EffectiveBind {
                            enabled: entry.is_enabled(),
                            source: entry.source.clone(),
                            target: entry.target.clone(),
                            access: entry.access.unwrap_or(Permission::ReadOnly),
                        },
                    );
                }
                Some(MountKind::ProjectState) => {
                    entries.project.insert(
                        name.clone(),
                        EffectiveStateEntry {
                            enabled: entry.is_enabled(),
                            target: entry.target.clone(),
                        },
                    );
                }
                Some(MountKind::UserState) => {
                    entries.user.insert(
                        name.clone(),
                        EffectiveStateEntry {
                            enabled: entry.is_enabled(),
                            target: entry.target.clone(),
                        },
                    );
                }
                None => {
                    return Err(anyhow!(
                        "cannot print entry `{name}` because it has no config category"
                    ));
                }
            }
        }
        append_toml_section(&mut output, "binds", &entries.binds)?;
        append_toml_section(&mut output, "state.project", &entries.project)?;
        append_toml_section(&mut output, "state.user", &entries.user)?;

        // Use the document serializer for arbitrary quick-command keys while
        // suppressing its otherwise-empty table.
        if !self.quick.is_empty() {
            start_toml_section(&mut output);
            let quick = toml::to_string(&EffectiveQuick { quick: &self.quick })
                .context("failed to serialize effective quick commands")?;
            output.push_str(&quick);
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }
        Ok(output)
    }

    /// Prints the merged config as user-facing TOML.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization or stdout writing fails.
    pub(crate) fn print_effective(&self) -> Result<ExitCode> {
        write_stdout(&self.effective_toml()?)?;
        Ok(ExitCode::SUCCESS)
    }
}

/// Image settings.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Image {
    /// Path to a Dockerfile with a literal `FROM silo-base:latest`; `None`
    /// uses Silo's embedded development-extras layer.
    pub dockerfile: Option<PathBuf>,
}

/// Settings applied when creating a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Container {
    /// Number of CPUs allocated to the container. `None` uses Apple
    /// container's configured default.
    pub cpus: Option<usize>,
    /// Memory allocated to the container, using Apple container's accepted
    /// syntax (for example `4G`). `None` uses its configured default.
    pub memory: Option<String>,
    /// Grants the base image's `silo` user passwordless sudo access.
    pub sudo: bool,
}

/// Settings for the project workspace mounted into the container.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Workspace {
    /// Project-relative directories overlaid read-only in the container.
    pub read_only: Vec<PathBuf>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            read_only: vec![PathBuf::from(".git")],
        }
    }
}

/// Partial workspace settings supplied by one configuration layer.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct LayerWorkspace {
    read_only: Option<Vec<PathBuf>>,
}

/// One named host loopback port exposed to a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Forward {
    pub port: u16,
    pub enabled: Option<bool>,
}

impl Forward {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// Shells guaranteed to be available in every Silo-compatible image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Nu,
}

/// One normalized layer of a named bind or state definition. The config-facing
/// category selects `kind`; this representation keeps the container layer and
/// global/project merging independent of TOML layout.
#[derive(Debug, Clone, Default)]
pub struct Mount {
    pub enabled: Option<bool>,
    pub kind: Option<MountKind>,
    pub source: Option<PathBuf>,
    pub target: Option<PathBuf>,
    pub access: Option<Permission>,
}

impl Mount {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn kind(&self) -> Option<MountKind> {
        self.kind
    }

    pub fn host_source(&self) -> Option<&Path> {
        (self.kind == Some(MountKind::Host))
            .then_some(self.source.as_deref())
            .flatten()
    }

    pub fn effective_target(&self, project_dir: &Path) -> Option<PathBuf> {
        let target = self.target.as_deref()?;
        if target.is_absolute() {
            Some(target.to_path_buf())
        } else {
            let text = target.to_str()?;
            text.strip_prefix("~/")
                .map(|relative| Path::new(CONTAINER_HOME).join(relative))
                .or_else(|| {
                    text.strip_prefix("./")
                        .map(|relative| project_dir.join(relative))
                })
        }
    }

    pub fn effective_access(&self) -> Permission {
        match self.kind() {
            Some(MountKind::Host) | None => self.access.unwrap_or(Permission::ReadOnly),
            Some(MountKind::ProjectState | MountKind::UserState) => Permission::ReadWrite,
        }
    }
}

/// Config-facing managed state grouped by its persistence scope.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct StateTables {
    project: BTreeMap<String, StateEntry>,
    user: BTreeMap<String, StateEntry>,
}

/// One existing host directory exposed inside the container.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Bind {
    enabled: Option<bool>,
    source: Option<PathBuf>,
    target: Option<PathBuf>,
    access: Option<Permission>,
}

/// One Silo-managed writable state directory.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct StateEntry {
    enabled: Option<bool>,
    target: Option<PathBuf>,
}

#[derive(Default)]
struct EffectiveEntryTables {
    binds: BTreeMap<String, EffectiveBind>,
    project: BTreeMap<String, EffectiveStateEntry>,
    user: BTreeMap<String, EffectiveStateEntry>,
}

#[derive(Serialize)]
struct EffectiveBind {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<PathBuf>,
    access: Permission,
}

#[derive(Serialize)]
struct EffectiveStateEntry {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<PathBuf>,
}

#[derive(Serialize)]
struct EffectiveForward {
    port: u16,
    enabled: bool,
}

#[derive(Serialize)]
struct EffectiveQuick<'a> {
    quick: &'a BTreeMap<String, Vec<String>>,
}

/// Appends one static config key while delegating TOML value escaping and
/// inline-table formatting to the existing serializer.
fn append_toml_value<T>(output: &mut String, key: &str, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    output.push_str(key);
    output.push_str(" = ");
    value
        .serialize(toml::ser::ValueSerializer::new(output))
        .with_context(|| format!("failed to serialize effective config key `{key}`"))?;
    output.push('\n');
    Ok(())
}

fn append_toml_section<T>(
    output: &mut String,
    name: &str,
    entries: &BTreeMap<String, T>,
) -> Result<()>
where
    T: Serialize,
{
    if entries.is_empty() {
        return Ok(());
    }
    start_toml_section(output);
    output.push('[');
    output.push_str(name);
    output.push_str("]\n");
    for (key, value) in entries {
        append_toml_value(output, key, value)?;
    }
    Ok(())
}

fn start_toml_section(output: &mut String) {
    if !output.is_empty() && !output.ends_with("\n\n") {
        output.push('\n');
    }
}

fn normalize_mounts<E>(
    binds: BTreeMap<String, Bind>,
    state: StateTables,
) -> std::result::Result<BTreeMap<String, Mount>, E>
where
    E: serde::de::Error,
{
    let mut mounts = BTreeMap::new();
    for (name, entry) in binds {
        mounts.insert(
            name,
            Mount {
                enabled: entry.enabled,
                kind: Some(MountKind::Host),
                source: entry.source,
                target: entry.target,
                access: entry.access,
            },
        );
    }
    for (kind, entries) in [
        (MountKind::ProjectState, state.project),
        (MountKind::UserState, state.user),
    ] {
        for (name, entry) in entries {
            if mounts.contains_key(&name) {
                return Err(E::custom(format!(
                    "entry `{name}` is defined in more than one bind or state category"
                )));
            }
            mounts.insert(
                name,
                Mount {
                    enabled: entry.enabled,
                    kind: Some(kind),
                    source: None,
                    target: entry.target,
                    access: None,
                },
            );
        }
    }
    Ok(mounts)
}

/// The source and persistence scope of a normalized runtime mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountKind {
    /// Existing host content, bind-mounted into the container.
    Host,
    /// Silo-managed storage private to one canonical project path.
    ProjectState,
    /// Silo-managed storage reused by this user across projects.
    UserState,
}

/// Effective access passed to the container runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// The container can read the source but not modify it.
    ReadOnly,
    /// The container can read and modify the source.
    ReadWrite,
}

/// Converts a strict layer into a merge source without losing TOML semantics.
///
/// The text round trip is deliberate. The config-rs typed serializer builds
/// path expressions from map keys and cannot represent an empty sequence, so
/// using it directly would reinterpret dotted quick-command names and omit
/// `[]`. TOML preserves both after Silo has already enforced the layer's types.
fn layer_source<T>(document: &T, description: &str) -> Result<File<FileSourceString, FileFormat>>
where
    T: Serialize,
{
    let text = toml::to_string(document)
        .with_context(|| format!("failed to prepare {description} for merging"))?;
    Ok(File::from_str(&text, FileFormat::Toml))
}

fn build_defaults() -> Result<LayeredConfig> {
    let source = layer_source(&ConfigDocument::default(), "built-in defaults")?;
    LayeredConfig::builder()
        .add_source(source)
        .build()
        .context("failed to build the default configuration")
}

fn build_layered(
    base: LayeredConfig,
    layer: &ConfigLayer,
    description: &str,
) -> Result<LayeredConfig> {
    let source = layer_source(layer, description)?;
    LayeredConfig::builder()
        .add_source(base)
        .add_source(source)
        .build()
        .with_context(|| format!("failed to merge {description}"))
}

impl Config {
    /// Loads the global configuration and applies `.silo.toml` from
    /// `project_root` when it exists.
    ///
    /// Host-side paths are resolved relative to their declaring file. Named
    /// forwards, mounts, and quick commands merge by name and field.
    ///
    /// # Errors
    ///
    /// Returns an error when either configuration file cannot be read,
    /// parsed, or validated.
    pub fn load_for_project(project_root: &Path, overrides: &ConfigOverrides) -> Result<Self> {
        let layered = match config_path() {
            Some(path) => Self::load_layered_from(&path)?,
            None => build_defaults()?,
        };
        Self::apply_project_layer(layered, project_root, overrides)
    }

    /// Loads the config from `path`, creating a default file there on first
    /// use.
    ///
    /// # Errors
    ///
    /// Returns an error when the config file exists but cannot be read or
    /// parsed. Unknown keys produce warnings and are otherwise ignored.
    #[cfg(test)]
    pub fn load_from(path: &Path) -> Result<Self> {
        let layered = Self::load_layered_from(path)?;
        Self::extract_validated(&layered)
    }

    fn load_layered_from(path: &Path) -> Result<LayeredConfig> {
        if !path.exists() {
            write_default(path);
            return build_defaults();
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file `{}`", path.display()))?;
        let (mut layer, unknown_keys) = deserialize_toml::<ConfigLayer>(&text)
            .with_context(|| format!("invalid config in `{}`", path.display()))?;
        layer
            .resolve_paths(path.parent().unwrap_or(Path::new(".")))
            .with_context(|| format!("invalid config in `{}`", path.display()))?;

        // Validate the global stage before a project can mask any problem.
        let layered = build_layered(build_defaults()?, &layer, "global configuration")
            .with_context(|| format!("invalid config in `{}`", path.display()))?;
        Self::extract_validated(&layered)
            .with_context(|| format!("invalid config in `{}`", path.display()))?;
        warn_unknown_keys(path, &unknown_keys);
        Ok(layered)
    }

    fn apply_project_layer(
        layered: LayeredConfig,
        project_root: &Path,
        overrides: &ConfigOverrides,
    ) -> Result<Self> {
        let path = project_root.join(".silo.toml");
        let mut builder = LayeredConfig::builder().add_source(layered);
        let mut unknown_keys = None;
        if path.is_file() {
            let text = fs::read_to_string(&path).with_context(|| {
                format!("failed to read project config file `{}`", path.display())
            })?;
            let (mut layer, ignored) = deserialize_toml::<ConfigLayer>(&text)
                .with_context(|| format!("invalid project config in `{}`", path.display()))?;
            layer
                .resolve_paths(project_root)
                .with_context(|| format!("invalid project config in `{}`", path.display()))?;
            let source = layer_source(&layer, "project configuration")
                .with_context(|| format!("invalid project config in `{}`", path.display()))?;
            builder = builder.add_source(source);
            unknown_keys = Some(ignored);
        }

        // Keep invocation values final so a valid CLI value may intentionally
        // replace an otherwise-invalid project value for this run.
        let layered = overrides
            .apply(builder)?
            .build()
            .context("failed to merge configuration layers")?;
        let config = Self::extract_validated(&layered).with_context(|| {
            if path.is_file() {
                format!("invalid project config in `{}`", path.display())
            } else {
                "invalid effective configuration".to_string()
            }
        })?;
        if let Some(unknown_keys) = unknown_keys {
            warn_unknown_keys(&path, &unknown_keys);
        }
        Ok(config)
    }

    /// Parses the config from TOML text, filling missing keys with defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not valid TOML or does not match the
    /// config schema; the error includes the offending line and column.
    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Self> {
        let (config, _) = Self::parse_with_unknown_keys(text)?;
        Ok(config)
    }

    /// Parses and validates config text while retaining the paths of fields
    /// ignored by Serde so file-loading callers can warn about likely typos.
    #[cfg(test)]
    fn parse_with_unknown_keys(text: &str) -> Result<(Self, Vec<String>)> {
        let (layer, unknown_keys) = deserialize_toml::<ConfigLayer>(text)?;
        let layered = build_layered(build_defaults()?, &layer, "test configuration")?;
        let config = Self::extract_validated(&layered)?;
        Ok((config, unknown_keys))
    }

    #[cfg(test)]
    fn parse_with_overrides(text: &str, overrides: &ConfigOverrides) -> Result<Self> {
        let (layer, _) = deserialize_toml::<ConfigLayer>(text)?;
        let layered = build_layered(build_defaults()?, &layer, "test configuration")?;
        let layered = overrides
            .apply(LayeredConfig::builder().add_source(layered))?
            .build()
            .context("failed to merge test overrides")?;
        Self::extract_validated(&layered)
    }

    fn extract_validated(layered: &LayeredConfig) -> Result<Self> {
        let config = layered
            .clone()
            .try_deserialize::<Self>()
            .map_err(anyhow::Error::from)?;
        config.validate()?;
        Ok(config)
    }

    #[cfg(test)]
    fn apply_project_file(config: &Self, project_root: &Path) -> Result<Self> {
        let text = config.effective_toml()?;
        let (layer, _) = deserialize_toml::<ConfigLayer>(&text)?;
        let layered = build_layered(build_defaults()?, &layer, "test global configuration")?;
        Self::apply_project_layer(layered, project_root, &ConfigOverrides::default())
    }

    /// Returns an error when a quick command name can never be reached: a
    /// name colliding with one of the project's built-in subcommand names
    /// (the built-in command always wins when the user types it), or a name
    /// starting with `-` (silo reserves flag-shaped tokens for the CLI's own
    /// options). Such a name is unreachable, so the caller reports it as a
    /// warning instead of failing the invocation.
    ///
    /// `builtins` is the project's actual command set, supplied by the CLI
    /// layer (which enumerates it from the command definitions), so the
    /// built-in check stays in sync with the commands the project defines
    /// instead of hardcoding a list.
    ///
    /// # Errors
    ///
    /// Returns an error describing every unusable quick command name.
    pub fn check_quick_names<S: AsRef<str>>(&self, builtins: &[S]) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();
        for name in self.quick.keys() {
            if name.starts_with('-') {
                problems.push(format!(
                    "`{name}` starts with `-`, which silo reserves for its own options"
                ));
            } else if builtins
                .iter()
                .any(|builtin| builtin.as_ref() == name.as_str())
            {
                problems.push(format!("`{name}` is shadowed by a built-in command"));
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        let noun = if problems.len() == 1 { "name" } else { "names" };
        let pronoun = if problems.len() == 1 { "it" } else { "them" };
        Err(anyhow!(
            "unusable quick command {noun}: {}; rename {pronoun} in the `[quick]` section of the config file",
            problems.join("; ")
        ))
    }

    /// Rejects impossible resource limits and incomplete or malformed enabled
    /// binds and state. Bind source existence is checked later at `silo run`,
    /// since the file system can change between commands.
    ///
    /// # Errors
    ///
    /// Returns an error describing an invalid resource or configured entry.
    fn validate(&self) -> Result<()> {
        if self.container.cpus == Some(0) {
            return Err(anyhow!(
                "invalid `container.cpus` config option: CPU count must be greater than zero"
            ));
        }
        if self
            .container
            .memory
            .as_ref()
            .is_some_and(|memory| memory.trim().is_empty())
        {
            return Err(anyhow!(
                "invalid `container.memory` config option: memory must not be empty"
            ));
        }

        validate_read_only_paths(&self.workspace.read_only)?;

        let mut forward_problems = Vec::new();
        for (name, entry) in &self.forward {
            let valid_name = name.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || (index > 0 && (byte.is_ascii_digit() || byte == b'_'))
            });
            if !valid_name || name.is_empty() {
                forward_problems.push(format!(
                    "forward `{name}`: name must start with a lowercase ASCII letter and contain only lowercase ASCII letters, numbers, or `_`"
                ));
            }
            if entry.port < 1024 {
                forward_problems.push(format!(
                    "forward `{name}`: port must be between 1024 and 65535"
                ));
            }
        }
        if !forward_problems.is_empty() {
            let noun = if forward_problems.len() == 1 {
                "entry"
            } else {
                "entries"
            };
            return Err(anyhow!(
                "invalid forward {noun} in the `[forward]` config option: {}",
                forward_problems.join("; ")
            ));
        }

        let mut problems: Vec<String> = Vec::new();
        for (name, entry) in &self.mounts {
            let label = format!("bind or state entry `{name}`");
            if !valid_mount_name(name) {
                problems.push(format!(
                    "{label}: name must start with an ASCII letter or number and contain only ASCII letters, numbers, `_`, or `-`"
                ));
            }
            if let Some(source) = &entry.source
                && let Err(reason) = config_path_reason(source, false)
            {
                problems.push(format!("{label}: source {reason}"));
            }
            if let Some(target) = &entry.target
                && let Err(reason) = container_path_reason(target)
            {
                problems.push(format!("{label}: target {reason}"));
            }
            if !entry.is_enabled() {
                continue;
            }
            let Some(kind) = entry.kind else {
                problems.push(format!("{label}: enabled entry has no config category"));
                continue;
            };
            if entry.target.is_none() {
                problems.push(format!("{label}: enabled entry is missing `target`"));
            }
            if kind == MountKind::Host && entry.source.is_none() {
                problems.push(format!("{label}: bind is missing `source`"));
            }
            if kind == MountKind::Host && entry.access.is_none() {
                problems.push(format!("{label}: bind is missing `access`"));
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        let noun = if problems.len() == 1 {
            "entry"
        } else {
            "entries"
        };
        Err(anyhow!(
            "invalid bind or state {noun}: {}",
            problems.join("; ")
        ))
    }
}

/// Rejects unsafe or equivalent read-only targets as one config error.
fn validate_read_only_paths(paths: &[PathBuf]) -> Result<()> {
    // Compare normalized targets so equivalent entries do not produce
    // repeated overlay mounts.
    let mut normalized_paths = BTreeSet::new();
    let mut problems = Vec::new();
    for path in paths {
        match normalize_read_only_path(path) {
            Ok(normalized) if !normalized_paths.insert(normalized.clone()) => {
                problems.push(format!(
                    "path `{}` duplicates `{}` after normalization",
                    path.display(),
                    normalized.display()
                ));
            }
            Ok(_) => {}
            Err(reason) => problems.push(format!("path `{}` {reason}", path.display())),
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    let noun = if problems.len() == 1 {
        "entry"
    } else {
        "entries"
    };
    Err(anyhow!(
        "invalid {noun} in the `workspace.read_only` config option: {}",
        problems.join("; ")
    ))
}

/// Normalizes a project-relative read-only target without consulting the
/// filesystem. Parent components may cancel an earlier normal component but
/// must never escape the project root.
pub(crate) fn normalize_read_only_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;

    if path.as_os_str().is_empty() {
        return Err(anyhow!("is empty"));
    }
    if path.is_absolute() {
        return Err(anyhow!("must be relative to the project root"));
    }

    let text = path.to_str().ok_or_else(|| anyhow!("is not valid UTF-8"))?;
    if text.contains([':', '\n', '\r']) {
        return Err(anyhow!(
            "must not contain `:`, a newline, or a carriage return"
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if normalized.pop() => {}
            Component::ParentDir => return Err(anyhow!("must not escape the project root")),
            Component::Normal(component) => normalized.push(component),
            Component::Prefix(_) | Component::RootDir => {
                return Err(anyhow!("must be relative to the project root"));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    Ok(normalized)
}

/// Deserializes one TOML document and records every key ignored by the target
/// schema. Paths are sorted so multiple warnings have deterministic order.
fn deserialize_toml<'de, T>(
    text: &'de str,
) -> std::result::Result<(T, Vec<String>), toml::de::Error>
where
    T: Deserialize<'de>,
{
    let deserializer = toml::Deserializer::parse(text)?;
    let mut unknown_keys = Vec::new();
    let value = serde_ignored::deserialize(deserializer, |path| {
        unknown_keys.push(path.to_string().replace(".?.", "."));
    })?;
    unknown_keys.sort();
    Ok((value, unknown_keys))
}

/// Warns once for each schema key ignored while reading `path`.
fn warn_unknown_keys(path: &Path, unknown_keys: &[String]) {
    for key in unknown_keys {
        eprintln!("{}", unknown_key_warning(path, key));
    }
}

/// Formats unknown-key warnings separately from stderr emission so their
/// safety-relevant file and key context can be tested exactly.
fn unknown_key_warning(path: &Path, key: &str) -> String {
    format!(
        "warning: unknown config key `{key}` in `{}`; it will be ignored",
        path.display()
    )
}

impl ConfigLayer {
    /// Resolves host paths against this layer before merging. Resolved paths
    /// must remain representable in TOML and Apple container arguments.
    fn resolve_paths(&mut self, base: &Path) -> Result<()> {
        if let Some(dockerfile) = &mut self.image.dockerfile {
            if !dockerfile.as_os_str().is_empty() && dockerfile.is_relative() {
                *dockerfile = base.join(&*dockerfile);
            }
            ensure_utf8_config_path(dockerfile, "`image.dockerfile`")?;
        }
        for (name, entry) in &mut self.binds {
            let Some(source) = &mut entry.source else {
                continue;
            };
            if !source.as_os_str().is_empty()
                && source.is_relative()
                && !source.as_os_str().to_string_lossy().starts_with('~')
            {
                *source = base.join(&*source);
            }
            ensure_utf8_config_path(source, &format!("source for bind `{name}`"))?;
        }
        Ok(())
    }
}

fn ensure_utf8_config_path(path: &Path, label: &str) -> Result<()> {
    if path.to_str().is_none() {
        return Err(anyhow!(
            "resolved {label} path is not valid UTF-8; Silo requires UTF-8 configuration paths"
        ));
    }
    Ok(())
}

const CONTAINER_HOME: &str = "/home/silo";

fn container_path_reason(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(anyhow!("path is empty"));
    }
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8"))?;
    if text.contains([',', '=']) {
        return Err(anyhow!("path contains `,` or `=`"));
    }
    if text.contains(['\n', '\r']) {
        return Err(anyhow!("path contains a newline"));
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(anyhow!(
            "path must not contain `..`, which can cross container symlinks"
        ));
    }
    if path.is_relative() {
        text
            .strip_prefix("~/")
            .or_else(|| text.strip_prefix("./"))
            .ok_or_else(|| {
                anyhow!(
                    "path must start with `./` for the project, `~/` for the container home, or `/` for an absolute location"
                )
            })?;
    }
    Ok(())
}

fn valid_mount_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn config_path_reason(path: &Path, require_absolute: bool) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(anyhow!("path is empty"));
    }
    if require_absolute && !path.is_absolute() {
        return Err(anyhow!("path is not absolute"));
    }
    if !require_absolute && !path.is_absolute() && !path.starts_with("~") {
        return Err(anyhow!(
            "path is not absolute and does not start with a bare `~`"
        ));
    }
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8"))?;
    if text.contains([',', '=']) {
        return Err(anyhow!("path contains `,` or `=`"));
    }
    if text.contains(['\n', '\r']) {
        return Err(anyhow!("path contains a newline"));
    }
    Ok(())
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

/// Pure version of [`config_path`], taking the environment values as
/// arguments so the resolution rules are testable without mutating the
/// process environment.
fn config_path_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    config_dir_from(xdg, home).map(|dir| dir.join("config.toml"))
}

/// Pure version of the config directory resolution, taking the environment
/// values as arguments so the rules are testable without mutating the
/// process environment. Ignores relative XDG paths per the XDG spec.
pub(crate) fn config_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = match xdg {
        // The XDG spec only allows absolute paths; relative values are
        // ignored and the `~/.config` fallback applies.
        Some(dir) if !dir.is_empty() && Path::new(dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(home?).join(".config"),
    };
    Some(base.join("silo"))
}

/// Prints the embedded starter config without consulting filesystem state.
///
/// # Errors
///
/// Returns an error when stdout cannot be written.
pub(crate) fn print_default() -> Result<ExitCode> {
    write_stdout(DEFAULT_CONFIG)?;
    Ok(ExitCode::SUCCESS)
}

/// Reports a successful validation after all loader warnings have been
/// emitted.
///
/// # Errors
///
/// Returns an error when stdout cannot be written.
pub(crate) fn print_valid() -> Result<ExitCode> {
    write_stdout("configuration is valid\n")?;
    Ok(ExitCode::SUCCESS)
}

/// Prints existing config files that contribute to the current resolution,
/// from the lower-precedence global file to the project override.
///
/// # Errors
///
/// Returns an error when no config file exists or stdout cannot be written.
pub(crate) fn print_paths(project_root: &Path) -> Result<ExitCode> {
    let paths = active_config_paths_from(
        project_root,
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let text = config_paths_text(&paths)?;
    write_stdout(&text)?;
    Ok(ExitCode::SUCCESS)
}

/// Opens the nearest applicable config in the user's chosen editor. A
/// missing global target is initialized from the bundled template first.
///
/// # Errors
///
/// Returns an error when the target path cannot be determined or created,
/// the editor setting is invalid, the editor cannot start, or it fails.
pub(crate) fn edit(project_root: &Path, global: bool) -> Result<()> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let path = edit_path_from(project_root, global, xdg.as_deref(), home.as_deref())?;
    if !path.exists() {
        create_default(&path)?;
    }
    run_editor(
        &path,
        std::env::var_os("VISUAL").as_deref(),
        std::env::var_os("EDITOR").as_deref(),
    )
}

/// Selects only existing files because `config path` reports active inputs,
/// not potential locations. The built-in defaults have no filesystem path.
fn active_config_paths_from(
    project_root: &Path,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Vec<(&'static str, PathBuf)> {
    let mut paths = Vec::new();
    if let Some(path) = config_path_from(xdg, home)
        && path.is_file()
    {
        paths.push(("global", path));
    }
    let project = project_root.join(".silo.toml");
    if project.is_file() {
        paths.push(("project", project));
    }
    paths
}

fn config_paths_text(paths: &[(&str, PathBuf)]) -> Result<String> {
    if paths.is_empty() {
        return Err(anyhow!(
            "no configuration files are active; silo is using built-in defaults"
        ));
    }
    let mut text = String::new();
    for (kind, path) in paths {
        text.push_str(kind);
        text.push('\t');
        text.push_str(&path.to_string_lossy());
        text.push('\n');
    }
    Ok(text)
}

/// Chooses an existing project override unless global editing was requested.
fn edit_path_from(
    project_root: &Path,
    global: bool,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    let project = project_root.join(".silo.toml");
    if !global && project.is_file() {
        return Ok(project);
    }
    config_path_from(xdg, home).ok_or_else(|| {
        anyhow!("cannot determine the global config path; set XDG_CONFIG_HOME or HOME")
    })
}

/// Uses the conventional interactive-editor precedence while allowing
/// settings such as `code --wait` to include arguments.
fn run_editor(path: &Path, visual: Option<&OsStr>, editor: Option<&OsStr>) -> Result<()> {
    let editor = selected_editor(visual, editor);
    let editor = editor.to_str().ok_or_else(|| {
        anyhow!(
            "configured editor is not valid UTF-8: {}",
            editor.to_string_lossy()
        )
    })?;
    // The user's editor setting is intentionally interpreted by the shell;
    // the config path remains a quoted positional argument.
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$@\""))
        .arg("silo-config-editor")
        .arg(path)
        .status()
        .with_context(|| format!("failed to start config editor `{editor}`"))?;
    if !status.success() {
        return Err(anyhow!("config editor exited with status {status}"));
    }
    Ok(())
}

fn selected_editor(visual: Option<&OsStr>, editor: Option<&OsStr>) -> std::ffi::OsString {
    visual
        .filter(|value| !value.is_empty())
        .or_else(|| editor.filter(|value| !value.is_empty()))
        .unwrap_or_else(|| OsStr::new("vi"))
        .to_os_string()
}

fn write_stdout(text: &str) -> Result<()> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .context("failed to write config output")
}

/// Writes the default config file, warning instead of failing when it cannot
/// be created, so an unwritable home directory never breaks a command.
fn write_default(path: &Path) {
    if let Err(err) = create_default(path) {
        eprintln!(
            "warning: could not create default config at `{}`: {err:#}",
            path.display()
        );
    }
}

/// Creates a starter config with strict errors for commands that need to edit
/// the resulting file.
fn create_default(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config directory `{}`", parent.display()))?;
    fs::write(path, DEFAULT_CONFIG)
        .with_context(|| format!("failed to create default config `{}`", path.display()))
}

#[cfg(test)]
mod tests;
