//! User configuration, read from `~/.config/silo/config.toml` (or
//! `$XDG_CONFIG_HOME/silo/config.toml` when that variable is set).
//!
//! The file is optional: a default one is written on first use and every key
//! has a default, so a missing or partial file always yields a usable
//! [`Config`]. Precedence is `defaults < config file < CLI flags`; there are
//! no config-mirroring flags yet, but `load` is called once at startup and the
//! resulting [`Config`] is passed down, so flags can override later without a
//! refactor.

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
    /// Quick commands: `silo <name>` runs this command inside the container
    /// without typing `silo run --` every time. The key is what you type;
    /// the value is the command (and any fixed arguments) executed inside
    /// the container. Extra arguments from the invocation are appended.
    pub quick: BTreeMap<String, Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            image: Image::default(),
            // Protection on by default; disable it in the config file.
            read_only_git: true,
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

    /// Parses the config from TOML text, filling missing keys with defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not valid TOML or does not match the
    /// config schema; the error includes the offending line and column.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
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
}

/// Returns the run directory: `$XDG_CONFIG_HOME/silo/run` when the variable
/// is set, otherwise `~/.config/silo/run`. Files and env vars placed there
/// are injected into the container at start (see `silo run`). Returns `None`
/// when no home directory can be determined.
pub(crate) fn run_dir() -> Option<PathBuf> {
    run_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
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

/// Pure version of [`run_dir`], taking the environment values as arguments
/// so the resolution rules are testable without mutating the process
/// environment.
fn run_dir_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    config_dir_from(xdg, home).map(|dir| dir.join("run"))
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
