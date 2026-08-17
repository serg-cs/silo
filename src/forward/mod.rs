use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{Config, Forward};

const CONTAINER_BIN: &str = "container";
const BROKER_ENV: &str = "SILO_INTERNAL_FORWARD_BROKER";
const NETWORK_PREFIX: &str = "silo-net-";
const DIGEST_HEX_LEN: usize = 24;
const BROKER_STATE_PARENT: &str = "/tmp";
const BROKER_STATE_PREFIX: &str = "silo-fwd-";
const BROKER_SOCKET_NAME: &str = "broker.sock";
const BROKER_LOCK_NAME: &str = "broker.lock";
const BROKER_SOCKET_SUFFIX: &str = ".sock";
const PROJECT_LOCK_SUFFIX: &str = ".lock";
const DARWIN_UNIX_PATH_MAX: usize = 104;
const LABEL_OWNER: &str = "dev.silo.owner";
const LABEL_SCHEMA: &str = "dev.silo.schema";
const LABEL_PROJECT: &str = "dev.silo.project";
const LABEL_PROJECT_ROOT: &str = "dev.silo.project-root";
const LABEL_OWNER_VALUE: &str = "silo";
const LABEL_SCHEMA_VALUE: &str = "1";
const BROKER_START_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const NETWORK_DELETE_INTERVAL: Duration = Duration::from_millis(500);
const MAX_RELAY_CONNECTIONS: usize = 64;

/// Host forwarding state held for the full lifetime of one Silo session.
pub(crate) struct Session {
    network: Option<ProjectNetwork>,
    built_in_environment: Vec<(String, String)>,
    custom_environment: Vec<(String, String)>,
    guest: Option<GuestForwarding>,
    lease: Option<UnixStream>,
}

/// Guest-side loopback listeners and their host-broker destinations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuestForwarding {
    gateway: Ipv4Addr,
    ports: BTreeMap<u16, u16>,
}

impl GuestForwarding {
    pub(crate) fn new(gateway: Ipv4Addr, ports: BTreeMap<u16, u16>) -> Self {
        Self { gateway, ports }
    }

    pub(crate) const fn gateway(&self) -> Ipv4Addr {
        self.gateway
    }

    pub(crate) fn ports(&self) -> &BTreeMap<u16, u16> {
        &self.ports
    }
}

impl Session {
    /// Prepares the isolated project network and acquires relay listeners.
    pub(crate) fn prepare(config: &Config, project_root: &Path) -> Result<Self> {
        let enabled = enabled_forwards(&config.forward);
        if enabled.is_empty() {
            return Ok(Self {
                network: None,
                built_in_environment: Vec::new(),
                custom_environment: Vec::new(),
                guest: None,
                lease: None,
            });
        }

        require_runtime_version()?;
        sweep_orphaned_networks();
        let broker_path = broker_socket_path()?;
        let project_lock = project_lock_path(project_root)?;
        // Serialize network setup with broker cleanup so a newly queued lease
        // cannot refer to a network that the idle broker is deleting.
        let setup_lock = StartupLock::acquire(&project_lock)?;
        let network = ProjectNetwork::ensure(project_root)?;
        let request = LeaseRequest {
            network: network.name.clone(),
            project_root: network.project_root.clone(),
            subnet: network.subnet,
            gateway: network.gateway,
            ports: enabled.values().map(|entry| entry.port).collect(),
            cleanup_only: false,
        };
        let lease = match acquire_lease(&broker_path, &request) {
            Ok(lease) => lease,
            Err(err) => {
                // Release the setup barrier before broker-aware cleanup. A
                // live broker may own another lease for this same network.
                drop(setup_lock);
                cleanup_project_network(project_root);
                return Err(err);
            }
        };
        let details = forward_details(enabled, network.gateway, &lease.ports)?;
        Ok(Self {
            network: Some(network),
            built_in_environment: details.built_in_environment,
            custom_environment: details.custom_environment,
            guest: Some(details.guest),
            lease: Some(lease.stream),
        })
    }

    pub(crate) fn network_name(&self) -> Option<&str> {
        self.network.as_ref().map(|network| network.name.as_str())
    }

    pub(crate) fn environment(&self, custom_image: bool) -> &[(String, String)] {
        if custom_image {
            &self.custom_environment
        } else {
            &self.built_in_environment
        }
    }

    pub(crate) fn guest(&self) -> Option<&GuestForwarding> {
        self.guest.as_ref()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The broker owns cleanup so concurrent sessions cannot remove a
        // network while another container still uses it.
        self.lease.take();
    }
}

fn enabled_forwards(forwards: &BTreeMap<String, Forward>) -> BTreeMap<String, Forward> {
    forwards
        .iter()
        .filter(|(_, entry)| entry.is_enabled())
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect()
}

#[derive(Debug)]
struct ForwardDetails {
    built_in_environment: Vec<(String, String)>,
    custom_environment: Vec<(String, String)>,
    guest: GuestForwarding,
}

/// Resolves the public image contracts and the private guest listener map.
fn forward_details(
    enabled: BTreeMap<String, Forward>,
    gateway: Ipv4Addr,
    relay_ports: &BTreeMap<u16, u16>,
) -> Result<ForwardDetails> {
    let gateway_text = gateway.to_string();
    let mut built_in_environment = Vec::with_capacity(enabled.len());
    let mut custom_environment = Vec::with_capacity(enabled.len() * 2);
    let mut guest_ports = BTreeMap::new();
    for (name, entry) in enabled {
        let prefix = format!("SILO_{}", name.to_ascii_uppercase());
        let exposed_port = relay_ports.get(&entry.port).ok_or_else(|| {
            anyhow!(
                "host-forward relay omitted the listener for port {}",
                entry.port
            )
        })?;
        built_in_environment.push((format!("{prefix}_PORT"), entry.port.to_string()));
        custom_environment.push((format!("{prefix}_HOST"), gateway_text.clone()));
        custom_environment.push((format!("{prefix}_PORT"), exposed_port.to_string()));
        guest_ports.insert(entry.port, *exposed_port);
    }
    Ok(ForwardDetails {
        built_in_environment,
        custom_environment,
        guest: GuestForwarding::new(gateway, guest_ports),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectNetwork {
    name: String,
    project_root: PathBuf,
    subnet: Ipv4Subnet,
    gateway: Ipv4Addr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Ipv4Subnet {
    network: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Subnet {
    fn contains(self, address: Ipv4Addr) -> bool {
        let mask = u32::MAX
            .checked_shl(u32::from(32 - self.prefix))
            .unwrap_or(0);
        u32::from(address) & mask == u32::from(self.network)
    }
}

impl FromStr for Ipv4Subnet {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (address, prefix) = value
            .split_once('/')
            .ok_or_else(|| anyhow!("IPv4 subnet must use CIDR notation"))?;
        let address: Ipv4Addr = address.parse().context("invalid IPv4 subnet address")?;
        let prefix: u8 = prefix.parse().context("invalid IPv4 subnet prefix")?;
        if prefix > 32 {
            return Err(anyhow!("IPv4 subnet prefix must be at most 32"));
        }
        let mask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
        Ok(Self {
            network: Ipv4Addr::from(u32::from(address) & mask),
            prefix,
        })
    }
}

impl fmt::Display for Ipv4Subnet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.network, self.prefix)
    }
}

impl ProjectNetwork {
    fn ensure(project_root: &Path) -> Result<Self> {
        let name = network_name(project_root);
        if let Some(inspection) = inspect_network(&name)? {
            validate_network(&inspection, &name, project_root)
        } else {
            create_network(&name, project_root)?;
            let inspection = inspect_network(&name)?.ok_or_else(|| {
                anyhow!("Apple container did not return newly created network `{name}`")
            })?;
            validate_network(&inspection, &name, project_root)
        }
    }

    fn cleanup_once(&self) -> Result<NetworkCleanup> {
        let Some(inspection) = inspect_network(&self.name)? else {
            return Ok(NetworkCleanup::Finished);
        };
        validate_network(&inspection, &self.name, &self.project_root)?;
        let output = Command::new(CONTAINER_BIN)
            .args(["network", "delete", &self.name])
            .output()
            .context("failed to start Apple container network deletion")?;
        let outcome = network_delete_outcome(output.status.success(), &output.stderr);
        if outcome == NetworkCleanup::Failed {
            return Err(anyhow!(
                "{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(outcome)
    }
}

/// Removes this project's network once no project containers remain attached.
pub(crate) fn cleanup_project_network(project_root: &Path) {
    let name = network_name(project_root);
    if !network_may_need_cleanup(&name, inspect_network) {
        return;
    }
    if let Err(err) = cleanup_project_network_once(project_root, &name) {
        eprintln!("warning: could not remove forward network `{name}`: {err:#}");
    }
}

fn network_may_need_cleanup(
    name: &str,
    inspect: impl FnOnce(&str) -> Result<Option<Value>>,
) -> bool {
    inspect(name).is_ok_and(|inspection| inspection.is_some())
}

fn cleanup_project_network_once(project_root: &Path, name: &str) -> Result<()> {
    let project_lock = project_lock_path(project_root)?;
    let Some(_cleanup_lock) = StartupLock::try_acquire(&project_lock)? else {
        return Ok(());
    };
    let Some(inspection) = inspect_network(name)? else {
        return Ok(());
    };
    let network = validate_network(&inspection, name, project_root)?;
    let request = LeaseRequest {
        network: network.name.clone(),
        project_root: network.project_root.clone(),
        subnet: network.subnet,
        gateway: network.gateway,
        ports: Vec::new(),
        cleanup_only: true,
    };
    match connect_broker(&broker_socket_path()?, &request) {
        Ok(stream) => {
            drop(stream);
            return Ok(());
        }
        Err(BrokerConnectError::Rejected(err)) => return Err(err),
        Err(BrokerConnectError::Unavailable(_)) => {}
    }
    if broker_is_running(&legacy_broker_socket_path(project_root)?) {
        return Ok(());
    }
    network.cleanup_once().map(|_| ())
}

fn broker_is_running(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Reclaims Silo-owned networks left unattached by an interrupted process.
fn sweep_orphaned_networks() {
    let output = match Command::new(CONTAINER_BIN)
        .args(["network", "list", "--quiet"])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            eprintln!("warning: could not enumerate forward networks: {err}");
            return;
        }
    };
    if !output.status.success() {
        eprintln!(
            "warning: could not enumerate forward networks: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return;
    }
    for name in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|name| valid_network_name(name))
    {
        sweep_network(name);
    }
}

fn sweep_network(name: &str) {
    let Ok(Some(inspection)) = inspect_network(name) else {
        return;
    };
    let Some(project_root) = inspection
        .pointer(&format!("/configuration/labels/{LABEL_PROJECT_ROOT}"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
    else {
        return;
    };
    if validate_network(&inspection, name, &project_root).is_err() {
        return;
    }
    cleanup_project_network(&project_root);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkCleanup {
    Finished,
    Attached,
    Failed,
}

fn network_delete_outcome(success: bool, stderr: &[u8]) -> NetworkCleanup {
    if success {
        return NetworkCleanup::Finished;
    }
    let stderr = String::from_utf8_lossy(stderr).to_lowercase();
    if stderr.contains("not found") {
        NetworkCleanup::Finished
    } else if stderr.contains("in use")
        || stderr.contains("attached")
        || stderr.contains("referring containers")
    {
        NetworkCleanup::Attached
    } else {
        NetworkCleanup::Failed
    }
}

fn network_name(project_root: &Path) -> String {
    format!("{NETWORK_PREFIX}{}", project_key(project_root))
}

fn valid_network_name(name: &str) -> bool {
    let Some(digest) = name.strip_prefix(NETWORK_PREFIX) else {
        return false;
    };
    digest.len() == DIGEST_HEX_LEN
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn project_key(project_root: &Path) -> String {
    let digest = hex_digest(Sha256::digest(project_root.as_os_str().as_encoded_bytes()));
    digest[..DIGEST_HEX_LEN].to_string()
}

fn create_network(name: &str, project_root: &Path) -> Result<()> {
    let project = hex_digest(Sha256::digest(project_root.as_os_str().as_encoded_bytes()));
    let root = project_root.to_string_lossy();
    let output = Command::new(CONTAINER_BIN)
        .args(["network", "create"])
        .arg("--label")
        .arg(format!("{LABEL_OWNER}={LABEL_OWNER_VALUE}"))
        .arg("--label")
        .arg(format!("{LABEL_SCHEMA}={LABEL_SCHEMA_VALUE}"))
        .arg("--label")
        .arg(format!("{LABEL_PROJECT}={project}"))
        .arg("--label")
        .arg(format!("{LABEL_PROJECT_ROOT}={root}"))
        .arg(name)
        .output()
        .context("failed to start Apple container network creation")?;
    if output.status.success() || String::from_utf8_lossy(&output.stderr).contains("already exists")
    {
        return Ok(());
    }
    Err(anyhow!(
        "could not create forward network `{name}`: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn inspect_network(name: &str) -> Result<Option<Value>> {
    let output = Command::new(CONTAINER_BIN)
        .args(["network", "inspect", name])
        .output()
        .context("failed to inspect Apple container network")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if stderr.to_lowercase().contains("not found") {
            return Ok(None);
        }
        return Err(anyhow!(
            "could not inspect network `{name}`: {}",
            stderr.trim()
        ));
    }
    let items: Vec<Value> = serde_json::from_slice(&output.stdout)
        .context("invalid Apple container network inspect JSON")?;
    Ok(items
        .into_iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(name)))
}

fn validate_network(inspection: &Value, name: &str, project_root: &Path) -> Result<ProjectNetwork> {
    let project = hex_digest(Sha256::digest(project_root.as_os_str().as_encoded_bytes()));
    let expected_root = project_root.to_string_lossy();
    for (key, expected) in [
        (LABEL_OWNER, LABEL_OWNER_VALUE),
        (LABEL_SCHEMA, LABEL_SCHEMA_VALUE),
        (LABEL_PROJECT, project.as_str()),
        (LABEL_PROJECT_ROOT, expected_root.as_ref()),
    ] {
        let pointer = format!("/configuration/labels/{key}");
        let actual = inspection.pointer(&pointer).and_then(Value::as_str);
        if actual != Some(expected) {
            return Err(anyhow!(
                "refusing to manage network `{name}`: label `{key}` is {actual:?}, expected `{expected}`"
            ));
        }
    }
    let gateway = inspection
        .pointer("/status/ipv4Gateway")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("network `{name}` has no IPv4 gateway"))?
        .parse()
        .with_context(|| format!("network `{name}` has an invalid IPv4 gateway"))?;
    let subnet: Ipv4Subnet = inspection
        .pointer("/status/ipv4Subnet")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("network `{name}` has no IPv4 subnet"))?
        .parse()
        .with_context(|| format!("network `{name}` has an invalid IPv4 subnet"))?;
    if !subnet.contains(gateway) {
        return Err(anyhow!(
            "network `{name}` gateway {gateway} is outside its IPv4 subnet {subnet}"
        ));
    }
    Ok(ProjectNetwork {
        name: name.to_string(),
        project_root: project_root.to_path_buf(),
        subnet,
        gateway,
    })
}

fn require_runtime_version() -> Result<()> {
    let output = Command::new(CONTAINER_BIN)
        .args(["system", "version", "--format", "json"])
        .output()
        .context("failed to inspect Apple container version")?;
    if !output.status.success() {
        return Err(anyhow!(
            "host forwarding requires Apple container 1.2.1 or newer: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let values: Vec<Value> =
        serde_json::from_slice(&output.stdout).context("invalid Apple container version JSON")?;
    let version = values
        .iter()
        .find(|value| value.get("appName").and_then(Value::as_str) == Some("container"))
        .and_then(|value| value.get("version"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Apple container version output omitted the CLI version"))?;
    if version_at_least(version, (1, 2, 1)) {
        Ok(())
    } else {
        Err(anyhow!(
            "host forwarding requires Apple container 1.2.1 or newer; found {version}"
        ))
    }
}

fn version_at_least(version: &str, minimum: (u64, u64, u64)) -> bool {
    let mut parts = version.split('.').map(|part| {
        part.bytes()
            .take_while(u8::is_ascii_digit)
            .fold(0_u64, |value, digit| value * 10 + u64::from(digit - b'0'))
    });
    let actual = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    actual >= minimum
}

#[derive(Debug, Serialize, Deserialize)]
struct LeaseRequest {
    network: String,
    project_root: PathBuf,
    subnet: Ipv4Subnet,
    gateway: Ipv4Addr,
    ports: Vec<u16>,
    #[serde(default)]
    cleanup_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct LeaseResponse {
    error: Option<String>,
    #[serde(default)]
    ports: BTreeMap<u16, u16>,
}

struct RelayLease {
    stream: UnixStream,
    ports: BTreeMap<u16, u16>,
}

fn acquire_lease(path: &Path, request: &LeaseRequest) -> Result<RelayLease> {
    match connect_broker(path, request) {
        Ok(stream) => Ok(stream),
        Err(BrokerConnectError::Rejected(err)) => Err(err),
        Err(BrokerConnectError::Unavailable(_)) => {
            // Serialize broker spawning across projects, then recheck in case
            // another project started the shared broker first.
            let _startup_lock = StartupLock::acquire(&broker_lock_path()?)?;
            match connect_broker(path, request) {
                Ok(stream) => return Ok(stream),
                Err(BrokerConnectError::Rejected(err)) => return Err(err),
                Err(BrokerConnectError::Unavailable(_)) => {}
            }
            spawn_broker(path)?;
            wait_for_broker(path, request)
        }
    }
}

struct StartupLock {
    file: fs::File,
}

impl StartupLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = Self::open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error()).context("could not lock relay startup");
        }
        Ok(Self { file })
    }

    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = Self::open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(Self { file }));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(err).context("could not lock relay cleanup")
        }
    }

    fn open(path: &Path) -> Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("could not open relay startup lock `{}`", path.display()))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(anyhow!(
                "refusing to use unsafe relay startup lock `{}`",
                path.display()
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn broker_socket_path() -> Result<PathBuf> {
    broker_state_path(BROKER_SOCKET_NAME)
}

fn broker_lock_path() -> Result<PathBuf> {
    broker_state_path(BROKER_LOCK_NAME)
}

fn project_lock_path(project_root: &Path) -> Result<PathBuf> {
    broker_state_path(&format!(
        "{}{PROJECT_LOCK_SUFFIX}",
        project_key(project_root)
    ))
}

fn legacy_broker_socket_path(project_root: &Path) -> Result<PathBuf> {
    broker_state_path(&format!(
        "{}{BROKER_SOCKET_SUFFIX}",
        project_key(project_root)
    ))
}

fn broker_state_path(name: &str) -> Result<PathBuf> {
    let root = broker_state_root();
    fs::create_dir_all(&root).with_context(|| {
        format!(
            "could not create relay state directory `{}`",
            root.display()
        )
    })?;
    validate_broker_state_root(&root)?;
    Ok(root.join(name))
}

fn broker_state_root() -> PathBuf {
    Path::new(BROKER_STATE_PARENT).join(format!("{BROKER_STATE_PREFIX}{}", unsafe {
        libc::geteuid()
    }))
}

fn validate_broker_state_root(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root).with_context(|| {
        format!(
            "could not inspect relay state directory `{}`",
            root.display()
        )
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(anyhow!(
            "refusing to use unsafe relay state directory `{}`",
            root.display()
        ));
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn validate_broker_socket_path(path: &Path) -> Result<()> {
    let root = broker_state_root();
    validate_broker_state_root(&root)?;
    let valid_parent = path.parent() == Some(root.as_path());
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(valid_broker_socket_name);
    let valid_length = path.as_os_str().as_encoded_bytes().len() < DARWIN_UNIX_PATH_MAX;
    if !path.is_absolute() || !valid_parent || !valid_name || !valid_length {
        return Err(anyhow!(
            "refusing unsafe internal relay socket path `{}`",
            path.display()
        ));
    }
    Ok(())
}

fn valid_broker_socket_name(name: &str) -> bool {
    name == BROKER_SOCKET_NAME
}

fn spawn_broker(path: &Path) -> Result<()> {
    let executable = std::env::current_exe().context("could not locate the Silo executable")?;
    let mut broker = Command::new(executable);
    broker
        .env(BROKER_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    isolate_broker_process_group(&mut broker);
    broker
        .spawn()
        .context("could not start host-forward relay broker")?;
    Ok(())
}

/// Keeps terminal-generated signals in the interactive Silo process group.
fn isolate_broker_process_group(command: &mut Command) {
    command.process_group(0);
}

fn wait_for_broker(path: &Path, request: &LeaseRequest) -> Result<RelayLease> {
    let deadline = Instant::now() + BROKER_START_TIMEOUT;
    loop {
        match connect_broker(path, request) {
            Ok(stream) => return Ok(stream),
            Err(BrokerConnectError::Unavailable(_)) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(BrokerConnectError::Unavailable(err) | BrokerConnectError::Rejected(err)) => {
                return Err(err);
            }
        }
    }
}

enum BrokerConnectError {
    Unavailable(anyhow::Error),
    Rejected(anyhow::Error),
}

fn connect_broker(
    path: &Path,
    request: &LeaseRequest,
) -> std::result::Result<RelayLease, BrokerConnectError> {
    let mut stream = UnixStream::connect(path).map_err(|err| {
        BrokerConnectError::Unavailable(anyhow!(err).context(format!(
            "could not connect to relay broker `{}`",
            path.display()
        )))
    })?;
    stream
        .set_read_timeout(Some(BROKER_START_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(BROKER_START_TIMEOUT)))
        .map_err(|err| BrokerConnectError::Unavailable(err.into()))?;
    serde_json::to_writer(&mut stream, request)
        .context("could not send relay lease")
        .map_err(BrokerConnectError::Unavailable)?;
    stream
        .write_all(b"\n")
        .map_err(|err| BrokerConnectError::Unavailable(err.into()))?;
    let mut response = String::new();
    let reader = stream
        .try_clone()
        .map_err(|err| BrokerConnectError::Unavailable(err.into()))?;
    BufReader::new(reader)
        .read_line(&mut response)
        .context("could not read relay broker response")
        .map_err(BrokerConnectError::Unavailable)?;
    let response: LeaseResponse = serde_json::from_str(&response)
        .context("invalid relay broker response")
        .map_err(BrokerConnectError::Unavailable)?;
    if let Some(error) = response.error {
        return Err(BrokerConnectError::Rejected(anyhow!(error)));
    }
    Ok(RelayLease {
        stream,
        ports: response.ports,
    })
}

/// Runs the internal relay broker when the private environment marker exists.
pub(crate) fn run_internal_broker() -> Option<Result<()>> {
    std::env::var_os(BROKER_ENV).map(|path| run_broker(Path::new(&path)))
}

fn run_broker(path: &Path) -> Result<()> {
    validate_broker_socket_path(path)?;

    // Replace only a stale socket owned inside Silo's private directory.
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() } =>
        {
            fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(anyhow!(
                "refusing to replace relay socket `{}`",
                path.display()
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("could not inspect relay socket"),
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("could not bind relay socket `{}`", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let state = Arc::new(Mutex::new(BrokerState::default()));
    let active = Arc::new(AtomicUsize::new(0));
    let mut seen_client = false;
    let mut idle_since = Instant::now();
    let mut last_cleanup = None;

    // Keep one user-wide broker alive while any project network remains.
    let _shutdown_lock = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                seen_client = true;
                active.fetch_add(1, Ordering::Relaxed);
                let state = Arc::clone(&state);
                let active = Arc::clone(&active);
                thread::spawn(move || {
                    handle_control(stream, &state);
                    active.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err).context("relay broker accept failed"),
        }

        if last_cleanup.is_none_or(|last: Instant| last.elapsed() >= NETWORK_DELETE_INTERVAL) {
            last_cleanup = Some(Instant::now());
            cleanup_idle_networks(&state);
        }

        let empty = state.lock().is_ok_and(|state| state.networks.is_empty());
        if active.load(Ordering::Relaxed) != 0 || !empty {
            idle_since = Instant::now();
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        let timeout = if seen_client {
            BROKER_IDLE_TIMEOUT
        } else {
            BROKER_START_TIMEOUT
        };
        if idle_since.elapsed() < timeout {
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        let Some(shutdown_lock) = StartupLock::try_acquire(&broker_lock_path()?)? else {
            thread::sleep(POLL_INTERVAL);
            continue;
        };
        let still_empty = state.lock().is_ok_and(|state| state.networks.is_empty());
        if active.load(Ordering::Relaxed) == 0 && still_empty {
            break shutdown_lock;
        }
        idle_since = Instant::now();
        thread::sleep(POLL_INTERVAL);
    };
    cancel_listeners(&state);
    let _ = fs::remove_file(path);
    Ok(())
}

#[derive(Default)]
struct BrokerState {
    networks: HashMap<String, NetworkLease>,
    listeners: HashMap<u16, ListenerLease>,
    relay_capacity: Arc<RelayCapacity>,
}

struct NetworkLease {
    network: ProjectNetwork,
    users: usize,
    // Count each active lease so overlapping configurations retain their union.
    active_ports: BTreeMap<u16, usize>,
    desired: BTreeSet<u16>,
}

fn cleanup_idle_networks(state: &Arc<Mutex<BrokerState>>) {
    let candidates: Vec<_> = state
        .lock()
        .map(|state| {
            state
                .networks
                .values()
                .filter(|lease| lease.users == 0)
                .map(|lease| lease.network.clone())
                .collect()
        })
        .unwrap_or_default();
    for network in candidates {
        cleanup_idle_network(state, &network);
    }
}

fn cleanup_idle_network(state: &Arc<Mutex<BrokerState>>, network: &ProjectNetwork) {
    let Ok(project_lock) = project_lock_path(&network.project_root) else {
        return;
    };
    let Ok(Some(_cleanup_lock)) = StartupLock::try_acquire(&project_lock) else {
        return;
    };
    let remains_idle = state
        .lock()
        .ok()
        .and_then(|state| {
            state
                .networks
                .get(&network.name)
                .map(|lease| lease.users == 0)
        })
        .unwrap_or(false);
    if !remains_idle {
        return;
    }
    handle_network_cleanup(state, network, network.cleanup_once());
}

fn handle_network_cleanup(
    state: &Arc<Mutex<BrokerState>>,
    network: &ProjectNetwork,
    result: Result<NetworkCleanup>,
) {
    match result {
        Ok(NetworkCleanup::Attached) => {}
        Ok(NetworkCleanup::Finished) => forget_network(state, &network.name),
        Ok(NetworkCleanup::Failed) => unreachable!("failed deletion is returned as an error"),
        Err(err) => {
            eprintln!(
                "warning: could not remove idle forward network `{}`: {err:#}",
                network.name
            );
        }
    }
}

fn forget_network(state: &Arc<Mutex<BrokerState>>, name: &str) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    state.networks.remove(name);
    remove_undesired_idle_listeners(&mut state);
}

struct ListenerLease {
    users: usize,
    exposed_port: u16,
    cancelled: Arc<AtomicBool>,
    sources: Arc<Mutex<BTreeSet<Ipv4Subnet>>>,
}

#[derive(Default)]
struct RelayCapacity {
    active: AtomicUsize,
}

impl RelayCapacity {
    fn try_acquire(self: &Arc<Self>) -> Option<RelayPermit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_RELAY_CONNECTIONS).then_some(active + 1)
            })
            .ok()
            .map(|_| RelayPermit {
                capacity: Arc::clone(self),
            })
    }
}

struct RelayPermit {
    capacity: Arc<RelayCapacity>,
}

impl Drop for RelayPermit {
    fn drop(&mut self) {
        self.capacity.active.fetch_sub(1, Ordering::Release);
    }
}

fn handle_control(mut stream: UnixStream, state: &Arc<Mutex<BrokerState>>) {
    let mut line = String::new();
    let request = BufReader::new(&stream)
        .read_line(&mut line)
        .and_then(|_| serde_json::from_str::<LeaseRequest>(&line).map_err(io::Error::other));
    let result = match request.as_ref() {
        Ok(request) if request.cleanup_only => {
            register_cleanup(state, request).map(|()| BTreeMap::new())
        }
        Ok(request) => acquire_ports(state, request),
        Err(err) => Err(anyhow!("invalid relay lease: {err}")),
    };
    let response = match result {
        Ok(ports) => LeaseResponse { error: None, ports },
        Err(err) => LeaseResponse {
            error: Some(format!("{err:#}")),
            ports: BTreeMap::new(),
        },
    };
    let failed = response.error.is_some();
    let _ = serde_json::to_writer(&mut stream, &response);
    let _ = stream.write_all(b"\n");
    if failed || request.as_ref().is_ok_and(|request| request.cleanup_only) {
        return;
    }

    // The open control stream is the lease. Idle listeners remain available
    // to background container processes until the network detaches.
    let mut byte = [0_u8; 1];
    while stream.read(&mut byte).is_ok_and(|read| read != 0) {}
    if let Ok(request) = request {
        release_ports(state, &request);
    }
}

fn register_cleanup(state: &Arc<Mutex<BrokerState>>, request: &LeaseRequest) -> Result<()> {
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("relay state lock was poisoned"))?;
    register_network(&mut state, request)
}

fn acquire_ports(
    state: &Arc<Mutex<BrokerState>>,
    request: &LeaseRequest,
) -> Result<BTreeMap<u16, u16>> {
    let mut unique = request.ports.clone();
    unique.sort_unstable();
    unique.dedup();
    let mut state = state
        .lock()
        .map_err(|_| anyhow!("relay state lock was poisoned"))?;
    let relay_capacity = Arc::clone(&state.relay_capacity);
    register_network(&mut state, request)?;
    let desired: BTreeSet<_> = unique.iter().copied().collect();
    let mut acquired = Vec::new();
    for port in unique {
        if let Some(listener) = state.listeners.get_mut(&port) {
            listener.users += 1;
            acquired.push(AcquiredListener {
                port,
                created: false,
            });
            continue;
        }
        // vmnet gateway addresses are reachable from guests but are not local
        // macOS interface addresses, so accept on an ephemeral wildcard port.
        let tcp = match TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) {
            Ok(listener) => listener,
            Err(err) => {
                rollback_ports(&mut state, &acquired);
                return Err(err)
                    .with_context(|| format!("could not expose host service on port {port}"));
            }
        };
        let exposed_port = match tcp.local_addr() {
            Ok(address) => address.port(),
            Err(err) => {
                rollback_ports(&mut state, &acquired);
                return Err(err).context("could not inspect host-forward listener");
            }
        };
        if let Err(err) = tcp.set_nonblocking(true) {
            rollback_ports(&mut state, &acquired);
            return Err(err).context("could not configure host-forward listener");
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let sources = Arc::new(Mutex::new(BTreeSet::from([request.subnet])));
        spawn_tcp_listener(
            tcp,
            port,
            Arc::clone(&cancelled),
            Arc::clone(&relay_capacity),
            Arc::clone(&sources),
        );
        state.listeners.insert(
            port,
            ListenerLease {
                users: 1,
                exposed_port,
                cancelled,
                sources,
            },
        );
        acquired.push(AcquiredListener {
            port,
            created: true,
        });
    }
    let Some(_) = state.networks.get(&request.network) else {
        rollback_ports(&mut state, &acquired);
        return Err(anyhow!(
            "registered relay network `{}` disappeared",
            request.network
        ));
    };
    activate_desired_ports(&mut state, &request.network, &desired);
    request
        .ports
        .iter()
        .map(|port| {
            state
                .listeners
                .get(port)
                .map(|listener| (*port, listener.exposed_port))
                .ok_or_else(|| anyhow!("host-forward listener for port {port} disappeared"))
        })
        .collect()
}

fn register_network(state: &mut BrokerState, request: &LeaseRequest) -> Result<()> {
    let requested = ProjectNetwork {
        name: request.network.clone(),
        project_root: request.project_root.clone(),
        subnet: request.subnet,
        gateway: request.gateway,
    };
    if network_name(&requested.project_root) != requested.name {
        return Err(anyhow!(
            "relay network `{}` does not match project `{}`",
            requested.name,
            requested.project_root.display()
        ));
    }
    if !requested.subnet.contains(requested.gateway) {
        return Err(anyhow!(
            "relay network `{}` gateway {} is outside its IPv4 subnet {}",
            requested.name,
            requested.gateway,
            requested.subnet
        ));
    }
    match state.networks.get(&requested.name) {
        Some(existing) if existing.network != requested => Err(anyhow!(
            "relay broker project identity does not match network `{}`",
            request.network
        )),
        Some(_) => Ok(()),
        None => {
            state.networks.insert(
                requested.name.clone(),
                NetworkLease {
                    network: requested,
                    users: 0,
                    active_ports: BTreeMap::new(),
                    desired: BTreeSet::new(),
                },
            );
            Ok(())
        }
    }
}

fn release_ports(state: &Arc<Mutex<BrokerState>>, request: &LeaseRequest) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    let mut keys = request.ports.clone();
    keys.sort_unstable();
    keys.dedup();
    for key in &keys {
        if let Some(listener) = state.listeners.get_mut(key) {
            listener.users = listener.users.saturating_sub(1);
        }
    }
    let desired = keys.into_iter().collect();
    deactivate_desired_ports(&mut state, &request.network, &desired);
    remove_undesired_idle_listeners(&mut state);
}

struct AcquiredListener {
    port: u16,
    created: bool,
}

fn rollback_ports(state: &mut BrokerState, acquired: &[AcquiredListener]) {
    for entry in acquired {
        let remove = state
            .listeners
            .get_mut(&entry.port)
            .is_some_and(|listener| {
                listener.users = listener.users.saturating_sub(1);
                entry.created && listener.users == 0
            });
        if remove && let Some(listener) = state.listeners.remove(&entry.port) {
            listener.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

fn activate_desired_ports(state: &mut BrokerState, network: &str, desired: &BTreeSet<u16>) {
    if let Some(network) = state.networks.get_mut(network) {
        network.users += 1;
        for port in desired {
            *network.active_ports.entry(*port).or_default() += 1;
        }
        network.desired = network.active_ports.keys().copied().collect();
    }
    remove_undesired_idle_listeners(state);
}

fn deactivate_desired_ports(state: &mut BrokerState, network: &str, desired: &BTreeSet<u16>) {
    let Some(network) = state.networks.get_mut(network) else {
        return;
    };
    network.users = network.users.saturating_sub(1);
    for port in desired {
        let remove = network.active_ports.get_mut(port).is_some_and(|users| {
            *users = users.saturating_sub(1);
            *users == 0
        });
        if remove {
            network.active_ports.remove(port);
        }
    }
    // Preserve the final lease's forwards for attached background containers.
    if network.users > 0 {
        network.desired = network.active_ports.keys().copied().collect();
    }
}

fn remove_undesired_idle_listeners(state: &mut BrokerState) {
    // Keep each wildcard listener restricted to its owning vmnet subnets.
    for (port, listener) in &state.listeners {
        let sources = state
            .networks
            .values()
            .filter(|network| network.desired.contains(port))
            .map(|network| network.network.subnet)
            .collect();
        if let Ok(mut allowed) = listener.sources.lock() {
            *allowed = sources;
        }
    }
    let idle_keys: Vec<_> = state
        .listeners
        .iter()
        .filter(|(key, listener)| {
            listener.users == 0
                && !state
                    .networks
                    .values()
                    .any(|network| network.desired.contains(key))
        })
        .map(|(key, _)| *key)
        .collect();
    for key in idle_keys {
        if let Some(listener) = state.listeners.remove(&key) {
            listener.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

fn cancel_listeners(state: &Arc<Mutex<BrokerState>>) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    for (_, listener) in state.listeners.drain() {
        listener.cancelled.store(true, Ordering::Relaxed);
    }
}

fn spawn_tcp_listener(
    listener: TcpListener,
    port: u16,
    cancelled: Arc<AtomicBool>,
    relay_capacity: Arc<RelayCapacity>,
    sources: Arc<Mutex<BTreeSet<Ipv4Subnet>>>,
) {
    thread::spawn(move || {
        while !cancelled.load(Ordering::Relaxed) {
            let Some(permit) = relay_capacity.try_acquire() else {
                thread::sleep(POLL_INTERVAL);
                continue;
            };
            match listener.accept() {
                Ok((incoming, peer)) if source_is_allowed(peer, &sources) => {
                    let _ = thread::Builder::new().spawn(move || {
                        let _permit = permit;
                        let _ = relay_connection(incoming, port);
                    });
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    drop(permit);
                    thread::sleep(POLL_INTERVAL);
                }
                Err(_) => break,
            }
        }
    });
}

fn source_is_allowed(peer: SocketAddr, sources: &Arc<Mutex<BTreeSet<Ipv4Subnet>>>) -> bool {
    let SocketAddr::V4(peer) = peer else {
        return false;
    };
    sources
        .lock()
        .is_ok_and(|sources| sources.iter().any(|subnet| subnet.contains(*peer.ip())))
}

fn relay_connection(mut incoming: TcpStream, port: u16) -> Result<()> {
    let loopback = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ];
    let mut outgoing = TcpStream::connect(loopback.as_slice())
        .with_context(|| format!("host service on loopback port {port} is unavailable"))?;
    let mut incoming_read = incoming.try_clone()?;
    let mut outgoing_write = outgoing.try_clone()?;
    let upload = thread::spawn(move || {
        let result = io::copy(&mut incoming_read, &mut outgoing_write);
        let _ = outgoing_write.shutdown(Shutdown::Write);
        result
    });
    let download = io::copy(&mut outgoing, &mut incoming);
    let _ = incoming.shutdown(Shutdown::Write);
    if download.is_err() {
        // Unblock the upload half before joining it on a failed download.
        let _ = incoming.shutdown(Shutdown::Read);
        let _ = outgoing.shutdown(Shutdown::Both);
    }
    let upload = upload
        .join()
        .map_err(|_| anyhow!("relay upload worker panicked"))?;
    download?;
    upload?;
    Ok(())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests;
