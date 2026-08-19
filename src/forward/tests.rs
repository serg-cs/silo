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
        Path::new("/state/id_ed25519"),
        Path::new("/state/known_hosts"),
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
fn public_key_validation_accepts_only_one_ed25519_key() {
    validate_public_key("ssh-ed25519 AAAATEST").expect("Ed25519 key is valid");

    assert!(validate_public_key("ssh-rsa AAAATEST").is_err());
    assert!(validate_public_key("ssh-ed25519 AAAATEST comment").is_err());
    assert!(validate_public_key("").is_err());
}
