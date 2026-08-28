use super::*;
use crate::project::{project_digest, shared_dir_name};

fn validate_config_text(text: &str) -> Result<()> {
    validate_config_at(text, Path::new("/tmp/repo"))
}

fn validate_config_at(text: &str, project_root: &Path) -> Result<()> {
    let config = Config::parse(text)?;
    crate::container::validate_config(&config, project_root)
}

#[test]
fn validation_rejects_duplicate_names_targets_and_runtime_overlap() {
    let duplicate_name = validate_config_text(
        "[binds.same]\nsource = \"~/same\"\ntarget = \"~/one\"\naccess = \"read-only\"\n\
         [state.user.same]\ntarget = \"~/two\"\n",
    )
    .expect_err("duplicate names fail")
    .to_string();
    assert!(duplicate_name.contains("more than one"), "{duplicate_name}");

    let duplicate_target = validate_config_text(
        "[state.user.first]\ntarget = \"~/.cache\"\n\
         [state.project.second]\ntarget = \"~/.cache\"\n",
    )
    .expect_err("duplicate targets fail")
    .to_string();
    assert!(
        duplicate_target.contains("both target"),
        "{duplicate_target}"
    );

    let overlap = validate_config_text("[state.user.bad]\ntarget = \"/run/silo/data\"\n")
        .expect_err("runtime overlap fails")
        .to_string();
    assert!(overlap.contains("Silo-managed runtime path"), "{overlap}");
}

#[test]
fn validation_rejects_writable_overlays_below_read_only_workspace() {
    let dir = test_dir("read-only-overlap");
    fs::create_dir_all(dir.path().join("policy/child")).expect("policy directories create");
    fs::create_dir(dir.path().join("policy/cache")).expect("state target creates");

    let error = validate_config_at(
        "workspace.read_only = [\"policy\"]\n\
         [state.project.cache]\ntarget = \"./policy/cache\"\n",
        dir.path(),
    )
    .expect_err("writable overlay beneath protected workspace fails")
    .to_string();
    assert!(error.contains("overlaps read-only workspace"), "{error}");

    let error = validate_config_at(
        "workspace.read_only = [\"policy/child\"]\n\
         [state.project.cache]\ntarget = \"./policy\"\n",
        dir.path(),
    )
    .expect_err("writable parent of a protected workspace fails")
    .to_string();
    assert!(error.contains("overlaps read-only workspace"), "{error}");

    validate_config_at(
        "workspace.read_only = [\"policy\"]\n\
         [state.project.cache]\ntarget = \"./policy/missing\"\n",
        dir.path(),
    )
    .expect("missing project state does not overlap a read-only directory");
}

#[test]
fn validation_uses_the_actual_project_workdir() {
    let dir = test_dir("actual-project-workdir");
    fs::create_dir(dir.path().join(".git")).expect("Git directory creates");
    let project_dir = shared_dir_name(dir.path()).expect("project destination resolves");

    let overlap = validate_config_at(
        &format!(
            "[state.project.cache]\ntarget = \"{}/.git\"\n",
            project_dir.display()
        ),
        dir.path(),
    )
    .expect_err("absolute target below the actual read-only overlay fails")
    .to_string();
    assert!(
        overlap.contains("overlaps read-only workspace"),
        "{overlap}"
    );

    let duplicate = validate_config_at(
        &format!(
            "[binds.first]\nsource = \"~/first\"\ntarget = \"./cache\"\naccess = \"read-only\"\n\
             [state.user.second]\ntarget = \"{}/cache\"\n",
            project_dir.display()
        ),
        dir.path(),
    )
    .expect_err("relative and absolute targets for the actual workdir are equivalent")
    .to_string();
    assert!(duplicate.contains("both target"), "{duplicate}");

    validate_config_at(
        "[state.project.cache]\ntarget = \"/home/silo/other/.git\"\n",
        dir.path(),
    )
    .expect("a different project basename does not overlap this workspace");
}

#[test]
fn structured_mount_paths_allow_colons_and_reject_delimiters() {
    validate_config_text(
        "workspace.read_only = [\"metadata:local\"]\n\
         [binds.cache]\nsource = \"/tmp/cache:local\"\ntarget = \"./cache:local\"\naccess = \"read-only\"\n",
    )
    .expect("colons are unambiguous in structured mount arguments");

    let source = validate_config_text(
        "[binds.cache]\nsource = \"/tmp/cache,local\"\ntarget = \"./cache\"\naccess = \"read-only\"\n",
    )
    .expect_err("commas delimit structured mount options")
    .to_string();
    assert!(
        source.contains("source path contains `,` or `=`"),
        "{source}"
    );

    let target = validate_config_text("[state.user.cache]\ntarget = \"./cache=local\"\n")
        .expect_err("equals signs delimit structured mount values")
        .to_string();
    assert!(
        target.contains("target path contains `,` or `=`"),
        "{target}"
    );
}

#[test]
fn config_only_validation_from_the_filesystem_root_remains_available() {
    crate::container::validate_config(&Config::default(), Path::new("/"))
        .expect("context-independent config is valid without a project workdir");

    let runtime = validate_config_at("[state.user.bad]\ntarget = \"/run/silo\"\n", Path::new("/"))
        .expect_err("runtime overlap is rejected without a project workdir")
        .to_string();
    assert!(runtime.contains("Silo-managed runtime path"), "{runtime}");

    let duplicate = validate_config_at(
        "[state.user.first]\ntarget = \"~/.cache\"\n\
         [state.project.second]\ntarget = \"/home/silo/.cache\"\n",
        Path::new("/"),
    )
    .expect_err("resolvable duplicate targets are rejected without a project workdir")
    .to_string();
    assert!(duplicate.contains("both target"), "{duplicate}");
}

#[test]
fn read_only_resolution_stays_within_the_project() {
    let dir = test_dir("read-only-resolution");
    let outside = test_dir("read-only-outside");
    fs::create_dir(dir.path().join("policy")).expect("policy creates");
    fs::write(dir.path().join("file"), "content").expect("regular file creates");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked"))
            .expect("escaping symlink creates");
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("broken"))
            .expect("broken symlink creates");
    }

    let resolved = resolve_read_only_paths(
        dir.path(),
        &[
            PathBuf::from("policy"),
            PathBuf::from("policy/../policy"),
            PathBuf::from("missing"),
            PathBuf::from("file"),
            PathBuf::from("linked"),
            PathBuf::from("broken"),
            PathBuf::from("../escape"),
            PathBuf::from("/absolute"),
            PathBuf::from("bad,value"),
            PathBuf::new(),
        ],
    );
    assert_eq!(
        resolved,
        [read_only_path(
            canonical(&dir.path().join("policy")).to_str().unwrap(),
            "policy"
        )]
    );
}

#[test]
fn configured_mounts_encode_owner_and_sort_parents_before_children() {
    let project = test_dir("configured-mounts");
    fs::create_dir_all(project.path().join("cache/child")).expect("state target creates");
    let mut config = Config::default();
    config
        .state
        .project
        .insert("child".into(), state_entry("./cache/child"));
    config
        .state
        .user
        .insert("parent".into(), state_entry("./cache"));
    let project_dir = shared_dir_name(project.path()).expect("project destination resolves");
    let mounts = resolve_configured_mounts(
        &config,
        project.path(),
        &[],
        Some(Path::new("/home/user")),
        None,
    )
    .expect("mounts resolve");

    assert_eq!(mounts[0].dest, project_dir.join("cache"));
    assert_eq!(mounts[1].dest, project_dir.join("cache/child"));
    let MountSource::Managed(parent) = &mounts[0].source else {
        panic!("state mount is managed");
    };
    let MountSource::Managed(child) = &mounts[1].source else {
        panic!("state mount is managed");
    };
    assert_eq!(parent.owner, StateOwner::User);
    assert_eq!(child.owner, StateOwner::Project(project.path().into()));
    assert!(
        child
            .path
            .to_string_lossy()
            .contains(&project_digest(project.path()))
    );
}

#[test]
fn missing_project_relative_state_is_ignored() {
    let project = test_dir("missing-project-state");
    let target = project.path().join("target");
    let mut config = Config::default();
    config
        .state
        .project
        .insert("cargo-target".into(), state_entry("./target"));

    let mounts = resolve_configured_mounts(&config, project.path(), &[], None, None)
        .expect("missing project state is ignored without managed storage");
    assert!(mounts.is_empty());
    ensure_managed_mounts(&mounts).expect("no managed state needs creation");
    assert!(!target.exists());

    fs::create_dir(&target).expect("state target creates");
    let state_home = project.path().join("state-home");
    let mounts = resolve_configured_mounts(&config, project.path(), &[], None, Some(&state_home))
        .expect("existing project state resolves");
    assert_eq!(mounts.len(), 1);
    assert_eq!(
        mounts[0].dest,
        shared_dir_name(project.path())
            .expect("project destination resolves")
            .join("target")
    );
}

#[test]
fn project_state_eligibility_is_stable_during_resolution() {
    let project = test_dir("project-state-snapshot");
    let target = project.path().join("target");
    let project_dir = shared_dir_name(project.path()).expect("project destination resolves");
    let mut config = Config::default();
    config
        .state
        .project
        .insert("project-cache".into(), state_entry("./target"));
    config
        .state
        .user
        .insert("user-cache".into(), state_entry("./target"));

    let project_state = eligible_project_state(&config, project.path());
    assert!(project_state.is_empty());

    fs::create_dir(target).expect("target appears after eligibility snapshot");
    validate_eligible_project_targets(&config, &project_state, &project_dir, &[])
        .expect("the unchanged snapshot has no duplicate target");
    let names: Vec<_> = configured_mounts(&config, Some(&project_state))
        .map(|(name, _, _)| name)
        .collect();
    assert_eq!(names, ["user-cache"]);
}

#[test]
fn project_filesystem_validation_checks_bind_sources() {
    let project = test_dir("bind-source-project");
    let source = test_dir("bind-source");
    let mut config = Config::default();
    config.binds.insert(
        "docs".into(),
        Bind {
            source: source.path().to_path_buf(),
            target: PathBuf::from("~/docs"),
            access: Permission::ReadOnly,
        },
    );

    crate::container::validate_project_filesystem(&config, project.path())
        .expect("existing directory source is valid");

    config.binds.get_mut("docs").unwrap().source = project.path().join("missing");
    let missing = crate::container::validate_project_filesystem(&config, project.path())
        .expect_err("missing bind source fails")
        .to_string();
    assert!(missing.contains("bind `docs`"), "{missing}");
    assert!(missing.contains("missing"), "{missing}");

    let file = project.path().join("file");
    fs::write(&file, "content").expect("fixture file creates");
    config.binds.get_mut("docs").unwrap().source = file;
    let regular_file = crate::container::validate_project_filesystem(&config, project.path())
        .expect_err("regular file bind source fails")
        .to_string();
    assert!(
        regular_file.contains("must be a directory"),
        "{regular_file}"
    );
}

#[test]
fn managed_state_resolution_does_not_create_storage() {
    let project = test_dir("state-resolution-project");
    let state_home = project.path().join("state-home");
    let mut config = Config::default();
    config.state.user.insert(
        "cache".into(),
        StateEntry {
            target: PathBuf::from("~/.cache"),
        },
    );

    resolve_configured_mounts(&config, project.path(), &[], None, Some(&state_home))
        .expect("managed state paths resolve without creation");
    assert!(!state_home.exists());
}

#[test]
fn tilde_expansion_requires_a_known_home() {
    assert_eq!(
        expand_tilde(Path::new("~/.cache"), Some(Path::new("/home/user"))),
        Path::new("/home/user/.cache")
    );
    assert_eq!(
        expand_tilde(Path::new("~/.cache"), None),
        Path::new("~/.cache")
    );
    assert_eq!(
        expand_tilde(Path::new("~other/cache"), Some(Path::new("/home/user"))),
        Path::new("~other/cache")
    );
}
