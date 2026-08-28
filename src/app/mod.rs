use clap::{CommandFactory, Parser};
use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::anyhow;

use crate::cli::{Cli, Command, ConfigCommand, ContainersCommand, ImageCommand, StateCommand};
use crate::{config, container, host_ports, image, project, storage};

mod config_command;

#[derive(Clone, Copy)]
enum ValidationProfile {
    Standard,
    PostEdit,
    Check,
}

pub(crate) fn run() -> ExitCode {
    try_run(Cli::parse()).unwrap_or_else(|err| fail(&err))
}

/// Dispatches each command with only the project and configuration context it needs.
fn try_run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Config { command } => run_config_command(command.as_ref()),
        Command::Containers { command } => match command {
            None => container::print_containers(false),
            Some(ContainersCommand::List { json }) => container::print_containers(json),
            Some(ContainersCommand::Delete { selector, force }) => {
                container::delete_selected_container(&selector, force)
            }
        },
        Command::State { command } => match command {
            None => storage::managed::print_state(false),
            Some(StateCommand::List { json }) => storage::managed::print_state(json),
            Some(StateCommand::Delete { selector }) => {
                storage::managed::delete_selected_state(&selector)
            }
        },
        Command::Image {
            command: ImageCommand::Build,
        } => {
            let project_root = project::current_project_root()?;
            let config = load_config(&project_root, None, None, false)?;
            validate_config(&config, &project_root, ValidationProfile::Standard)?;
            image::build(&config)
        }
        Command::Run {
            isolated,
            cpus,
            memory,
            sudo,
            command,
        } => {
            let (project, config) = load_project_config(
                cpus.map(std::num::NonZeroUsize::get),
                memory.as_deref(),
                sudo,
            )?;
            container::run_session(&config, &project, &command, isolated)
        }
        Command::Quick(args) => {
            let (project, config) = load_project_config(None, None, false)?;
            let command = quick_command(&config, &args)?;
            container::run_session(&config, &project, &command, false)
        }
    }
}

/// Discovers the project and loads its effective configuration with run overrides.
fn load_project_config(
    cpus: Option<usize>,
    memory: Option<&str>,
    sudo: bool,
) -> anyhow::Result<(project::Project, config::Config)> {
    let project = project::Project::current()?;
    let config = load_config(&project.root, cpus, memory, sudo)?;
    validate_config(&config, &project.root, ValidationProfile::Standard)?;
    Ok((project, config))
}

/// Runs config-only commands using the validation profile each operation needs.
/// Inspection and editing remain available from directories such as `/`, while
/// `check` requires a root that could be mounted as a workspace.
fn run_config_command(command: Option<&ConfigCommand>) -> anyhow::Result<ExitCode> {
    match command {
        None | Some(ConfigCommand::List { .. }) => {
            let json = matches!(command, Some(ConfigCommand::List { json: true }));
            let project_root = project::current_project_root()?;
            let config = load_config(&project_root, None, None, false)?;
            validate_config(&config, &project_root, ValidationProfile::Standard)?;
            config_command::print_effective(&config, json)
        }
        Some(ConfigCommand::Edit { global }) => {
            let project_root = project::current_project_root()?;
            config_command::edit(&project_root, *global)?;
            let config = load_config(&project_root, None, None, false)?;
            validate_config(&config, &project_root, ValidationProfile::PostEdit)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(ConfigCommand::Path) => config_command::print_paths(&project::current_project_root()?),
        Some(ConfigCommand::Check) => {
            let project_root = project::current_project_root()?;
            validate_check_project_root(&project_root)?;
            let config = load_config(&project_root, None, None, false)?;
            validate_config(&config, &project_root, ValidationProfile::Check)?;
            config_command::print_valid()
        }
        Some(ConfigCommand::Default) => config_command::print_default(),
    }
}

/// Loads the effective config and applies invocation-only overrides.
fn load_config(
    project_root: &std::path::Path,
    cpus: Option<usize>,
    memory: Option<&str>,
    sudo: bool,
) -> anyhow::Result<config::Config> {
    let mut config = config::Config::load_for_project(project_root)?;
    if let Some(cpus) = cpus {
        config.container.cpus = Some(cpus);
    }
    if let Some(memory) = memory {
        config.container.memory = Some(memory.to_string());
    }
    if sudo {
        config.container.sudo = true;
    }
    Ok(config)
}

/// Applies the validation required by one command without mutating runtime or
/// managed state.
fn validate_config(
    config: &config::Config,
    project_root: &std::path::Path,
    profile: ValidationProfile,
) -> anyhow::Result<()> {
    validate_quick_commands(config)?;
    container::validate_config(config, project_root)?;
    host_ports::validate_ports(&config.host_ports)?;
    if matches!(
        profile,
        ValidationProfile::PostEdit | ValidationProfile::Check
    ) {
        image::validate_config(config)?;
    }
    if matches!(profile, ValidationProfile::Check) {
        container::validate_project_filesystem(config, project_root)?;
    }
    Ok(())
}

fn validate_check_project_root(project_root: &std::path::Path) -> anyhow::Result<()> {
    project::Project::from_root(project_root.to_path_buf())
        .map(|_| ())
        .map_err(|_| {
            anyhow!(
                "cannot check configuration from `{}` because it cannot be used as a Silo workspace; run this command from a project directory",
                project_root.display()
            )
        })
}

/// Returns the project's built-in subcommand names, enumerated from the CLI
/// definition so the set stays in sync with the commands the project
/// actually defines. `help` is clap's built-in subcommand: it is resolved
/// before any quick command lookup, so a quick command of that name could
/// never run either.
fn builtin_commands() -> Vec<String> {
    let mut names: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|subcommand| subcommand.get_name().to_string())
        .collect();
    names.push("help".to_string());
    names
}

fn validate_quick_commands(config: &config::Config) -> anyhow::Result<()> {
    let builtins = builtin_commands();
    for (name, command) in &config.quick {
        let reason = if name.trim().is_empty() {
            Some("is empty or contains only whitespace")
        } else if name.starts_with('-') {
            Some("starts with `-`, which silo reserves for its own options")
        } else if builtins.iter().any(|builtin| builtin == name) {
            Some("is shadowed by a built-in command")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(anyhow!(
                "unusable quick command name: `{name}` {reason}; rename it in the `[quick]` section of the config file"
            ));
        }
        let Some(executable) = command.first() else {
            return Err(anyhow!(
                "unusable quick command `{name}`: command array is empty; provide an executable as its first value"
            ));
        };
        if executable.trim().is_empty() {
            return Err(anyhow!(
                "unusable quick command `{name}`: executable is empty or contains only whitespace"
            ));
        }
    }
    Ok(())
}

/// Resolves a quick command invocation (`silo <name> [args...]`) into the
/// command to run inside the container: the configured base command with the
/// invocation's extra arguments appended. `silo run -- <command>` bypasses
/// this entirely.
///
/// # Errors
///
/// Returns an error when the name is not valid UTF-8 or is not configured.
fn quick_command(config: &config::Config, args: &[OsString]) -> anyhow::Result<Vec<OsString>> {
    let Some(name) = args.first() else {
        return Err(anyhow!("quick command is missing its name"));
    };
    let name = name.to_str().ok_or_else(|| {
        anyhow!(
            "quick command name is not valid UTF-8: {}",
            name.to_string_lossy()
        )
    })?;
    let base = config.quick.get(name).ok_or_else(|| {
        anyhow!(
            "unknown quick command `{name}`; define it under `[quick]` in your silo \
             config file, or run any command directly with `silo run -- <command>`"
        )
    })?;
    let mut command: Vec<OsString> = base.iter().map(OsString::from).collect();
    command.extend(args[1..].iter().cloned());
    Ok(command)
}

/// Prints the error to stderr and returns the failure exit code.
fn fail(err: &anyhow::Error) -> ExitCode {
    eprintln!("error: {err:#}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests;
