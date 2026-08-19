use super::*;

fn forward(port: u16, enabled: bool) -> Forward {
    Forward {
        port,
        enabled: Some(enabled),
    }
}

#[test]
fn enabled_forward_selection_and_environment_are_stable() {
    let forwards = BTreeMap::from([
        ("redis".to_string(), forward(6379, false)),
        ("postgres".to_string(), forward(5432, true)),
    ]);

    let enabled = enabled_forwards(&forwards);

    assert_eq!(enabled.keys().collect::<Vec<_>>(), ["postgres"]);
    assert_eq!(
        forward_environment(&enabled),
        [("SILO_POSTGRES_PORT".to_string(), "5432".to_string())]
    );
}

#[test]
fn authorized_key_restricts_remote_listeners_to_enabled_ports() {
    let ports = BTreeSet::from([5432, 6379]);

    let authorized = authorized_keys("ssh-ed25519 AAAATEST", &ports);

    assert!(authorized.contains("command=\"/usr/bin/false\""));
    assert!(authorized.contains("no-agent-forwarding"));
    assert!(authorized.contains("no-X11-forwarding"));
    assert!(authorized.contains("no-pty"));
    assert!(authorized.contains("no-user-rc"));
    assert!(authorized.contains("permitlisten=\"127.0.0.1:5432\""));
    assert!(authorized.contains("permitlisten=\"127.0.0.1:6379\""));
    assert!(authorized.ends_with("ssh-ed25519 AAAATEST silo-forward\n"));
}

#[test]
fn asset_identity_tracks_project_key_and_unique_ports() {
    let root = Path::new("/tmp/project");
    let public = "ssh-ed25519 AAAATEST";

    let first = asset_key(root, public, &BTreeSet::from([5432, 6379]));
    let reordered = asset_key(root, public, &BTreeSet::from([6379, 5432]));
    let changed = asset_key(root, public, &BTreeSet::from([5432]));

    assert_eq!(first, reordered);
    assert_ne!(first, changed);
    assert_eq!(first.len(), DIGEST_HEX_LEN);
}

#[test]
fn ssh_arguments_disable_ambient_configuration_and_bind_ipv4_loopback() {
    let arguments = ssh_arguments(
        Path::new("/tmp/silo.sock"),
        "silo-forward-project-assets",
        &BTreeSet::from([5432, 6379]),
        Ipv4Addr::new(192, 168, 64, 2),
    );
    let arguments: Vec<_> = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();

    assert!(arguments.windows(2).any(|pair| pair == ["-F", "/dev/null"]));
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["-S", "/tmp/silo.sock"])
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| { pair == ["-R", "127.0.0.1:5432:127.0.0.1:5432"] })
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| { pair == ["-R", "127.0.0.1:6379:127.0.0.1:6379"] })
    );
    assert!(arguments.contains(&"StrictHostKeyChecking=yes".to_string()));
    assert!(arguments.contains(&"GlobalKnownHostsFile=/dev/null".to_string()));
    assert!(arguments.contains(&"IdentityAgent=none".to_string()));
    assert!(arguments.contains(&format!("IdentityFile=${{{SSH_IDENTITY_ENV}}}")));
    assert!(arguments.contains(&format!("UserKnownHostsFile=${{{SSH_KNOWN_HOSTS_ENV}}}")));
    assert_eq!(
        arguments.last().map(String::as_str),
        Some("silo@192.168.64.2")
    );
}

#[test]
fn control_socket_validation_requires_the_private_root_and_short_name() {
    let root = Path::new("/tmp/silo-ssh-501");

    validate_control_socket_path(Path::new("/tmp/silo-ssh-501/project.sock"), root)
        .expect("owned socket path is accepted");
    assert!(validate_control_socket_path(Path::new("/tmp/project.sock"), root).is_err());
    assert!(validate_control_socket_path(Path::new("project.sock"), root).is_err());
    assert!(validate_control_socket_path(Path::new("/tmp/silo-ssh-501/project"), root).is_err());
}

#[test]
fn control_socket_identity_includes_the_container_address() {
    let root = Path::new("/tmp/silo-ssh-501");
    let first = control_socket_path(
        root,
        "aaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbb",
        Ipv4Addr::new(192, 168, 64, 2),
    );
    let second = control_socket_path(
        root,
        "aaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbb",
        Ipv4Addr::new(192, 168, 64, 3),
    );

    validate_control_socket_path(&first, root).expect("bounded socket path is accepted");
    assert_ne!(first, second);
}

#[test]
fn public_key_normalization_accepts_ssh_keygen_comments() {
    assert_eq!(
        normalize_public_key("ssh-ed25519 AAAATEST").expect("uncommented Ed25519 key is valid"),
        "ssh-ed25519 AAAATEST"
    );
    assert_eq!(
        normalize_public_key("ssh-ed25519 AAAATEST silo-forward")
            .expect("commented Ed25519 key is valid"),
        "ssh-ed25519 AAAATEST"
    );
    assert_eq!(
        normalize_public_key("ssh-ed25519 AAAATEST comment with spaces")
            .expect("an SSH key comment may contain spaces"),
        "ssh-ed25519 AAAATEST"
    );
}

#[test]
fn public_key_normalization_rejects_invalid_identity_output() {
    assert!(normalize_public_key("ssh-rsa AAAATEST").is_err());
    assert!(normalize_public_key("ssh-ed25519").is_err());
    assert!(normalize_public_key("ssh-ed25519 AAAATEST\nssh-ed25519 AAAAOTHER").is_err());
    assert!(normalize_public_key("").is_err());
}

#[test]
fn managed_key_generation_accepts_installed_ssh_keygen_output() {
    let directory = tempfile::tempdir().expect("temporary key directory is created");
    let identity = directory.path().join("id_ed25519");

    let public = ensure_key_pair(&identity, "silo-forward")
        .expect("installed ssh-keygen produces a supported Ed25519 key");
    let stored_public = fs::read_to_string(identity.with_extension("pub"))
        .expect("generated public key is readable");

    assert_eq!(public.split_whitespace().count(), 2);
    assert_eq!(stored_public, format!("{public} silo-forward\n"));
    assert_eq!(
        ensure_key_pair(&identity, "silo-forward").expect("managed key can be reused"),
        public
    );
}

#[test]
fn ssh_path_environment_preserves_config_metacharacters() {
    let directory = tempfile::Builder::new()
        .prefix("silo ssh % ${state} ")
        .tempdir()
        .expect("temporary SSH state directory is created");
    let identity = directory.path().join("identity with % and ${tokens}");
    ensure_key_pair(&identity, "silo-forward").expect("managed identity is generated");
    let known_hosts = directory.path().join("known hosts % ${tokens}");
    write_managed_file(&known_hosts, b"", 0o600).expect("known-hosts file is created");
    let arguments = ssh_arguments(
        Path::new("/tmp/silo.sock"),
        "silo-forward-test",
        &BTreeSet::from([8000]),
        Ipv4Addr::new(192, 0, 2, 1),
    );

    // Ask the installed client to parse the same options without connecting.
    let mut command = Command::new(SSH_BIN);
    command.arg("-G").args(arguments);
    set_ssh_path_environment(&mut command, &identity, &known_hosts);
    assert!(command.get_envs().any(|(name, value)| {
        name == OsStr::new(SSH_IDENTITY_ENV) && value == Some(identity.as_os_str())
    }));
    assert!(command.get_envs().any(|(name, value)| {
        name == OsStr::new(SSH_KNOWN_HOSTS_ENV) && value == Some(known_hosts.as_os_str())
    }));
    let output = command.output().expect("installed SSH client starts");
    let configuration = String::from_utf8(output.stdout).expect("SSH configuration is UTF-8");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        configuration
            .lines()
            .any(|line| { line == format!("identityfile ${{{SSH_IDENTITY_ENV}}}") })
    );
    assert!(
        configuration
            .lines()
            .any(|line| { line.strip_prefix("userknownhostsfile ") == known_hosts.to_str() })
    );
}
