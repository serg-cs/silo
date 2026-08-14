use std::ffi::OsString;

use clap::{Parser, error::ErrorKind};

use super::{Cli, Command};

fn parse(args: &[&str]) -> Command {
    let arguments: Vec<OsString> = args.iter().map(OsString::from).collect();
    Cli::try_parse_from(arguments)
        .expect("arguments parse")
        .command
}

#[test]
fn run_accepts_resource_overrides() {
    let Command::Run {
        isolated: false,
        cpus: Some(cpus),
        memory: Some(memory),
        command,
    } = parse(&[
        "silo", "run", "--cpus", "4", "--memory", "8G", "--", "codex",
    ])
    else {
        panic!("expected run command with resource overrides");
    };

    assert_eq!(cpus.get(), 4);
    assert_eq!(memory, "8G");
    assert_eq!(command, [OsString::from("codex")]);
}

#[test]
fn run_accepts_independent_resource_overrides_without_a_command() {
    let Command::Run {
        cpus: Some(cpus),
        memory: None,
        command,
        ..
    } = parse(&["silo", "run", "--cpus", "2"])
    else {
        panic!("expected CPU override");
    };
    assert_eq!(cpus.get(), 2);
    assert!(command.is_empty());

    let Command::Run {
        cpus: None,
        memory: Some(memory),
        command,
        ..
    } = parse(&["silo", "run", "--memory", "4G"])
    else {
        panic!("expected memory override");
    };
    assert_eq!(memory, "4G");
    assert!(command.is_empty());
}

#[test]
fn run_rejects_invalid_resource_overrides() {
    for arguments in [
        &["silo", "run", "--cpus", "0"][..],
        &["silo", "run", "--memory", "  "][..],
    ] {
        let error = Cli::try_parse_from(arguments)
            .err()
            .expect("invalid resource override is rejected");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }
}

#[test]
fn resource_flags_after_double_dash_are_command_arguments() {
    let Command::Run {
        cpus: None,
        memory: None,
        command,
        ..
    } = parse(&["silo", "run", "--", "tool", "--cpus", "3", "--memory", "6G"])
    else {
        panic!("expected run command");
    };

    assert_eq!(
        command,
        [
            OsString::from("tool"),
            OsString::from("--cpus"),
            OsString::from("3"),
            OsString::from("--memory"),
            OsString::from("6G"),
        ]
    );
}
