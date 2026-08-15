//! User configuration, read from `~/.config/silo/config.toml` (or
//! `$XDG_CONFIG_HOME/silo/config.toml` when that variable is set), then
//! overridden by the discovered project's `.silo.toml` file.
//!
//! The global file is optional: a default one is written on first use and
//! every key has a default, so a missing or partial file always yields a
//! usable [`Config`]. Project settings are partial and replace explicitly
//! present global options. Precedence is `defaults < global config < project
//! config < CLI flags`. Named mounts and forwards are the exceptions to
//! whole-collection replacement: project entries overlay global entries by
//! name and field.
//!
//! On top of the built-in mounts (the shared project directory and the
//! optional read-only `.git`), the `mounts` table provides named host mounts
//! and managed project- or user-scoped state that survives automatic
//! container removal.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

/// Contents of the concise starter config file created on first use.
const DEFAULT_CONFIG: &str = include_str!("default.toml");

/// User configuration, loaded once at startup and passed to commands.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub image: Image,
    /// Resource limits for containers created by Silo. Omitted limits are
    /// left to Apple container's configured defaults.
    pub container: Container,
    /// Named host loopback ports exposed to this project's container.
    /// Project config may add entries or overlay global entries by name and
    /// field.
    pub forward: BTreeMap<String, Forward>,
    /// Interactive shell used by the built-in image. When omitted, Silo
    /// mirrors a supported host `$SHELL` and falls back to Zsh.
    pub shell: Option<Shell>,
    /// Whether the project's `.git` directory is mounted read-only in the
    /// container, so tools inside it cannot modify version control state.
    pub read_only_git: bool,
    /// Named configurable mounts, grouped under `mounts.host`,
    /// `mounts.project`, and `mounts.shared`. Project configuration overlays
    /// these by name and field.
    #[serde(default, deserialize_with = "deserialize_mounts")]
    pub mounts: BTreeMap<String, Mount>,
    /// Quick commands: `silo <name>` runs this command inside the container
    /// without typing `silo run --` every time. The key is what you type;
    /// the value is the command (and any fixed arguments) executed inside
    /// the container. Extra arguments from the invocation are appended.
    pub quick: BTreeMap<String, Vec<String>>,
}

/// Partial configuration read from a project's `.silo.toml`.
///
/// Collection fields are optional so omission can inherit the global value.
/// Quick commands replace as a collection; named mounts merge by entry.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectConfig {
    image: ProjectImage,
    container: ProjectContainer,
    forward: Option<BTreeMap<String, ProjectForward>>,
    shell: Option<Shell>,
    read_only_git: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_mounts")]
    mounts: Option<BTreeMap<String, Mount>>,
    quick: Option<BTreeMap<String, Vec<String>>>,
}

/// Image overrides supplied by a project. An omitted Dockerfile inherits the
/// global setting; project configuration intentionally has no reset sentinel.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectImage {
    dockerfile: Option<PathBuf>,
}

/// Container resource overrides supplied by a project. Each omitted limit
/// inherits the corresponding global setting.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectContainer {
    cpus: Option<usize>,
    memory: Option<String>,
}

/// Partial forward supplied by a project configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectForward {
    port: Option<u16>,
    enabled: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            image: Image::default(),
            container: Container::default(),
            forward: BTreeMap::new(),
            shell: None,
            // Protection on by default; disable it in the config file.
            read_only_git: true,
            mounts: BTreeMap::new(),
            quick: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Applies invocation-specific resource limits after global and project
    /// configuration have been merged.
    pub fn apply_container_overrides(&mut self, cpus: Option<usize>, memory: Option<String>) {
        if let Some(cpus) = cpus {
            self.container.cpus = Some(cpus);
        }
        if let Some(memory) = memory {
            self.container.memory = Some(memory);
        }
    }

    /// Renders the merged configuration in the compact style used by the
    /// starter file. Runtime-delegated values and empty sections are omitted,
    /// while concrete mount defaults are made explicit.
    ///
    /// # Errors
    ///
    /// Returns an error if an internal mount has no user-facing category or
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
        if let Some(shell) = self.shell {
            append_toml_value(&mut output, "shell", &shell)?;
        }
        append_toml_value(&mut output, "read_only_git", &self.read_only_git)?;

        // Keep forwards compact and aligned with named mount syntax.
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

        // Group normalized mounts back into their user-facing categories.
        let mut mounts = EffectiveMountTables::default();
        for (name, entry) in &self.mounts {
            match entry.kind() {
                Some(MountKind::Host) => {
                    mounts.host.insert(
                        name.clone(),
                        EffectiveHostMount {
                            enabled: entry.is_enabled(),
                            source: entry.source.clone(),
                            target: entry.target.clone(),
                            writable: entry.writable.unwrap_or(false),
                        },
                    );
                }
                Some(MountKind::ProjectState) => {
                    mounts.project.insert(
                        name.clone(),
                        EffectiveManagedMount {
                            enabled: entry.is_enabled(),
                            target: entry.target.clone(),
                        },
                    );
                }
                Some(MountKind::SharedState) => {
                    mounts.shared.insert(
                        name.clone(),
                        EffectiveManagedMount {
                            enabled: entry.is_enabled(),
                            target: entry.target.clone(),
                        },
                    );
                }
                None => {
                    return Err(anyhow!(
                        "cannot print mount `{name}` because it has no config category"
                    ));
                }
            }
        }
        append_toml_section(&mut output, "mounts.host", &mounts.host)?;
        append_toml_section(&mut output, "mounts.project", &mounts.project)?;
        append_toml_section(&mut output, "mounts.shared", &mounts.shared)?;

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
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Image {
    /// Path to a Dockerfile defining a custom image; `None` uses the
    /// built-in image.
    pub dockerfile: Option<PathBuf>,
}

/// Resource limits passed to Apple container when creating a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Container {
    /// Number of CPUs allocated to the container. `None` uses Apple
    /// container's configured default.
    pub cpus: Option<usize>,
    /// Memory allocated to the container, using Apple container's accepted
    /// syntax (for example `4G`). `None` uses its configured default.
    pub memory: Option<String>,
}

/// One named host loopback port exposed to a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
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

/// Shells guaranteed to be available in Silo's built-in image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Nu,
}

/// One normalized layer of a named mount definition. The config-facing
/// category selects `kind`; this representation keeps the container layer and
/// global/project merging independent of TOML layout.
#[derive(Debug, Clone, Default)]
pub struct Mount {
    pub enabled: Option<bool>,
    pub kind: Option<MountKind>,
    pub source: Option<PathBuf>,
    pub target: Option<PathBuf>,
    pub writable: Option<bool>,
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
            Some(MountKind::Host) | None if !self.writable.unwrap_or(false) => Permission::ReadOnly,
            Some(MountKind::Host | MountKind::ProjectState | MountKind::SharedState) | None => {
                Permission::ReadWrite
            }
        }
    }
}

/// Config-facing mount categories. Keeping the category outside each entry
/// makes `source` and `target` mean the same thing everywhere they appear.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MountTables {
    host: BTreeMap<String, HostMount>,
    project: BTreeMap<String, ManagedMount>,
    shared: BTreeMap<String, ManagedMount>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HostMount {
    enabled: Option<bool>,
    source: Option<PathBuf>,
    target: Option<PathBuf>,
    writable: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ManagedMount {
    enabled: Option<bool>,
    target: Option<PathBuf>,
}

#[derive(Default)]
struct EffectiveMountTables {
    host: BTreeMap<String, EffectiveHostMount>,
    project: BTreeMap<String, EffectiveManagedMount>,
    shared: BTreeMap<String, EffectiveManagedMount>,
}

#[derive(Serialize)]
struct EffectiveHostMount {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<PathBuf>,
    writable: bool,
}

#[derive(Serialize)]
struct EffectiveManagedMount {
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

fn deserialize_mounts<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, Mount>, D::Error>
where
    D: Deserializer<'de>,
{
    let tables = MountTables::deserialize(deserializer)?;
    let mut mounts = BTreeMap::new();
    for (name, entry) in tables.host {
        mounts.insert(
            name,
            Mount {
                enabled: entry.enabled,
                kind: Some(MountKind::Host),
                source: entry.source,
                target: entry.target,
                writable: entry.writable,
            },
        );
    }
    for (kind, entries) in [
        (MountKind::ProjectState, tables.project),
        (MountKind::SharedState, tables.shared),
    ] {
        for (name, entry) in entries {
            if mounts.contains_key(&name) {
                return Err(D::Error::custom(format!(
                    "mount `{name}` is defined in more than one category"
                )));
            }
            mounts.insert(
                name,
                Mount {
                    enabled: entry.enabled,
                    kind: Some(kind),
                    source: None,
                    target: entry.target,
                    writable: None,
                },
            );
        }
    }
    Ok(mounts)
}

fn deserialize_optional_mounts<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<BTreeMap<String, Mount>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_mounts(deserializer).map(Some)
}

/// The source and sharing lifetime of a named mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountKind {
    /// Existing host content, bind-mounted into the container.
    Host,
    /// Silo-managed storage private to one canonical project path.
    ProjectState,
    /// Silo-managed storage reused by every project with the same mount name.
    SharedState,
}

/// Effective access passed to the container runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// The container can read the source but not modify it.
    ReadOnly,
    /// The container can read and modify the source.
    ReadWrite,
}

impl Config {
    /// Loads the config from its default location, creating a default file
    /// on first use.
    ///
    /// # Errors
    ///
    /// Returns an error when the config file exists but cannot be read or
    /// parsed. Unknown keys produce warnings and are otherwise ignored.
    pub fn load() -> Result<Self> {
        let Some(path) = config_path() else {
            // No home directory to read from; run on defaults.
            return Ok(Self::default());
        };
        Self::load_from(&path)
    }

    /// Loads the global configuration and applies `.silo.toml` from
    /// `project_root` when it exists.
    ///
    /// Relative project Dockerfile and host-mount source paths are resolved
    /// from the project root. Mounts merge by name and field; quick commands
    /// still replace their global collection when explicitly supplied.
    ///
    /// # Errors
    ///
    /// Returns an error when either configuration file cannot be read,
    /// parsed, or validated.
    pub fn load_for_project(project_root: &Path) -> Result<Self> {
        let config = Self::load()?;
        Self::apply_project_file(config, project_root)
    }

    /// Loads the config from `path`, creating a default file there on first
    /// use.
    ///
    /// # Errors
    ///
    /// Returns an error when the config file exists but cannot be read or
    /// parsed. Unknown keys produce warnings and are otherwise ignored.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            write_default(path);
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file `{}`", path.display()))?;
        let (mut config, unknown_keys) = Self::deserialize_with_unknown_keys(&text)
            .with_context(|| format!("invalid config in `{}`", path.display()))?;
        config.resolve_mount_sources(path.parent().unwrap_or(Path::new(".")));
        config
            .validate()
            .with_context(|| format!("invalid config in `{}`", path.display()))?;
        warn_unknown_keys(path, &unknown_keys);
        Ok(config)
    }

    /// Applies the project file to an already loaded global configuration.
    fn apply_project_file(mut config: Self, project_root: &Path) -> Result<Self> {
        let path = project_root.join(".silo.toml");
        if !path.is_file() {
            return Ok(config);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read project config file `{}`", path.display()))?;
        let (mut project, unknown_keys) = deserialize_toml::<ProjectConfig>(&text)
            .with_context(|| format!("invalid project config in `{}`", path.display()))?;
        project.resolve_paths(project_root);
        config
            .apply_project(project)
            .with_context(|| format!("invalid project config in `{}`", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid project config in `{}`", path.display()))?;
        warn_unknown_keys(&path, &unknown_keys);
        Ok(config)
    }

    /// Replaces every global option explicitly supplied by the project.
    fn apply_project(&mut self, project: ProjectConfig) -> Result<()> {
        if let Some(dockerfile) = project.image.dockerfile {
            self.image.dockerfile = Some(dockerfile);
        }
        if let Some(cpus) = project.container.cpus {
            self.container.cpus = Some(cpus);
        }
        if let Some(memory) = project.container.memory {
            self.container.memory = Some(memory);
        }
        if let Some(forwards) = project.forward {
            for (name, overlay) in forwards {
                if let Some(entry) = self.forward.get_mut(&name) {
                    if let Some(port) = overlay.port {
                        entry.port = port;
                    }
                    if overlay.enabled.is_some() {
                        entry.enabled = overlay.enabled;
                    }
                } else {
                    let port = overlay.port.ok_or_else(|| {
                        anyhow!(
                            "project forward `{name}` must define `port` when adding a new entry"
                        )
                    })?;
                    self.forward.insert(
                        name,
                        Forward {
                            port,
                            enabled: overlay.enabled,
                        },
                    );
                }
            }
        }
        if let Some(shell) = project.shell {
            self.shell = Some(shell);
        }
        if let Some(read_only_git) = project.read_only_git {
            self.read_only_git = read_only_git;
        }
        if let Some(mounts) = project.mounts {
            for (name, overlay) in mounts {
                match self.mounts.get_mut(&name) {
                    Some(base) => merge_mount(base, overlay),
                    None => {
                        self.mounts.insert(name, overlay);
                    }
                }
            }
        }
        if let Some(quick) = project.quick {
            self.quick = quick;
        }
        Ok(())
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
        let (config, unknown_keys) = Self::deserialize_with_unknown_keys(text)?;
        config.validate()?;
        Ok((config, unknown_keys))
    }

    fn deserialize_with_unknown_keys(text: &str) -> Result<(Self, Vec<String>)> {
        Ok(deserialize_toml::<Self>(text)?)
    }

    fn resolve_mount_sources(&mut self, base: &Path) {
        resolve_mount_sources(&mut self.mounts, base);
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
    /// mounts. Host source existence is checked later at `silo run`, since the
    /// file system can change between commands.
    ///
    /// # Errors
    ///
    /// Returns an error describing an invalid resource or every invalid
    /// shared entry.
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
            let label = format!("mount `{name}`");
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
                problems.push(format!("{label}: enabled entry has no mount category"));
                continue;
            };
            if entry.target.is_none() {
                problems.push(format!("{label}: enabled entry is missing `target`"));
            }
            if kind == MountKind::Host && entry.source.is_none() {
                problems.push(format!("{label}: host entry is missing `source`"));
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
            "invalid mount {noun} in the `mounts` config option: {}",
            problems.join("; ")
        ))
    }
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

impl ProjectConfig {
    /// Makes relative project-owned paths independent of the invocation's
    /// current working directory. Empty paths remain empty so validation can
    /// report the intended error instead of resolving them to the root.
    fn resolve_paths(&mut self, project_root: &Path) {
        if let Some(dockerfile) = &mut self.image.dockerfile
            && !dockerfile.as_os_str().is_empty()
            && dockerfile.is_relative()
        {
            *dockerfile = project_root.join(&*dockerfile);
        }
        if let Some(mounts) = &mut self.mounts {
            resolve_mount_sources(mounts, project_root);
        }
    }
}

fn merge_mount(base: &mut Mount, overlay: Mount) {
    let overlay_kind = overlay.kind();
    if overlay_kind.is_some() && overlay_kind != base.kind() {
        base.source = None;
        base.target = None;
        base.writable = None;
    }
    if overlay.enabled.is_some() {
        base.enabled = overlay.enabled;
    }
    if overlay.kind.is_some() {
        base.kind = overlay.kind;
    }
    if overlay.source.is_some() {
        base.source = overlay.source;
    }
    if overlay.target.is_some() {
        base.target = overlay.target;
    }
    if overlay.writable.is_some() {
        base.writable = overlay.writable;
    }
}

fn resolve_mount_sources(mounts: &mut BTreeMap<String, Mount>, base: &Path) {
    for entry in mounts.values_mut() {
        let Some(source) = &mut entry.source else {
            continue;
        };
        if !source.as_os_str().is_empty()
            && source.is_relative()
            && !source.as_os_str().to_string_lossy().starts_with('~')
        {
            *source = base.join(&*source);
        }
    }
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
    if path.is_relative() {
        let anchored = text
            .strip_prefix("~/")
            .or_else(|| text.strip_prefix("./"))
            .ok_or_else(|| {
                anyhow!(
                    "path must start with `./` for the project, `~/` for the container home, or `/` for an absolute location"
                )
            })?;
        if Path::new(anchored)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(anyhow!("path must not escape its target root with `..`"));
        }
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
