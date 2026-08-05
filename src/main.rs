use clap::Parser;

/// A simple CLI for silo.
#[derive(Parser)]
#[command(name = "silo", version, about)]
struct Cli;

fn main() {
    Cli::parse();
}
