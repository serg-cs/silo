use clap::{Parser, Subcommand};
use std::process::ExitCode;

mod config;
mod container;

/// A tiny wrapper around Apple's `container` CLI for macOS.
#[derive(Parser)]
#[command(name = "silo", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the image.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Run the container from the built image.
    Run,
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Build the image: the embedded Dockerfile by default, or the one
    /// configured in the config file.
    Build,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Loaded once at startup and passed down, so config can be overridden by
    // CLI flags later without restructuring (defaults < config < flags).
    let config = match config::Config::load() {
        Ok(config) => config,
        Err(err) => return fail(&err),
    };
    match cli.command {
        Command::Image {
            command: ImageCommand::Build,
        } => container::build_image(&config),
        Command::Run => container::run_image(&config),
    }
    .unwrap_or_else(|err| fail(&err))
}

/// Prints the error to stderr and returns the failure exit code.
fn fail(err: &anyhow::Error) -> ExitCode {
    eprintln!("error: {err:#}");
    ExitCode::FAILURE
}
