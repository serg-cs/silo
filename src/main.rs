use clap::{Parser, Subcommand};
use std::process::ExitCode;

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
    /// Build the image from the embedded Dockerfile.
    Build,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Image {
            command: ImageCommand::Build,
        } => container::build_image(),
        Command::Run => container::run_image(),
    }
    .unwrap_or_else(|err| {
        eprintln!("error: {err:#}");
        ExitCode::FAILURE
    })
}
