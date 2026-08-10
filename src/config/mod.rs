//! User configuration, read from `~/.config/silo/config.toml` (or
//! `$XDG_CONFIG_HOME/silo/config.toml` when that variable is set), then
//! overridden by the discovered project's `.silo.toml` file.
//!
//! The global file is optional: a default one is written on first use and
//! every key has a default, so a missing or partial file always yields a
//! usable [`Config`]. Project settings are partial and replace explicitly
//! present global options. Precedence is `defaults < global config < project
//! config < CLI flags`.
//!
//! On top of the built-in mounts (the shared project directory and the
//! optional read-only `.git`), the `shared` option mounts additional host
//! paths into the container, each read-only or read-write (see [`Shared`]).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// Contents of a freshly created config file; every key is documented there,
/// since the file doubles as the user-facing reference.
const DEFAULT_CONFIG: &str = include_str!("default.toml");

/// User configuration, loaded once at startup and passed to commands.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub image: Image,
    /// Whether the project's `.git` directory is mounted read-only in the
    /// container, so tools inside it cannot modify version control state.
    pub read_only_git: bool,
    /// Configurable shared mounts, applied when a project's shared container
    /// is created: each entry mounts a host `source` at container `target`,
    /// read-only or read-write. `silo stop` and the next run recreate them.
    pub shared: Vec<Shared>,
    /// Quick commands: `silo <name>` runs this command inside the container
    /// without typing `silo run --` every time. The key is what you type;
    /// the value is the command (and any fixed arguments) executed inside
    /// the container. Extra arguments from the invocation are appended.
    pub quick: BTreeMap<String, Vec<String>>,
}

/// Partial configuration read from a project's `.silo.toml`.
///
/// Collection fields are optional so omission can inherit the global value,
/// while an explicitly empty collection can clear it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectConfig {
    image: ProjectImage,
    read_only_git: Option<bool>,
    shared: Option<Vec<Shared>>,
    quick: Option<BTreeMap<String, Vec<String>>>,
}

/// Image overrides supplied by a project. An omitted Dockerfile inherits the
/// global setting; project configuration intentionally has no reset sentinel.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectImage {
    dockerfile: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            image: Image::default(),
            // Protection on by default; disable it in the config file.
            read_only_git: true,
            shared: Vec::new(),
            quick: BTreeMap::new(),
        }
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

/// One configurable shared mount: `source` on the host is mounted into the
/// container at `target`, with the given [`Permission`].
#[derive(Debug, Clone, Deserialize)]
pub struct Shared {
    /// Path on the host to mount: an absolute path, or a `~`-prefixed path
    /// like `~/notes` (a bare `~` is the home directory itself; expanded when
    /// the shared container is created; `~user` paths are not supported).
    /// The path must exist at creation or the run fails.
    pub source: PathBuf,
    /// Absolute path inside the container where the source is mounted.
    pub target: PathBuf,
    /// How the mount may be used inside the container. Defaults to
    /// [`Permission::ReadOnly`] — protection on by default, like
    /// `read_only_git`.
    #[serde(default = "default_permission")]
    pub permission: Permission,
}

/// How a shared mount may be used inside the container. An enum rather
/// than a boolean, so new permissions can be added later without reshaping
/// the config schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// The container can read the source but not modify it.
    ReadOnly,
    /// The container can read and modify the source.
    ReadWrite,
}

/// Shared mounts are read-only unless the config says otherwise.
fn default_permission() -> Permission {
    Permission::ReadOnly
}

impl Config {
    /// Loads the config from its default location, creating a default file
    /// on first use.
    ///
    /// # Errors
    ///
    /// Returns an error when the config file exists but cannot be read or
    /// parsed.
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
    /// Relative project Dockerfile and shared-source paths are resolved from
    /// the project root. Project collection options replace their global
    /// counterparts completely, including when explicitly empty.
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
    /// parsed.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            write_default(path);
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file `{}`", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config in `{}`", path.display()))
    }

    /// Applies the project file to an already loaded global configuration.
    fn apply_project_file(mut config: Self, project_root: &Path) -> Result<Self> {
        let path = project_root.join(".silo.toml");
        if !path.is_file() {
            return Ok(config);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read project config file `{}`", path.display()))?;
        let mut project: ProjectConfig = toml::from_str(&text)
            .with_context(|| format!("invalid project config in `{}`", path.display()))?;
        project.resolve_paths(project_root);
        config.apply_project(project);
        config
            .validate()
            .with_context(|| format!("invalid project config in `{}`", path.display()))?;
        Ok(config)
    }

    /// Replaces every global option explicitly supplied by the project.
    fn apply_project(&mut self, project: ProjectConfig) {
        if let Some(dockerfile) = project.image.dockerfile {
            self.image.dockerfile = Some(dockerfile);
        }
        if let Some(read_only_git) = project.read_only_git {
            self.read_only_git = read_only_git;
        }
        if let Some(shared) = project.shared {
            self.shared = shared;
        }
        if let Some(quick) = project.quick {
            self.quick = quick;
        }
    }

    /// Parses the config from TOML text, filling missing keys with defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not valid TOML or does not match the
    /// config schema; the error includes the offending line and column.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
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

    /// Rejects shared mounts that can never be mounted: an empty source or
    /// target, an unresolved source that is neither absolute nor
    /// `~`-prefixed (`~user` paths are unsupported), a target that is not an
    /// absolute container path, or a path the container CLI cannot parse
    /// (valid UTF-8 without `:`). Whether the source actually exists is
    /// checked later at `silo run`, when the entry is resolved against the
    /// file system, since the file system can change between commands.
    ///
    /// # Errors
    ///
    /// Returns an error describing every invalid entry.
    fn validate(&self) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();
        for (index, entry) in self.shared.iter().enumerate() {
            let label = format!(
                "shared entry {} (`{}` -> `{}`)",
                index + 1,
                entry.source.display(),
                entry.target.display()
            );
            if entry.source.as_os_str().is_empty() {
                problems.push(format!("{label}: source path is empty"));
            } else if !entry.source.is_absolute() && !entry.source.starts_with("~") {
                problems.push(format!(
                    "{label}: source path is not absolute and does not start with a bare `~`; use an absolute path or a `~`-prefixed path like `~/notes` (expanded to your home directory; `~user` paths are not supported)"
                ));
            } else if let Err(reason) = spec_path(&entry.source) {
                problems.push(format!("{label}: source {reason}"));
            }
            if entry.target.as_os_str().is_empty() {
                problems.push(format!("{label}: target path is empty"));
            } else if !entry.target.is_absolute() {
                problems.push(format!("{label}: target path is not absolute"));
            } else if let Err(reason) = spec_path(&entry.target) {
                problems.push(format!("{label}: target {reason}"));
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
            "invalid shared {noun} in the `shared` config option: {}",
            problems.join("; ")
        ))
    }
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
        if let Some(shared) = &mut self.shared {
            for entry in shared {
                if !entry.source.as_os_str().is_empty()
                    && entry.source.is_relative()
                    && !entry.source.as_os_str().to_string_lossy().starts_with('~')
                {
                    entry.source = project_root.join(&entry.source);
                }
            }
        }
    }
}

/// Returns `Ok` when `path` can appear in a container volume spec: valid
/// UTF-8 without `:` (the `:` separates the host and container sides of the
/// spec).
fn spec_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8"))?;
    if text.contains(':') {
        return Err(anyhow!("path contains `:`"));
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

/// Writes the default config file, warning instead of failing when it cannot
/// be created, so an unwritable home directory never breaks a command.
fn write_default(path: &Path) {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Err(err) = fs::create_dir_all(parent).and_then(|()| fs::write(path, DEFAULT_CONFIG)) {
        eprintln!(
            "warning: could not create default config at `{}`: {err:#}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests;
