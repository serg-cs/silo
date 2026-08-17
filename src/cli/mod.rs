use std::ffi::OsString;
use std::num::NonZeroUsize;

use clap::{Parser, Subcommand};

/// Run development tools in lightweight, project-scoped containers.
#[derive(Parser)]
#[command(name = "silo", version, about)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Inspect and manage Silo configuration.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Manage the image used by Silo.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Run a session in the shared container for the current project.
    ///
    /// Executes COMMAND inside it instead of the default shell, passed
    /// after `--`: `silo run -- <command>`. A `[quick]` config entry runs
    /// as `silo <name>`.
    Run {
        /// Use a separate one-shot container removed when the session ends.
        #[arg(long)]
        isolated: bool,
        /// Override the number of CPUs allocated to the container.
        #[arg(long)]
        cpus: Option<NonZeroUsize>,
        /// Override the memory allocated to the container (for example `4G`).
        #[arg(long, value_parser = parse_memory)]
        memory: Option<String>,
        /// Grant passwordless sudo access in the built-in container.
        #[arg(long)]
        sudo: bool,
        /// Command to run inside the container; empty runs the default shell.
        #[arg(value_name = "COMMAND", last = true)]
        command: Vec<OsString>,
    },
    /// List and manage Silo containers.
    Containers {
        #[command(subcommand)]
        command: Option<ContainersCommand>,
    },
    /// List and manage Silo-owned persistent state.
    State {
        #[command(subcommand)]
        command: Option<StateCommand>,
    },
    /// Run a configured quick command: `silo <name> [args...]`, where `name`
    /// is a key in the `[quick]` section of the config file; extra arguments
    /// are appended.
    #[command(external_subcommand)]
    Quick(Vec<OsString>),
}

#[derive(Subcommand)]
pub(crate) enum ConfigCommand {
    /// Print the effective configuration for the current project.
    #[command(alias = "ls")]
    List,
    /// Edit the nearest project configuration, or the global configuration.
    Edit {
        /// Edit the global configuration even when a project file exists.
        #[arg(long)]
        global: bool,
    },
    /// Print the bundled starter configuration.
    Default,
    /// Print the active configuration file paths in precedence order.
    Path,
    /// Validate the effective configuration for the current project.
    Check,
}

#[derive(Subcommand)]
pub(crate) enum ImageCommand {
    /// Rebuild the configured Silo image without cached layers.
    Build,
}

#[derive(Subcommand)]
pub(crate) enum ContainersCommand {
    /// List all shared and isolated Silo containers.
    #[command(alias = "ls")]
    List,
    /// Stop and delete one Silo container.
    #[command(alias = "rm")]
    Delete {
        /// Exact container ID, unique ID prefix, project path, or unique project name.
        selector: String,
        /// Terminate active sessions before deleting the container.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum StateCommand {
    /// List all Silo-owned persistent state.
    #[command(alias = "ls")]
    List,
    /// Permanently delete one unused state directory.
    #[command(alias = "rm")]
    Delete {
        /// Exact state ID, unique ID prefix, project, or logical state name.
        selector: String,
    },
}

/// Keeps CLI memory validation aligned with the configuration schema while
/// leaving accepted size syntax to Apple Container.
fn parse_memory(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err("memory must not be empty".to_string());
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests;
