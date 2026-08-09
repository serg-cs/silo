use clap::{CommandFactory, Parser, Subcommand};
use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::anyhow;

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
    /// Stop and delete the shared container for the current project.
    Stop,
    /// Run a configured quick command: `silo <name> [args...]`, where `name`
    /// is a key in the `[quick]` section of the config file; extra arguments
    /// are appended.
    #[command(external_subcommand)]
    Quick(Vec<OsString>),
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
    // A quick command whose name collides with a built-in command could
    // never run (the built-in wins), and a flag-shaped name is unreachable.
    // Such names do not block other commands; report them and continue.
    if let Err(err) = config.check_quick_names(&builtin_commands()) {
        eprintln!("warning: {err:#}");
    }
    let result = match cli.command {
        Command::Image {
            command: ImageCommand::Build,
        } => container::build_image(&config),
        Command::Run { isolated, command } => container::run_image(&config, &command, isolated),
        Command::Stop => container::stop_image(),
        Command::Quick(args) => quick_command(&config, &args)
            .and_then(|command| container::run_image(&config, &command, false)),
    };
    result.unwrap_or_else(|err| fail(&err))
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
        assert!(matches!(parse(&["silo", "stop"]), Command::Stop));
    }

    #[test]
    fn builtin_commands_covers_the_project_command_set() {
        let names = builtin_commands();
        for expected in ["run", "stop", "image", "help"] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
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
