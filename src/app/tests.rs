use super::*;

mod config_command;

#[test]
fn builtin_commands_covers_the_project_command_set() {
    let names = builtin_commands();
    for expected in [
        "config",
        "run",
        "start",
        "stop",
        "containers",
        "state",
        "image",
        "help",
    ] {
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
    let error = quick_command(&config, &[OsString::from("nope")])
        .expect_err("unknown name")
        .to_string();
    assert!(error.contains("unknown quick command `nope`"), "{error}");
    assert!(
        error.contains("silo run --"),
        "hints at the escape hatch: {error}"
    );
}

#[test]
fn quick_names_reject_builtin_commands() {
    let config = config::Config::parse("[quick]\nrun = [\"bash\"]\n").expect("config parses");
    let message = validate_quick_commands(&config)
        .expect_err("built-in command shadows quick name")
        .to_string();

    assert!(message.contains("`run`"), "{message}");
    assert!(message.contains("shadowed"), "{message}");
}

#[test]
fn quick_command_validation_fails_fast() {
    let config = config::Config::parse(
        "[quick]\nrun = [\"bash\"]\nimage = [\"bash\"]\ncodex = [\"codex\"]\n",
    )
    .expect("config parses");
    let message = validate_quick_commands(&config)
        .expect_err("shadowed name fails")
        .to_string();

    assert!(message.contains("`image`"), "{message}");
    assert!(!message.contains("`run`"), "{message}");
    assert!(!message.contains("`codex`"), "{message}");
}

#[test]
fn quick_names_accept_non_conflicting_names() {
    let config = config::Config::parse("[quick]\ncodex = [\"codex\"]\n").expect("config parses");

    validate_quick_commands(&config).expect("quick name is reachable");
}

#[test]
fn quick_names_reject_flag_shaped_names() {
    let config = config::Config::parse("[quick]\n\"-h\" = [\"codex\"]\n").expect("config parses");
    let message = validate_quick_commands(&config)
        .expect_err("flag-shaped name is unreachable")
        .to_string();

    assert!(message.contains("`-h`"), "{message}");
    assert!(message.contains("starts with `-`"), "{message}");
}

#[test]
fn quick_commands_require_reachable_names_and_executables() {
    for (text, expected) in [
        ("[quick]\n\"\" = [\"codex\"]\n", "name"),
        ("[quick]\n\"   \" = [\"codex\"]\n", "whitespace"),
        ("[quick]\ncodex = []\n", "array is empty"),
        ("[quick]\ncodex = [\"   \"]\n", "executable"),
    ] {
        let config = config::Config::parse(text).expect("quick command parses");
        let message = validate_quick_commands(&config)
            .expect_err("unreachable quick command fails")
            .to_string();
        assert!(message.contains(expected), "{message}");
    }

    let config = config::Config::parse("[quick]\ncodex = [\"codex\", \"\"]\n")
        .expect("quick command parses");
    validate_quick_commands(&config).expect("empty later arguments remain valid");
}

#[test]
fn full_config_validation_requires_a_runnable_project_root() {
    let error = validate_check_project_root(std::path::Path::new("/"))
        .expect_err("the filesystem root is not a Silo workspace")
        .to_string();
    assert!(
        error.contains("cannot check configuration from `/`"),
        "{error}"
    );
    assert!(error.contains("project directory"), "{error}");
}

#[test]
fn full_config_validation_accepts_defaults_without_a_project_file() {
    let project = crate::test_support::test_dir("global-only-check");
    validate_check_project_root(project.path()).expect("ordinary directory is a project");
    validate_config(
        &config::Config::default(),
        project.path(),
        ValidationProfile::Check,
    )
    .expect("defaults are valid without a project config file");
}
