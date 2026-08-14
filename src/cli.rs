use std::ffi::OsString;

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
        /// Command to run inside the container; empty runs the default shell.
        #[arg(value_name = "COMMAND", last = true)]
        command: Vec<OsString>,
    },
    /// List and manage Silo containers.
    Containers {
        #[command(subcommand)]
        command: Option<ContainersCommand>,
    },
    /// List and manage Silo-owned persistent mounts.
    Mounts {
        #[command(subcommand)]
        command: Option<MountsCommand>,
    },
    /// Run a configured quick command: `silo <name> [args...]`, where `name`
    /// is a key in the `[quick]` section of the config file; extra arguments
    /// are appended.
    #[command(external_subcommand)]
    Quick(Vec<OsString>),
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
    /// Stop one Silo container without deleting it.
    Stop {
        /// Exact container ID, unique ID prefix, project path, or unique project name.
        selector: String,
        /// Terminate active sessions before stopping the container.
        #[arg(long)]
        force: bool,
    },
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
pub(crate) enum MountsCommand {
    /// List all Silo-owned persistent mounts.
    #[command(alias = "ls")]
    List,
    /// Permanently delete one unused persistent mount.
    #[command(alias = "rm")]
    Delete {
        /// Exact mount ID, unique ID prefix, project, or logical mount name.
        selector: String,
    },
}
