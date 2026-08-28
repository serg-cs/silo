use std::ffi::OsString;

use clap::{Parser, error::ErrorKind};

use super::{Cli, Command, ConfigCommand, ContainersCommand, StateCommand};

fn parse(args: &[&str]) -> Command {
    let arguments: Vec<OsString> = args.iter().map(OsString::from).collect();
    Cli::try_parse_from(arguments)
        .expect("arguments parse")
        .command
}

#[test]
fn config_defaults_to_effective_list() {
    assert!(matches!(
        parse(&["silo", "config"]),
        Command::Config { command: None }
    ));
    for name in ["list", "ls"] {
        assert!(matches!(
            parse(&["silo", "config", name]),
            Command::Config {
                command: Some(ConfigCommand::List { json: false })
            }
        ));
    }
}

#[test]
fn explicit_list_commands_accept_json() {
    for group in ["config", "containers", "state"] {
        for list in ["list", "ls"] {
            let command = parse(&["silo", group, list, "--json"]);
            let (Command::Config {
                command: Some(ConfigCommand::List { json }),
            }
            | Command::Containers {
                command: Some(ContainersCommand::List { json }),
            }
            | Command::State {
                command: Some(StateCommand::List { json }),
            }) = command
            else {
                panic!("expected {group} list command");
            };
            assert!(json, "{group} {list}");
        }
    }
}

#[test]
fn json_is_restricted_to_explicit_list_commands() {
    for arguments in [
        &["silo", "config", "--json"][..],
        &["silo", "containers", "--json"][..],
        &["silo", "state", "--json"][..],
        &["silo", "config", "path", "--json"][..],
        &["silo", "containers", "delete", "project", "--json"][..],
        &["silo", "state", "delete", "cache", "--json"][..],
    ] {
        let error = Cli::try_parse_from(arguments)
            .err()
            .expect("JSON is rejected outside explicit list commands");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{arguments:?}");
    }
}

#[test]
fn config_parses_management_subcommands() {
    assert!(matches!(
        parse(&["silo", "config", "default"]),
        Command::Config {
            command: Some(ConfigCommand::Default)
        }
    ));
    assert!(matches!(
        parse(&["silo", "config", "path"]),
        Command::Config {
            command: Some(ConfigCommand::Path)
        }
    ));
    assert!(matches!(
        parse(&["silo", "config", "check"]),
        Command::Config {
            command: Some(ConfigCommand::Check)
        }
    ));
}

#[test]
fn config_edit_accepts_only_its_global_flag() {
    assert!(matches!(
        parse(&["silo", "config", "edit", "--global"]),
        Command::Config {
            command: Some(ConfigCommand::Edit { global: true })
        }
    ));
    for arguments in [
        &["silo", "config", "--global", "edit"][..],
        &["silo", "config", "list", "--global"][..],
        &["silo", "config", "path", "--global"][..],
    ] {
        let error = Cli::try_parse_from(arguments)
            .err()
            .expect("misplaced global flag is rejected");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }
}

#[test]
fn run_accepts_resource_overrides() {
    let Command::Run {
        isolated: false,
        cpus: Some(cpus),
        memory: Some(memory),
        sudo: false,
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
fn run_accepts_sudo_override() {
    let Command::Run {
        sudo: true,
        command,
        ..
    } = parse(&["silo", "run", "--sudo", "--", "id"])
    else {
        panic!("expected sudo override");
    };

    assert_eq!(command, [OsString::from("id")]);
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
fn run_flags_after_double_dash_are_command_arguments() {
    let Command::Run {
        cpus: None,
        memory: None,
        command,
        ..
    } = parse(&[
        "silo", "run", "--", "tool", "--cpus", "3", "--memory", "6G", "--sudo",
    ])
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
            OsString::from("--sudo"),
        ]
    );
}

#[test]
fn run_passes_through_arbitrary_commands() {
    let Command::Run { command, .. } = parse(&["silo", "run", "--", "codex", "--model", "compact"])
    else {
        panic!("expected run command");
    };
    assert_eq!(
        command,
        [
            OsString::from("codex"),
            OsString::from("--model"),
            OsString::from("compact"),
        ]
    );
}

#[test]
fn run_rejects_commands_without_double_dash() {
    for arguments in [
        &["silo", "run", "codex"][..],
        &["silo", "run", "codex", "-t"][..],
        &["silo", "run", "-t"][..],
    ] {
        let error = Cli::try_parse_from(arguments)
            .err()
            .expect("the command requires a preceding `--`");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{arguments:?}");
    }
}

#[test]
fn run_without_a_command_keeps_the_default_shell() {
    for arguments in [&["silo", "run"][..], &["silo", "run", "--"][..]] {
        let Command::Run { command, .. } = parse(arguments) else {
            panic!("expected run command");
        };
        assert!(command.is_empty(), "{arguments:?}");
    }
}

#[test]
fn run_preserves_double_dash_inside_the_command() {
    let Command::Run { command, .. } = parse(&["silo", "run", "--", "codex", "--", "--flag"])
    else {
        panic!("expected run command");
    };
    assert_eq!(
        command,
        [
            OsString::from("codex"),
            OsString::from("--"),
            OsString::from("--flag"),
        ]
    );
}

#[test]
fn unknown_first_token_is_a_quick_command() {
    let Command::Quick(arguments) = parse(&["silo", "codex", "--model", "compact"]) else {
        panic!("expected quick command");
    };
    assert_eq!(
        arguments,
        [
            OsString::from("codex"),
            OsString::from("--model"),
            OsString::from("compact"),
        ]
    );
}

#[test]
fn builtin_subcommands_win_over_quick_commands() {
    assert!(matches!(parse(&["silo", "run"]), Command::Run { .. }));
    assert!(matches!(
        parse(&["silo", "image", "build"]),
        Command::Image { .. }
    ));
    assert!(matches!(
        parse(&["silo", "containers"]),
        Command::Containers { command: None }
    ));
}

#[test]
fn containers_stop_is_not_available() {
    let error = Cli::try_parse_from(["silo", "containers", "stop", "silo-abcd"])
        .err()
        .expect("stop is not a management command");
    assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
}

#[test]
fn container_delete_aliases_parse_force() {
    for command in ["delete", "rm"] {
        let Command::Containers {
            command: Some(ContainersCommand::Delete { selector, force }),
        } = parse(&["silo", "containers", command, "project", "--force"])
        else {
            panic!("expected container deletion");
        };
        assert_eq!(selector, "project");
        assert!(force);
    }
}

#[test]
fn state_defaults_to_list_and_parses_delete_aliases() {
    assert!(matches!(
        parse(&["silo", "state"]),
        Command::State { command: None }
    ));
    assert!(matches!(
        parse(&["silo", "state", "list"]),
        Command::State {
            command: Some(StateCommand::List { json: false })
        }
    ));
    for command in ["delete", "rm"] {
        let Command::State {
            command: Some(StateCommand::Delete { selector }),
        } = parse(&["silo", "state", command, "cargo"])
        else {
            panic!("expected state deletion");
        };
        assert_eq!(selector, "cargo");
    }
}

#[test]
fn mounts_is_not_a_management_alias() {
    let Command::Quick(arguments) = parse(&["silo", "mounts"]) else {
        panic!("mounts should not be a built-in command");
    };
    assert_eq!(arguments, ["mounts"]);
}

#[test]
fn isolated_run_uses_only_the_documented_flag() {
    let Command::Run {
        isolated: true,
        command,
        ..
    } = parse(&["silo", "run", "--isolated", "--", "nu", "-c", "version"])
    else {
        panic!("expected isolated run");
    };
    assert_eq!(
        command,
        [
            OsString::from("nu"),
            OsString::from("-c"),
            OsString::from("version"),
        ]
    );

    let error = Cli::try_parse_from(["silo", "run", "--rm"])
        .err()
        .expect("the removed alias is rejected");
    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
}
