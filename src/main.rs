use clap::{CommandFactory, Parser};
use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::anyhow;

mod cli;
mod config;
mod container;
mod forward;

use cli::{Cli, Command, ConfigCommand, ContainersCommand, ImageCommand, StateCommand};

fn main() -> ExitCode {
    if let Some(result) = forward::run_internal_broker() {
        return result.map_or_else(|err| fail(&err), |()| ExitCode::SUCCESS);
    }
    let cli = Cli::parse();
    if let Command::Config { command } = &cli.command {
        return run_config_command(command.as_ref()).unwrap_or_else(|err| fail(&err));
    }
    match &cli.command {
        Command::Containers { command } => {
            let result = match command {
                None | Some(ContainersCommand::List) => container::print_containers(),
                Some(ContainersCommand::Delete { selector, force }) => {
                    container::delete_selected_container(selector, *force)
                }
            };
            return result.unwrap_or_else(|err| fail(&err));
        }
        Command::State { command } => {
            let result = match command {
                None | Some(StateCommand::List) => container::print_state(),
                Some(StateCommand::Delete { selector }) => {
                    container::delete_selected_state(selector)
                }
            };
            return result.unwrap_or_else(|err| fail(&err));
        }
        _ => {}
    }
    let project = match container::Project::current() {
        Ok(project) => project,
        Err(err) => return fail(&err),
    };
    // Loaded once for every config-consuming command. Precedence is built-in
    // defaults < global config < project config < CLI flags.
    let mut config = match load_config(&project.root) {
        Ok(config) => config,
        Err(err) => return fail(&err),
    };
    let result = match cli.command {
        Command::Config { .. } => unreachable!("config commands returned before project setup"),
        Command::Image {
            command: ImageCommand::Build,
        } => container::build_image(&config),
        Command::Run {
            isolated,
            cpus,
            memory,
            sudo,
            command,
        } => {
            config.apply_container_overrides(cpus.map(std::num::NonZeroUsize::get), memory, sudo);
            container::run_image(&config, &project, &command, isolated)
        }
        Command::Containers { .. } | Command::State { .. } => {
            unreachable!("management commands returned before config")
        }
        Command::Quick(args) => quick_command(&config, &args)
            .and_then(|command| container::run_image(&config, &project, &command, false)),
    };
    result.unwrap_or_else(|err| fail(&err))
}

/// Runs config-only commands using project discovery without container mount
/// validation, allowing inspection and repair from directories such as `/`.
fn run_config_command(command: Option<&ConfigCommand>) -> anyhow::Result<ExitCode> {
    if matches!(command, Some(ConfigCommand::Default)) {
        return config::print_default();
    }
    let project_root = container::current_project_root()?;
    match command {
        None | Some(ConfigCommand::List) => load_config(&project_root)?.print_effective(),
        Some(ConfigCommand::Edit { global }) => {
            config::edit(&project_root, *global)?;
            load_validated_config(&project_root)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(ConfigCommand::Path) => config::print_paths(&project_root),
        Some(ConfigCommand::Check) => {
            load_validated_config(&project_root)?;
            config::print_valid()
        }
        Some(ConfigCommand::Default) => unreachable!("default config returned before discovery"),
    }
}

/// Loads the effective project config and emits non-fatal quick-name
/// diagnostics shared by run, inspection, checking, and post-edit validation.
fn load_config(project_root: &std::path::Path) -> anyhow::Result<config::Config> {
    let config = config::Config::load_for_project(project_root)?;
    // Unreachable quick commands do not block otherwise valid commands.
    if let Err(err) = config.check_quick_names(&builtin_commands()) {
        eprintln!("warning: {err:#}");
    }
    Ok(config)
}

/// Extends schema validation with filesystem and cross-field compatibility
/// checks shared by `config check` and post-edit validation.
fn load_validated_config(project_root: &std::path::Path) -> anyhow::Result<config::Config> {
    let config = load_config(project_root)?;
    container::validate_effective_config(&config)?;
    Ok(config)
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
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        let os: Vec<OsString> = args.iter().map(OsString::from).collect();
        Cli::try_parse_from(&os).expect("args parse").command
    }

    #[test]
    fn run_passes_through_arbitrary_command() {
        let Command::Run {
            isolated: false,
            cpus: None,
            memory: None,
            sudo: false,
            command,
        } = parse(&["silo", "run", "--", "codex", "--model", "compact"])
        else {
            panic!("expected Run");
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
        for case in [
            &["silo", "run", "codex"][..],
            &["silo", "run", "codex", "-t"][..],
            &["silo", "run", "-t"][..],
        ] {
            let os: Vec<OsString> = case.iter().map(OsString::from).collect();
            let err = Cli::try_parse_from(&os)
                .err()
                .expect("requires `--` before the command");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{case:?}: {err}"
            );
        }
    }

    #[test]
    fn run_accepts_flag_shaped_tokens_after_double_dash() {
        let Command::Run {
            isolated: false,
            cpus: None,
            memory: None,
            sudo: false,
            command,
        } = parse(&["silo", "run", "--", "-t", "--model", "compact"])
        else {
            panic!("expected Run");
        };
        assert_eq!(
            command,
            [
                OsString::from("-t"),
                OsString::from("--model"),
                OsString::from("compact"),
            ]
        );
    }

    #[test]
    fn run_without_command_keeps_the_default_shell() {
        let Command::Run {
            isolated: false,
            cpus: None,
            memory: None,
            sudo: false,
            command,
        } = parse(&["silo", "run"])
        else {
            panic!("expected Run");
        };
        assert!(command.is_empty());
    }

    #[test]
    fn run_with_double_dash_and_no_command_keeps_the_default_shell() {
        let Command::Run {
            isolated: false,
            cpus: None,
            memory: None,
            sudo: false,
            command,
        } = parse(&["silo", "run", "--"])
        else {
            panic!("expected Run");
        };
        assert!(command.is_empty());
    }

    #[test]
    fn run_preserves_double_dash_inside_the_command() {
        let Command::Run {
            isolated: false,
            cpus: None,
            memory: None,
            sudo: false,
            command,
        } = parse(&["silo", "run", "--", "codex", "--", "--flag"])
        else {
            panic!("expected Run");
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
        let Command::Quick(args) = parse(&["silo", "codex", "--model", "compact"]) else {
            panic!("expected Quick");
        };
        assert_eq!(
            args,
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
            parse(&["silo", "run", "--", "anything"]),
            Command::Run { .. }
        ));
        assert!(matches!(
            parse(&["silo", "image", "build"]),
            Command::Image { .. }
        ));
        assert!(matches!(
            parse(&["silo", "containers"]),
            Command::Containers { command: None }
        ));
        assert!(matches!(
            parse(&["silo", "containers", "list"]),
            Command::Containers {
                command: Some(ContainersCommand::List)
            }
        ));
        assert!(matches!(
            parse(&["silo", "containers", "ls"]),
            Command::Containers {
                command: Some(ContainersCommand::List)
            }
        ));
    }

    #[test]
    fn builtin_commands_covers_the_project_command_set() {
        let names = builtin_commands();
        for expected in ["config", "run", "containers", "state", "image", "help"] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn containers_stop_is_no_longer_available() {
        let err = Cli::try_parse_from(["silo", "containers", "stop", "silo-abcd"])
            .err()
            .expect("stop is not a management command");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn containers_delete_and_rm_parse_force() {
        for command in ["delete", "rm"] {
            let Command::Containers {
                command: Some(ContainersCommand::Delete { selector, force }),
            } = parse(&["silo", "containers", command, "project", "--force"])
            else {
                panic!("expected containers delete");
            };
            assert_eq!(selector, "project");
            assert!(force);
        }
    }

    #[test]
    fn state_defaults_to_list_and_parses_delete_alias() {
        assert!(matches!(
            parse(&["silo", "state"]),
            Command::State { command: None }
        ));
        assert!(matches!(
            parse(&["silo", "state", "list"]),
            Command::State {
                command: Some(StateCommand::List)
            }
        ));
        for command in ["delete", "rm"] {
            let Command::State {
                command: Some(StateCommand::Delete { selector }),
            } = parse(&["silo", "state", command, "cargo"])
            else {
                panic!("expected state delete");
            };
            assert_eq!(selector, "cargo");
        }
    }

    #[test]
    fn mounts_is_not_retained_as_a_management_alias() {
        let Command::Quick(arguments) = parse(&["silo", "mounts"]) else {
            panic!("mounts should no longer be a built-in command");
        };
        assert_eq!(arguments, ["mounts"]);
    }

    #[test]
    fn builtin_commands_excludes_the_external_quick_placeholder() {
        let names = builtin_commands();
        assert!(
            !names.iter().any(|name| name == "quick"),
            "the external quick placeholder is not a built-in command"
        );
    }

    #[test]
    fn quick_command_appends_invocation_args_to_the_configured_command() {
        let config = config::Config::parse(
            "[quick]\ncodex = [\"codex\"]\ncx = [\"codex\", \"--model\", \"compact\"]\n",
        )
        .expect("config parses");
        let args: Vec<OsString> = ["codex", "--model", "compact"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(quick_command(&config, &args).expect("resolves"), args);
        let args = vec![OsString::from("cx"), OsString::from("--flag")];
        assert_eq!(
            quick_command(&config, &args).expect("resolves"),
            [
                OsString::from("codex"),
                OsString::from("--model"),
                OsString::from("compact"),
                OsString::from("--flag"),
            ]
        );
    }

    #[test]
    fn quick_command_reports_unknown_names() {
        let config = config::Config::default();
        let err = quick_command(&config, &[OsString::from("nope")]).expect_err("unknown name");
        let msg = err.to_string();
        assert!(msg.contains("unknown quick command `nope`"), "{msg}");
        assert!(
            msg.contains("silo run --"),
            "hints at the escape hatch: {msg}"
        );
    }

    #[test]
    fn run_isolated_selects_the_ephemeral_lifecycle() {
        let Command::Run {
            isolated: true,
            cpus: None,
            memory: None,
            sudo: false,
            command,
        } = parse(&["silo", "run", "--isolated", "--", "nu", "-c", "version"])
        else {
            panic!("expected isolated Run");
        };
        assert_eq!(
            command,
            [
                OsString::from("nu"),
                OsString::from("-c"),
                OsString::from("version"),
            ]
        );
    }

    #[test]
    fn run_does_not_accept_rm_as_an_isolated_alias() {
        let err = Cli::try_parse_from(["silo", "run", "--rm"])
            .err()
            .expect("only --isolated is supported");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
