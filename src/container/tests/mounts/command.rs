use super::*;

#[test]
fn creation_uses_structured_mounts_in_explicit_order() {
    let command = isolated_create_command(
        &ConfigMounts {
            read_only: vec![read_only_path("/tmp/project/.git", ".git")],
            configured: vec![configured_host(
                "/tmp/cache:shared",
                "/home/silo/project/cache:shared",
                Permission::ReadWrite,
            )],
            host_ports: None,
        },
        &Container::default(),
        &[],
        Shell::Zsh,
    )
    .expect("command builds");

    assert_eq!(
        mount_specs(&command),
        [
            "type=bind,source=/tmp/project,target=/home/silo/project",
            "type=bind,source=/tmp/project/.git,target=/home/silo/project/.git,readonly",
            "type=bind,source=/tmp/cache:shared,target=/home/silo/project/cache:shared",
        ]
    );
    assert!(!args_without_labels(&command).contains(&"-v"));
}

#[test]
fn isolated_command_applies_runtime_contract_and_user_command() {
    let command = isolated_create_command(
        &ConfigMounts::default(),
        &Container {
            cpus: Some(4),
            memory: Some("8G".into()),
            sudo: true,
        },
        &[OsString::from("codex"), OsString::from("--quiet")],
        Shell::Fish,
    )
    .expect("command builds");
    let args = args_without_labels(&command);

    assert!(
        args.windows(4)
            .any(|args| args == ["--cpus", "4", "--memory", "8G"])
    );
    assert!(args.contains(&"SILO_SUDO=1"));
    assert!(args.contains(&"SHELL=/home/linuxbrew/.linuxbrew/bin/fish"));
    assert!(args.ends_with(&[TEST_IMAGE, "codex", "--quiet"]));
}

#[test]
fn managed_mounts_use_structured_sources_and_access() {
    let project = Path::new("/tmp/project");
    let writable = test_managed_mount(StateOwner::Project(project.into()), "cargo");
    let readonly = test_managed_mount(StateOwner::User, "codex");
    let command = create_command(
        &test_project("/tmp/project"),
        &HostIds {
            uid: "501".into(),
            gid: "20".into(),
        },
        &ConfigMounts {
            read_only: Vec::new(),
            configured: vec![
                ConfiguredMount {
                    source: MountSource::Managed(writable),
                    dest: "/home/silo/.cargo".into(),
                    access: Permission::ReadWrite,
                },
                ConfiguredMount {
                    source: MountSource::Managed(readonly),
                    dest: "/home/silo/.codex".into(),
                    access: Permission::ReadOnly,
                },
            ],
            host_ports: None,
        },
        &Container::default(),
        Path::new("/tmp/container.cid"),
    )
    .expect("command builds");
    let specs = mount_specs(&command);

    assert_eq!(specs.len(), 3);
    assert_eq!(
        specs[0],
        "type=bind,source=/tmp/project,target=/home/silo/project"
    );
    assert!(specs[1].contains("target=/home/silo/.cargo"));
    assert!(specs[2].ends_with("target=/home/silo/.codex,readonly"));
}
