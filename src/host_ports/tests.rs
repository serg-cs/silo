use super::*;
use std::ffi::OsStr;

#[test]
fn validation_rejects_privileged_host_ports() {
    let port = validate_ports(&BTreeSet::from([80]))
        .expect_err("privileged host port fails")
        .to_string();
    assert!(port.contains("host port `80`"), "{port}");
    assert!(port.contains("between 1024 and 65535"), "{port}");
}

#[test]
fn validation_bounds_the_authorized_keys_line() {
    let maximum_count = u16::try_from(MAX_HOST_PORTS).expect("host-port limit fits u16");
    let first = u16::MAX - maximum_count + 1;
    let maximum: BTreeSet<_> = (first..=u16::MAX).collect();
    validate_ports(&maximum).expect("the maximum host-port allowlist is accepted");

    let public = format!("ssh-ed25519 {}", "A".repeat(68));
    assert!(authorized_keys(&public, &maximum).len() < 8 * 1024);

    let over_limit: BTreeSet<_> = (first - 1..=u16::MAX).collect();
    let error = validate_ports(&over_limit)
        .expect_err("an oversized host-port allowlist is rejected")
        .to_string();
    assert!(error.contains("at most 256"), "{error}");
}

#[test]
fn empty_host_ports_require_no_host_state() {
    let tunnel = prepare(&BTreeSet::new(), Path::new("/project"))
        .expect("empty host ports need no environment or filesystem state");

    assert!(tunnel.is_none());
}

#[test]
fn authorized_key_restricts_remote_listeners_to_configured_ports() {
    let ports = BTreeSet::from([5432, 6379]);

    let authorized = authorized_keys("ssh-ed25519 AAAATEST", &ports);

    assert_eq!(
        authorized,
        concat!(
            "restrict,port-forwarding,command=\"/usr/bin/false\",",
            "permitlisten=\"127.0.0.1:5432\",",
            "permitlisten=\"127.0.0.1:6379\" ",
            "ssh-ed25519 AAAATEST silo-host-ports\n"
        )
    );
}

#[test]
fn asset_identity_tracks_the_client_key_and_unique_ports() {
    let public = "ssh-ed25519 AAAATEST";

    let first = asset_key(public, &BTreeSet::from([5432, 6379]));
    let reordered = asset_key(public, &BTreeSet::from([6379, 5432]));
    let changed = asset_key(public, &BTreeSet::from([5432]));

    assert_eq!(first, reordered);
    assert_ne!(first, changed);
    assert_eq!(first.len(), DIGEST_HEX_LEN);
}

#[test]
fn ssh_arguments_disable_ambient_configuration_and_bind_ipv4_loopback() {
    let arguments = ssh_arguments(
        Path::new("/tmp/silo.sock"),
        "silo-host-ports-project-assets",
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
fn control_socket_path_leaves_room_for_opensshs_temporary_suffix() {
    const DARWIN_UNIX_PATH_MAX: usize = 104;
    const OPENSSH_TEMPORARY_SUFFIX_LEN: usize = 17;

    let root = Path::new("/tmp/silo-ssh-4294967295");
    let socket = control_socket_path(
        root,
        "a".repeat(DIGEST_HEX_LEN).as_str(),
        "b".repeat(DIGEST_HEX_LEN).as_str(),
        Ipv4Addr::BROADCAST,
    );

    assert_eq!(socket.parent(), Some(root));
    assert_eq!(socket.extension(), Some(OsStr::new("sock")));
    assert!(
        socket.as_os_str().as_encoded_bytes().len() + OPENSSH_TEMPORARY_SUFFIX_LEN
            < DARWIN_UNIX_PATH_MAX
    );
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

    assert_ne!(first, second);
}

#[test]
fn stale_control_socket_cleanup_accepts_only_owned_sockets() {
    let directory = tempfile::tempdir().expect("temporary control directory is created");
    let regular = directory.path().join("regular.sock");
    fs::write(&regular, b"not a socket").expect("regular file is created");
    assert!(remove_stale_control_socket(&regular).is_err());
    assert!(regular.exists());

    let symlink = directory.path().join("symlink.sock");
    std::os::unix::fs::symlink(&regular, &symlink).expect("socket symlink is created");
    assert!(remove_stale_control_socket(&symlink).is_err());
    assert!(fs::symlink_metadata(&symlink).is_ok());

    let socket = directory.path().join("stale.sock");
    let listener =
        std::os::unix::net::UnixListener::bind(&socket).expect("owned Unix socket is created");
    drop(listener);
    remove_stale_control_socket(&socket).expect("owned stale socket is removed");
    assert!(!socket.exists());
}

#[test]
fn public_key_normalization_accepts_ssh_keygen_comments() {
    assert_eq!(
        normalize_public_key("ssh-ed25519 AAAATEST").expect("uncommented Ed25519 key is valid"),
        "ssh-ed25519 AAAATEST"
    );
    assert_eq!(
        normalize_public_key("ssh-ed25519 AAAATEST silo-host-ports")
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

    let public = ensure_key_pair(&identity, "silo-host-ports")
        .expect("installed ssh-keygen produces a supported Ed25519 key");
    let stored_public = fs::read_to_string(identity.with_extension("pub"))
        .expect("generated public key is readable");

    assert_eq!(public.split_whitespace().count(), 2);
    assert_eq!(stored_public, format!("{public} silo-host-ports\n"));
    assert_eq!(
        ensure_key_pair(&identity, "silo-host-ports").expect("managed key can be reused"),
        public
    );
}

#[test]
fn managed_key_does_not_require_a_public_key_file() {
    let directory = tempfile::tempdir().expect("temporary key directory is created");
    let identity = directory.path().join("id_ed25519");
    let expected =
        ensure_key_pair(&identity, "silo-host-ports").expect("managed identity is generated");
    fs::remove_file(identity.with_extension("pub")).expect("public key is removed");

    let reused = ensure_key_pair(&identity, "silo-host-ports")
        .expect("public key is derived from the private key");

    assert_eq!(reused, expected);
    assert!(!identity.with_extension("pub").exists());
}

#[test]
fn managed_key_rejects_a_symlinked_private_key() {
    let directory = tempfile::tempdir().expect("temporary key directory is created");
    let real = directory.path().join("real_id_ed25519");
    ensure_key_pair(&real, "silo-host-ports").expect("real managed key is generated");
    let symlink = directory.path().join("linked_id_ed25519");
    std::os::unix::fs::symlink(&real, &symlink).expect("private-key symlink is created");

    let error = ensure_key_pair(&symlink, "silo-host-ports")
        .expect_err("a symlinked private key is rejected")
        .to_string();
    assert!(error.contains("managed SSH private key"), "{error}");
}

#[test]
fn managed_key_rejects_a_dangling_private_key_symlink() {
    let directory = tempfile::tempdir().expect("temporary key directory is created");
    let target = directory.path().join("missing_id_ed25519");
    let identity = directory.path().join("linked_id_ed25519");
    std::os::unix::fs::symlink(&target, &identity).expect("private-key symlink is created");

    let error = ensure_key_pair(&identity, "silo-host-ports")
        .expect_err("a dangling private-key symlink is rejected")
        .to_string();

    assert!(error.contains("managed SSH private key"), "{error}");
    assert!(!target.exists());
}

#[test]
fn managed_key_rejects_a_dangling_public_key_symlink() {
    let directory = tempfile::tempdir().expect("temporary key directory is created");
    let identity = directory.path().join("id_ed25519");
    let target = directory.path().join("missing_id_ed25519.pub");
    std::os::unix::fs::symlink(&target, identity.with_extension("pub"))
        .expect("public-key symlink is created");

    let error = ensure_key_pair(&identity, "silo-host-ports")
        .expect_err("a dangling public-key symlink is rejected")
        .to_string();

    assert!(error.contains("missing its private half"), "{error}");
    assert!(!identity.exists());
    assert!(!target.exists());
}

#[test]
fn concurrent_preparation_produces_one_asset_set() {
    let directory = tempfile::tempdir().expect("temporary state directory is created");
    let state_root = directory.path().join("host-ports");
    let ports = BTreeSet::from([5432, 6379]);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut handles = Vec::new();

    for _ in 0..2 {
        let barrier = barrier.clone();
        let ports = ports.clone();
        let state_root = state_root.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            prepare_at(&ports, Path::new("/project"), &state_root)
                .expect("concurrent preparation succeeds")
        }));
    }
    barrier.wait();
    let first = handles.remove(0).join().expect("first preparation joins");
    let second = handles.remove(0).join().expect("second preparation joins");

    assert_eq!(first.identity, second.identity);
    assert_eq!(first.known_hosts, second.known_hosts);
    assert_eq!(first.assets, second.assets);
    assert!(first.identity.is_file());
    assert!(first.known_hosts.is_file());
    assert!(first.assets.source.join(AUTHORIZED_KEYS_NAME).is_file());
}

#[test]
fn ssh_path_environment_preserves_config_metacharacters() {
    let directory = tempfile::Builder::new()
        .prefix("silo ssh % ${state} ")
        .tempdir()
        .expect("temporary SSH state directory is created");
    let identity = directory.path().join("identity with % and ${tokens}");
    ensure_key_pair(&identity, "silo-host-ports").expect("managed identity is generated");
    let known_hosts = directory.path().join("known hosts % ${tokens}");
    write_managed_file(&known_hosts, b"", 0o600).expect("known-hosts file is created");
    let arguments = ssh_arguments(
        Path::new("/tmp/silo.sock"),
        "silo-host-ports-test",
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
