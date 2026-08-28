//! User-facing configuration commands.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, anyhow};

use crate::config::{Config, DEFAULT_CONFIG, config_path_from};
use crate::output::{write_json, write_stdout};

/// Prints the merged configuration as TOML or JSON.
pub(super) fn print_effective(config: &Config, json: bool) -> Result<ExitCode> {
    if json {
        write_json(config)?;
    } else {
        let text = toml::to_string_pretty(config).context("failed to serialize configuration")?;
        write_stdout(&text)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints the embedded starter config without consulting filesystem state.
pub(super) fn print_default() -> Result<ExitCode> {
    write_stdout(DEFAULT_CONFIG)?;
    Ok(ExitCode::SUCCESS)
}

/// Reports a successful validation after all loader warnings have been
/// emitted.
pub(super) fn print_valid() -> Result<ExitCode> {
    write_stdout("configuration is valid\n")?;
    Ok(ExitCode::SUCCESS)
}

/// Prints existing config files in precedence order.
pub(super) fn print_paths(project_root: &Path) -> Result<ExitCode> {
    let paths = active_config_paths_from(
        project_root,
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    write_stdout(&config_paths_text(&paths))?;
    Ok(ExitCode::SUCCESS)
}

/// Opens the applicable config in the user's chosen editor.
pub(super) fn edit(project_root: &Path, global: bool) -> Result<()> {
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

pub(super) fn active_config_paths_from(
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

pub(super) fn config_paths_text(paths: &[(&str, PathBuf)]) -> String {
    if paths.is_empty() {
        return "built-in\t<embedded defaults>\n".to_string();
    }
    let mut text = String::new();
    for (kind, path) in paths {
        text.push_str(kind);
        text.push('\t');
        text.push_str(&path.to_string_lossy());
        text.push('\n');
    }
    text
}

pub(super) fn edit_path_from(
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

pub(super) fn run_editor(
    path: &Path,
    visual: Option<&OsStr>,
    editor: Option<&OsStr>,
) -> Result<()> {
    let editor = selected_editor(visual, editor);
    let editor = editor.to_str().ok_or_else(|| {
        anyhow!(
            "configured editor is not valid UTF-8: {}",
            editor.to_string_lossy()
        )
    })?;
    // Interpret the editor setting through the shell while preserving the
    // config path as one quoted positional argument.
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

pub(super) fn selected_editor(
    visual: Option<&OsStr>,
    editor: Option<&OsStr>,
) -> std::ffi::OsString {
    visual
        .filter(|value| !value.is_empty())
        .or_else(|| editor.filter(|value| !value.is_empty()))
        .unwrap_or_else(|| OsStr::new("vi"))
        .to_os_string()
}

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
