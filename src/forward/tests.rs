use super::*;

#[test]
fn versions_compare_numerically() {
    assert!(version_at_least("1.2.1", (1, 2, 1)));
    assert!(version_at_least("1.10.0", (1, 2, 1)));
    assert!(version_at_least("2.0.0-dev", (1, 2, 1)));
    assert!(!version_at_least("1.2.0", (1, 2, 1)));
}

#[test]
fn project_network_names_are_stable_and_path_specific() {
    let first = network_name(Path::new("/tmp/one"));
    assert_eq!(first, network_name(Path::new("/tmp/one")));
    assert_ne!(first, network_name(Path::new("/tmp/two")));
    assert!(first.starts_with(NETWORK_PREFIX));
}

#[test]
fn only_canonical_silo_network_names_are_sweep_candidates() {
    assert!(valid_network_name("silo-net-0123456789abcdef01234567"));
    assert!(!valid_network_name("default"));
    assert!(!valid_network_name("silo-net-0123456789abcdef0123456g"));
    assert!(!valid_network_name("silo-net-0123456789abcdef012345678"));
}

#[test]
fn internal_broker_path_is_confined_to_private_state() {
    let path = broker_socket_path().expect("broker path resolves");
    let first_lock = project_lock_path(Path::new("/tmp/one")).expect("project lock resolves");
    let second_lock = project_lock_path(Path::new("/tmp/two")).expect("project lock resolves");

    validate_broker_socket_path(&path).expect("generated broker path validates");
    assert!(path.as_os_str().as_encoded_bytes().len() < DARWIN_UNIX_PATH_MAX);
    assert_ne!(first_lock, second_lock);
    assert!(validate_broker_socket_path(Path::new("/tmp/ssh-agent.sock")).is_err());
    assert!(valid_broker_socket_name(BROKER_SOCKET_NAME));
    assert!(!valid_broker_socket_name("ssh-agent.sock"));
    assert!(!valid_broker_socket_name("0123456789abcdef0123456g.sock"));
}

#[test]
fn cleanup_cannot_cross_an_active_setup_barrier() {
    let directory = tempfile::tempdir().expect("temporary directory exists");
    let path = directory.path().join("broker.lock");
    let setup = StartupLock::acquire(&path).expect("setup barrier locks");

    assert!(
        StartupLock::try_acquire(&path)
            .expect("cleanup barrier is checked")
            .is_none()
    );

    drop(setup);
    assert!(
        StartupLock::try_acquire(&path)
            .expect("released cleanup barrier locks")
            .is_some()
    );
}

#[test]
fn unsupported_network_inspection_disables_best_effort_cleanup() {
    assert!(!network_may_need_cleanup("silo-net-test", |_| {
        Err(anyhow!("network commands are unsupported"))
    }));
    assert!(!network_may_need_cleanup("silo-net-test", |_| Ok(None)));
    assert!(network_may_need_cleanup("silo-net-test", |_| {
        Ok(Some(Value::Null))
    }));
}

#[test]
fn a_live_legacy_broker_retains_network_cleanup_ownership() {
    let directory = tempfile::tempdir().expect("temporary directory exists");
    let path = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&path).expect("broker socket binds");

    assert!(broker_is_running(&path));

    drop(listener);
    assert!(!broker_is_running(&directory.path().join("missing.sock")));
}

#[test]
fn broker_process_uses_a_separate_process_group() {
    let mut command = Command::new("sh");
    command
        .args(["-c", "sleep 30"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    isolate_broker_process_group(&mut command);
    let mut child = command.spawn().expect("detached child starts");
    let child_pid = i32::try_from(child.id()).expect("child PID fits pid_t");
    let child_group = unsafe { libc::getpgid(child_pid) };
    let parent_group = unsafe { libc::getpgrp() };

    let _ = child.kill();
    child.wait().expect("detached child is reaped");

    assert_eq!(child_group, child_pid);
    assert_ne!(child_group, parent_group);
}

#[test]
fn relay_capacity_applies_one_broker_wide_limit() {
    let capacity = Arc::new(RelayCapacity::default());
    let permits: Vec<_> = (0..MAX_RELAY_CONNECTIONS)
        .map(|_| capacity.try_acquire().expect("capacity remains"))
        .collect();

    assert!(capacity.try_acquire().is_none());

    drop(permits);
    assert!(capacity.try_acquire().is_some());
}

#[test]
fn network_inspection_requires_ownership_and_gateway() {
    let root = Path::new("/tmp/project");
    let project = hex_digest(Sha256::digest(root.as_os_str().as_encoded_bytes()));
    let value = serde_json::json!({
        "configuration": {
            "labels": {
                LABEL_OWNER: LABEL_OWNER_VALUE,
                LABEL_SCHEMA: LABEL_SCHEMA_VALUE,
                LABEL_PROJECT: project,
                LABEL_PROJECT_ROOT: "/tmp/project"
            }
        },
        "status": {
            "ipv4Subnet": "192.168.64.0/24",
            "ipv4Gateway": "192.168.64.1"
        }
    });
    let network = validate_network(&value, "silo-net-test", root).expect("network validates");
    assert_eq!(network.gateway, Ipv4Addr::new(192, 168, 64, 1));
    assert_eq!(network.subnet, subnet("192.168.64.0/24"));

    let mut foreign = value;
    foreign["configuration"]["labels"][LABEL_OWNER] = Value::String("other".into());
    assert!(validate_network(&foreign, "silo-net-test", root).is_err());
}

#[test]
fn ipv4_subnets_are_normalized_and_match_only_members() {
    let subnet = subnet("192.168.64.9/24");

    assert_eq!(subnet.to_string(), "192.168.64.0/24");
    assert!(subnet.contains(Ipv4Addr::new(192, 168, 64, 2)));
    assert!(!subnet.contains(Ipv4Addr::new(192, 168, 65, 2)));
    assert!("192.168.64.0/33".parse::<Ipv4Subnet>().is_err());
}

#[test]
fn relay_sources_are_limited_to_vmnet_subnets() {
    let sources = Arc::new(Mutex::new(BTreeSet::from([subnet("192.168.64.0/24")])));

    assert!(source_is_allowed(
        "192.168.64.2:50000".parse().expect("peer address parses"),
        &sources
    ));
    assert!(!source_is_allowed(
        "192.168.65.2:50000".parse().expect("peer address parses"),
        &sources
    ));
    assert!(!source_is_allowed(
        "[::1]:50000".parse().expect("peer address parses"),
        &sources
    ));
}

#[test]
fn enabled_forward_selection_respects_default_true() {
    let forwards = BTreeMap::from([
        (
            "postgres".to_string(),
            Forward {
                port: 5432,
                enabled: None,
            },
        ),
        (
            "redis".to_string(),
            Forward {
                port: 6379,
                enabled: Some(false),
            },
        ),
    ]);
    let selected = enabled_forwards(&forwards);
    assert_eq!(
        selected.keys().map(String::as_str).collect::<Vec<_>>(),
        ["postgres"]
    );
}

#[test]
fn duplicate_port_aliases_release_one_listener_reference() {
    let gateway = Ipv4Addr::new(192, 168, 64, 1);
    let subnet = subnet("192.168.64.0/24");
    let root = Path::new("/tmp/project");
    let network = network_name(root);
    let cancelled = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(BrokerState {
        networks: HashMap::from([(
            network.clone(),
            NetworkLease {
                network: ProjectNetwork {
                    name: network.clone(),
                    project_root: root.to_path_buf(),
                    subnet,
                    gateway,
                },
                users: 1,
                active_ports: BTreeMap::from([(5432, 1)]),
                desired: BTreeSet::from([5432]),
            },
        )]),
        listeners: HashMap::from([(
            5432,
            ListenerLease {
                users: 1,
                exposed_port: 15432,
                cancelled: Arc::clone(&cancelled),
                sources: Arc::new(Mutex::new(BTreeSet::from([subnet]))),
            },
        )]),
        relay_capacity: Arc::new(RelayCapacity::default()),
    }));
    let request = LeaseRequest {
        network: network.clone(),
        project_root: root.to_path_buf(),
        subnet,
        gateway,
        ports: vec![5432, 5432],
        cleanup_only: false,
    };

    release_ports(&state, &request);

    {
        let state = state.lock().expect("state locks");
        assert_eq!(state.listeners[&5432].users, 0);
        assert_eq!(state.networks[&network].users, 0);
    }
    assert!(!cancelled.load(Ordering::Relaxed));

    cancel_listeners(&state);

    assert!(state.lock().expect("state locks").listeners.is_empty());
    assert!(cancelled.load(Ordering::Relaxed));
}

#[test]
fn failed_acquisition_preserves_a_preexisting_idle_listener() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut state = BrokerState {
        networks: HashMap::new(),
        listeners: HashMap::from([(
            5432,
            ListenerLease {
                users: 1,
                exposed_port: 15432,
                cancelled: Arc::clone(&cancelled),
                sources: Arc::new(Mutex::new(BTreeSet::new())),
            },
        )]),
        relay_capacity: Arc::new(RelayCapacity::default()),
    };
    let acquired = [AcquiredListener {
        port: 5432,
        created: false,
    }];

    rollback_ports(&mut state, &acquired);

    assert_eq!(state.listeners[&5432].users, 0);
    assert!(!cancelled.load(Ordering::Relaxed));
}

#[test]
fn newer_forward_set_removes_undesired_idle_listeners() {
    let gateway = Ipv4Addr::new(192, 168, 64, 1);
    let subnet = subnet("192.168.64.0/24");
    let root = Path::new("/tmp/project");
    let network = network_name(root);
    let postgres_cancelled = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(BrokerState {
        networks: HashMap::from([(
            network.clone(),
            NetworkLease {
                network: ProjectNetwork {
                    name: network.clone(),
                    project_root: root.to_path_buf(),
                    subnet,
                    gateway,
                },
                users: 0,
                active_ports: BTreeMap::new(),
                desired: BTreeSet::from([5432]),
            },
        )]),
        listeners: HashMap::from([(
            5432,
            ListenerLease {
                users: 0,
                exposed_port: 15432,
                cancelled: Arc::clone(&postgres_cancelled),
                sources: Arc::new(Mutex::new(BTreeSet::from([subnet]))),
            },
        )]),
        relay_capacity: Arc::new(RelayCapacity::default()),
    }));
    let request = lease_request(root, gateway, vec![6379]);

    acquire_ports(&state, &request).expect("new forward set acquires");

    let state = state.lock().expect("state locks");
    assert!(!state.listeners.contains_key(&5432));
    assert!(state.listeners.contains_key(&6379));
    assert!(postgres_cancelled.load(Ordering::Relaxed));
}

#[test]
fn overlapping_leases_preserve_each_forward_until_release() {
    let root = Path::new("/tmp/overlapping");
    let gateway = Ipv4Addr::LOCALHOST;
    let first = lease_request(root, gateway, vec![5432, 8000]);
    let second = lease_request(root, gateway, vec![6379, 8000]);
    let state = Arc::new(Mutex::new(BrokerState::default()));

    acquire_ports(&state, &first).expect("first lease acquires");
    acquire_ports(&state, &second).expect("second lease acquires");

    {
        let state = state.lock().expect("state locks");
        let network = &state.networks[&first.network];
        assert_eq!(network.desired, BTreeSet::from([5432, 6379, 8000]));
        assert_eq!(
            network.active_ports,
            BTreeMap::from([(5432, 1), (6379, 1), (8000, 2)])
        );
        for port in [5432, 6379, 8000] {
            assert_eq!(
                *state.listeners[&port]
                    .sources
                    .lock()
                    .expect("source set locks"),
                BTreeSet::from([first.subnet])
            );
        }
    }

    release_ports(&state, &first);

    {
        let state = state.lock().expect("state locks");
        let network = &state.networks[&first.network];
        assert_eq!(network.desired, BTreeSet::from([6379, 8000]));
        assert_eq!(network.active_ports, BTreeMap::from([(6379, 1), (8000, 1)]));
        assert!(!state.listeners.contains_key(&5432));
        assert!(state.listeners.contains_key(&6379));
        assert!(state.listeners.contains_key(&8000));
    }

    release_ports(&state, &second);

    {
        let state = state.lock().expect("state locks");
        let network = &state.networks[&first.network];
        assert_eq!(network.users, 0);
        assert!(network.active_ports.is_empty());
        assert_eq!(network.desired, BTreeSet::from([6379, 8000]));
    }

    forget_network(&state, &first.network);
}

#[test]
fn projects_with_the_same_target_port_share_one_listener() {
    let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("port probe binds");
    let port = probe.local_addr().expect("probe address").port();
    let gateway = Ipv4Addr::LOCALHOST;
    let first = lease_request(Path::new("/tmp/one"), gateway, vec![port]);
    let second = lease_request(
        Path::new("/tmp/two"),
        Ipv4Addr::new(10, 20, 30, 1),
        vec![port],
    );
    let state = Arc::new(Mutex::new(BrokerState::default()));

    let first_ports = acquire_ports(&state, &first).expect("first project acquires listener");
    let second_ports = acquire_ports(&state, &second).expect("second project shares listener");

    {
        let state = state.lock().expect("state locks");
        assert_eq!(state.networks.len(), 2);
        assert_eq!(state.listeners.len(), 1);
        assert_eq!(state.listeners[&port].users, 2);
        assert_ne!(state.listeners[&port].exposed_port, port);
        assert_eq!(first_ports, second_ports);
        assert_eq!(
            *state.listeners[&port]
                .sources
                .lock()
                .expect("source set locks"),
            BTreeSet::from([subnet("10.20.30.0/24"), subnet("127.0.0.0/8")])
        );
    }
    drop(probe);

    release_ports(&state, &first);
    release_ports(&state, &second);
    forget_network(&state, &first.network);
    assert_eq!(state.lock().expect("state locks").listeners.len(), 1);
    forget_network(&state, &second.network);
    assert!(state.lock().expect("state locks").listeners.is_empty());
}

#[test]
fn cleanup_registration_preserves_another_projects_shared_listener() {
    let gateway = Ipv4Addr::LOCALHOST;
    let active = lease_request(Path::new("/tmp/active"), gateway, vec![5432]);
    let mut cleanup = lease_request(Path::new("/tmp/cleanup"), gateway, Vec::new());
    cleanup.cleanup_only = true;
    let cancelled = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(BrokerState {
        networks: HashMap::from([(
            active.network.clone(),
            NetworkLease {
                network: ProjectNetwork {
                    name: active.network.clone(),
                    project_root: active.project_root.clone(),
                    subnet: active.subnet,
                    gateway,
                },
                users: 0,
                active_ports: BTreeMap::new(),
                desired: BTreeSet::from([5432]),
            },
        )]),
        listeners: HashMap::from([(
            5432,
            ListenerLease {
                users: 0,
                exposed_port: 15432,
                cancelled: Arc::clone(&cancelled),
                sources: Arc::new(Mutex::new(BTreeSet::from([active.subnet]))),
            },
        )]),
        relay_capacity: Arc::new(RelayCapacity::default()),
    }));

    register_cleanup(&state, &cleanup).expect("cleanup network registers");

    let state = state.lock().expect("state locks");
    assert_eq!(state.networks.len(), 2);
    assert_eq!(state.listeners.len(), 1);
    assert!(!cancelled.load(Ordering::Relaxed));
}

fn lease_request(root: &Path, gateway: Ipv4Addr, ports: Vec<u16>) -> LeaseRequest {
    let subnet = if gateway.is_loopback() {
        subnet("127.0.0.0/8")
    } else {
        subnet(&format!("{gateway}/24"))
    };
    LeaseRequest {
        network: network_name(root),
        project_root: root.to_path_buf(),
        subnet,
        gateway,
        ports,
        cleanup_only: false,
    }
}

fn subnet(value: &str) -> Ipv4Subnet {
    value.parse().expect("test subnet parses")
}

#[test]
fn attached_network_delete_is_retried_after_detach() {
    assert_eq!(
        network_delete_outcome(false, b"network is still attached to a container"),
        NetworkCleanup::Attached
    );
    assert_eq!(
        network_delete_outcome(
            false,
            b"cannot delete subnet 192.168.64.0/24 with referring containers"
        ),
        NetworkCleanup::Attached
    );
    assert_eq!(
        network_delete_outcome(false, b"network not found"),
        NetworkCleanup::Finished
    );
    assert_eq!(network_delete_outcome(true, b""), NetworkCleanup::Finished);
}

#[test]
fn transient_cleanup_failure_retains_network_and_relays() {
    let root = Path::new("/tmp/transient-cleanup");
    let gateway = Ipv4Addr::LOCALHOST;
    let request = lease_request(root, gateway, vec![5432]);
    let network = ProjectNetwork {
        name: request.network.clone(),
        project_root: root.to_path_buf(),
        subnet: request.subnet,
        gateway,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(BrokerState {
        networks: HashMap::from([(
            request.network.clone(),
            NetworkLease {
                network: network.clone(),
                users: 0,
                active_ports: BTreeMap::new(),
                desired: BTreeSet::from([5432]),
            },
        )]),
        listeners: HashMap::from([(
            5432,
            ListenerLease {
                users: 0,
                exposed_port: 15432,
                cancelled: Arc::clone(&cancelled),
                sources: Arc::new(Mutex::new(BTreeSet::from([request.subnet]))),
            },
        )]),
        relay_capacity: Arc::new(RelayCapacity::default()),
    }));

    handle_network_cleanup(&state, &network, Err(anyhow!("temporary runtime failure")));

    let state = state.lock().expect("state locks");
    assert!(state.networks.contains_key(&request.network));
    assert!(state.listeners.contains_key(&5432));
    assert!(!cancelled.load(Ordering::Relaxed));
}

#[test]
fn acquired_listener_maps_a_busy_target_port_and_relays() {
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("target binds");
    let target_port = target.local_addr().expect("target address").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = target.accept().expect("target accepts");
        let mut input = [0_u8; 4];
        stream.read_exact(&mut input).expect("target reads");
        assert_eq!(&input, b"ping");
        stream.write_all(b"pong").expect("target writes");
    });
    let request = lease_request(
        Path::new("/tmp/relay-integration"),
        Ipv4Addr::LOCALHOST,
        vec![target_port],
    );
    let state = Arc::new(Mutex::new(BrokerState::default()));

    let mappings = acquire_ports(&state, &request).expect("relay listener is acquired");
    let exposed_port = mappings[&target_port];
    assert_ne!(exposed_port, target_port);

    let mut client =
        TcpStream::connect((Ipv4Addr::LOCALHOST, exposed_port)).expect("allowed client connects");
    client.write_all(b"ping").expect("client writes");
    client
        .shutdown(Shutdown::Write)
        .expect("client finishes request");
    let mut output = Vec::new();
    client.read_to_end(&mut output).expect("client reads");
    assert_eq!(output, b"pong");

    release_ports(&state, &request);
    forget_network(&state, &request.network);
    server.join().expect("target joins");
}

#[test]
fn relay_copies_bytes_in_both_directions() {
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("target binds");
    let port = target.local_addr().expect("target address").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = target.accept().expect("target accepts");
        let mut input = [0_u8; 4];
        stream.read_exact(&mut input).expect("target reads");
        assert_eq!(&input, b"ping");
        stream.write_all(b"pong").expect("target writes");
    });

    let relay = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("relay binds");
    let relay_address = relay.local_addr().expect("relay address");
    let worker = thread::spawn(move || {
        let (stream, _) = relay.accept().expect("relay accepts");
        relay_connection(stream, port).expect("connection relays");
    });
    let mut client = TcpStream::connect(relay_address).expect("client connects");
    client.write_all(b"ping").expect("client writes");
    client
        .shutdown(Shutdown::Write)
        .expect("client finishes request");
    let mut output = Vec::new();
    client.read_to_end(&mut output).expect("client reads");
    assert_eq!(output, b"pong");
    worker.join().expect("relay joins");
    server.join().expect("server joins");
}

#[test]
fn relay_reaches_ipv6_only_loopback_services() {
    let target = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).expect("IPv6 target binds");
    let port = target.local_addr().expect("target address").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = target.accept().expect("IPv6 target accepts");
        let mut input = [0_u8; 4];
        stream.read_exact(&mut input).expect("target reads");
        assert_eq!(&input, b"ping");
        stream.write_all(b"pong").expect("target writes");
    });

    let relay = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("relay binds");
    let relay_address = relay.local_addr().expect("relay address");
    let worker = thread::spawn(move || {
        let (stream, _) = relay.accept().expect("relay accepts");
        relay_connection(stream, port).expect("connection relays");
    });
    let mut client = TcpStream::connect(relay_address).expect("client connects");
    client.write_all(b"ping").expect("client writes");
    client
        .shutdown(Shutdown::Write)
        .expect("client finishes request");
    let mut output = Vec::new();
    client.read_to_end(&mut output).expect("client reads");
    assert_eq!(output, b"pong");
    worker.join().expect("relay joins");
    server.join().expect("server joins");
}
