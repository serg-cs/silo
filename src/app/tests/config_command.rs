use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use tempfile::tempdir;

use crate::app::config_command::*;

#[test]
fn active_paths_follow_precedence_order() {
    let dir = tempdir().expect("temporary directory exists");
    let xdg = dir.path().join("xdg");
    let global = xdg.join("silo/config.toml");
    fs::create_dir_all(global.parent().expect("global has parent"))
        .expect("global directory creation succeeds");
    fs::write(&global, "").expect("global config creation succeeds");
    let project = dir.path().join("project");
    fs::create_dir_all(&project).expect("project directory creation succeeds");
    let project_config = project.join(".silo.toml");
    fs::write(&project_config, "").expect("project config creation succeeds");

    let paths = active_config_paths_from(&project, Some(xdg.as_os_str()), None);
    assert_eq!(
        paths,
        [
            ("global", global.clone()),
            ("project", project_config.clone())
        ]
    );
    assert_eq!(
        config_paths_text(&paths),
        format!(
            "global\t{}\nproject\t{}\n",
            global.display(),
            project_config.display()
        )
    );
}

#[test]
fn missing_active_paths_report_builtin_defaults_successfully() {
    let dir = tempdir().expect("temporary directory exists");
    let paths = active_config_paths_from(dir.path(), Some(dir.path().as_os_str()), None);
    assert_eq!(config_paths_text(&paths), "built-in\t<embedded defaults>\n");
}

#[test]
fn effective_config_uses_the_existing_schema_when_serialized_as_json() {
    let config = crate::config::Config::parse(
        "[container]\ncpus = 4\nsudo = true\n[quick]\ntest = [\"cargo\", \"test\"]\n",
    )
    .expect("config parses");
    let value = serde_json::to_value(config).expect("effective config serializes as JSON");

    assert_eq!(value["container"]["cpus"], 4);
    assert_eq!(value["container"]["sudo"], true);
    assert_eq!(value["quick"]["test"], serde_json::json!(["cargo", "test"]));
    assert!(value.is_object());
}

#[test]
fn edit_path_prefers_project_unless_global_is_requested() {
    let dir = tempdir().expect("temporary directory exists");
    let xdg = dir.path().join("xdg");
    let project_config = dir.path().join(".silo.toml");
    fs::write(&project_config, "").expect("project config creation succeeds");

    assert_eq!(
        edit_path_from(dir.path(), false, Some(xdg.as_os_str()), None)
            .expect("project edit path resolves"),
        project_config
    );
    assert_eq!(
        edit_path_from(dir.path(), true, Some(xdg.as_os_str()), None)
            .expect("global edit path resolves"),
        xdg.join("silo/config.toml")
    );
}

#[test]
fn edit_path_requires_a_home_location() {
    let dir = tempdir().expect("temporary directory exists");
    let home = dir.path().join("home");
    assert_eq!(
        edit_path_from(dir.path(), false, None, Some(home.as_os_str()))
            .expect("global fallback resolves"),
        home.join(".config/silo/config.toml")
    );
    let message = edit_path_from(dir.path(), false, None, None)
        .expect_err("missing home is reported")
        .to_string();
    assert!(message.contains("XDG_CONFIG_HOME or HOME"), "{message}");
}

#[test]
fn editor_selection_uses_visual_then_editor_then_vi() {
    assert_eq!(
        selected_editor(Some(OsStr::new("nvim")), Some(OsStr::new("nano"))),
        OsStr::new("nvim")
    );
    assert_eq!(
        selected_editor(Some(OsStr::new("")), Some(OsStr::new("nano"))),
        OsStr::new("nano")
    );
    assert_eq!(selected_editor(None, None), OsStr::new("vi"));
}

#[test]
fn editor_keeps_the_config_path_as_one_argument() {
    let dir = tempdir().expect("temporary directory exists");
    let script = dir.path().join("fake editor.sh");
    let output = dir.path().join("editor-arguments.txt");
    let config = dir.path().join("config file.toml");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            output.display()
        ),
    )
    .expect("editor script creation succeeds");
    let editor = format!("/bin/sh '{}' --wait", script.display());

    run_editor(&config, None, Some(OsStr::new(&editor))).expect("fake editor succeeds");

    let arguments = fs::read_to_string(output).expect("editor arguments are recorded");
    assert_eq!(arguments, format!("--wait\n{}\n", config.display()));
}

#[test]
fn editor_failure_is_reported() {
    let message = run_editor(
        Path::new("/tmp/config.toml"),
        Some(OsStr::new("false")),
        None,
    )
    .expect_err("failed editor is reported")
    .to_string();
    assert!(message.contains("exited with status"), "{message}");
}
