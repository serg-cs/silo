use std::process::ExitCode;

mod app;
mod apple;
mod cli;
mod config;
mod container;
mod digest;
mod host_ports;
mod image;
mod output;
mod project;
mod storage;
#[cfg(test)]
mod test_support;

fn main() -> ExitCode {
    app::run()
}
