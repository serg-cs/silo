use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write as _};
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::config::{Config, Forward};

const SSH_BIN: &str = "ssh";
const SSH_KEYGEN_BIN: &str = "ssh-keygen";
const SSH_IDENTITY_ENV: &str = "SILO_SSH_IDENTITY_FILE";
const SSH_KNOWN_HOSTS_ENV: &str = "SILO_SSH_KNOWN_HOSTS_FILE";
const FORWARD_STATE_DIR: &str = "forward";
const IDENTITY_DIR: &str = "identity";
const PROJECTS_DIR: &str = "projects";
const CLIENT_KEY_NAME: &str = "id_ed25519";
const HOST_KEY_NAME: &str = "ssh_host_ed25519_key";
const AUTHORIZED_KEYS_NAME: &str = "authorized_keys";
const KNOWN_HOSTS_NAME: &str = "known_hosts";
const CONTROL_STATE_PARENT: &str = "/tmp";
const CONTROL_STATE_PREFIX: &str = "silo-ssh-";
const DARWIN_UNIX_PATH_MAX: usize = 104;
const DIGEST_HEX_LEN: usize = 24;

/// Immutable guest-side SSH material mounted into one shared container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuestAssets {
    source: PathBuf,
    ports: BTreeSet<u16>,
}

impl GuestAssets {
    pub(crate) fn source(&self) -> &Path {
        &self.source
    }

    pub(crate) fn ports(&self) -> &BTreeSet<u16> {
        &self.ports
    }

    #[cfg(test)]
    pub(crate) fn new(source: PathBuf, ports: BTreeSet<u16>) -> Self {
        Self { source, ports }
    }
}

/// Host forwarding state prepared for one compatible shared-container run.
pub(crate) struct Session {
    tunnel: Option<Tunnel>,
}

struct Tunnel {
    project_key: String,
    asset_key: String,
    identity: PathBuf,
    known_hosts: PathBuf,
    host_alias: String,
    guest: GuestAssets,
}

impl Session {
    /// Creates the managed key material required by the enabled forward set.
    pub(crate) fn prepare(config: &Config, project_root: &Path) -> Result<Self> {
        let enabled = enabled_forwards(&config.forward);
        let ports: BTreeSet<_> = enabled.values().map(|forward| forward.port).collect();
        if ports.is_empty() {
            return Ok(Self { tunnel: None });
        }

        let state_root = forward_state_root()?;
        let _state_lock = FileLock::acquire(&state_root.join(".lock"), "SSH forwarding state")?;
        let identity_dir = state_root.join(IDENTITY_DIR);
        ensure_private_directory(&identity_dir, "SSH identity directory")?;
        let identity = identity_dir.join(CLIENT_KEY_NAME);
        let client_public = ensure_key_pair(&identity, "silo-forward")?;

        let project_key = digest_prefix(project_root.as_os_str().as_encoded_bytes());
        let asset_key = asset_key(project_root, &client_public, &ports);
        let asset_dir = state_root
            .join(PROJECTS_DIR)
            .join(&project_key)
            .join(&asset_key);
        ensure_private_directory(&asset_dir, "SSH forwarding asset directory")?;
        let host_key = asset_dir.join(HOST_KEY_NAME);
        let host_public = ensure_key_pair(&host_key, "silo-forward-host")?;
        let host_alias = format!("silo-forward-{project_key}-{asset_key}");

        // Derive the public authentication files from the validated keys.
        write_managed_file(
            &asset_dir.join(AUTHORIZED_KEYS_NAME),
            authorized_keys(&client_public, &ports).as_bytes(),
            0o600,
        )?;
        let known_hosts = asset_dir.join(KNOWN_HOSTS_NAME);
        write_managed_file(
            &known_hosts,
            format!("{host_alias} {host_public}\n").as_bytes(),
            0o600,
        )?;

        Ok(Self {
            tunnel: Some(Tunnel {
                project_key,
                asset_key,
                identity,
                known_hosts,
                host_alias,
                guest: GuestAssets {
                    source: asset_dir,
                    ports,
                },
            }),
        })
    }

    pub(crate) fn guest(&self) -> Option<&GuestAssets> {
        self.tunnel.as_ref().map(|tunnel| &tunnel.guest)
    }

    pub(crate) const fn requires_address(&self) -> bool {
        self.tunnel.is_some()
    }

    /// Reuses or starts the project tunnel after the guest is ready.
    pub(crate) fn ensure_tunnel(&self, address: Ipv4Addr) -> Result<()> {
        let Some(tunnel) = &self.tunnel else {
            return Ok(());
        };
        tunnel.ensure(address)
    }
}

fn enabled_forwards(forwards: &BTreeMap<String, Forward>) -> BTreeMap<String, Forward> {
    forwards
        .iter()
        .filter(|(_, entry)| entry.is_enabled())
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect()
}

fn forward_state_root() -> Result<PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .filter(|path| Path::new(path).is_absolute() && !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| Path::new(path).is_absolute() && !path.is_empty())
                .map(|path| PathBuf::from(path).join(".local/state"))
        })
        .ok_or_else(|| anyhow!("SSH forwarding requires XDG_STATE_HOME or HOME to be absolute"))?;
    let root = state_home.join("silo").join(FORWARD_STATE_DIR);
    ensure_private_directory(&root, "SSH forwarding state directory")?;
    Ok(root)
}

fn ensure_private_directory(path: &Path, description: &str) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("could not create {description} `{}`", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {description} `{}`", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(anyhow!(
            "refusing to use unsafe {description} `{}`",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect {description} `{}`", path.display()))
}

fn ensure_key_pair(private_key: &Path, comment: &str) -> Result<String> {
    let public_key = private_key.with_extension("pub");
    let private_exists = private_key
        .try_exists()
        .with_context(|| format!("could not inspect SSH key `{}`", private_key.display()))?;
    let public_exists = public_key
        .try_exists()
        .with_context(|| format!("could not inspect SSH key `{}`", public_key.display()))?;
    if !private_exists && public_exists {
        return Err(anyhow!(
            "managed SSH key `{}` is missing its private half",
            private_key.display()
        ));
    }
    if !private_exists {
        let output = Command::new(SSH_KEYGEN_BIN)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
            .arg(private_key)
            .output()
            .context("could not start ssh-keygen for managed forwarding identity")?;
        if !output.status.success() {
            return Err(anyhow!(
                "could not create managed SSH key `{}`: {}",
                private_key.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    validate_private_file(private_key, 0o600, "managed SSH private key")?;

    // Re-derive the public half so a stale or missing .pub file cannot drift.
    let output = Command::new(SSH_KEYGEN_BIN)
        .args(["-y", "-f"])
        .arg(private_key)
        .output()
        .context("could not start ssh-keygen to inspect managed forwarding identity")?;
    if !output.status.success() {
        return Err(anyhow!(
            "could not read managed SSH key `{}`: {}",
            private_key.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let public = String::from_utf8(output.stdout).context("managed SSH public key is not UTF-8")?;
    let public = normalize_public_key(public.trim())?;
    write_managed_file(
        &public_key,
        format!("{public} {comment}\n").as_bytes(),
        0o644,
    )?;
    Ok(public)
}

fn validate_private_file(path: &Path, mode: u32, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {description} `{}`", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(anyhow!(
            "refusing to use unsafe {description} `{}`",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("could not protect {description} `{}`", path.display()))
}

fn normalize_public_key(public: &str) -> Result<String> {
    if public.lines().count() != 1 {
        return Err(anyhow!("managed SSH identity is not an Ed25519 public key"));
    }

    let mut fields = public.split_whitespace();
    let key_type = fields.next();
    let Some(body) = fields.next() else {
        return Err(anyhow!("managed SSH identity is not an Ed25519 public key"));
    };
    if key_type != Some("ssh-ed25519") {
        return Err(anyhow!("managed SSH identity is not an Ed25519 public key"));
    }
    Ok(format!("ssh-ed25519 {body}"))
}

fn write_managed_file(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("managed SSH path `{}` has no parent", path.display()))?;
    let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "could not create managed SSH file near `{}`",
            path.display()
        )
    })?;
    temporary
        .write_all(contents)
        .with_context(|| format!("could not write managed SSH file `{}`", path.display()))?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not install managed SSH file `{}`", path.display()))?;
    validate_private_file(path, mode, "managed SSH file")
}

fn authorized_keys(public_key: &str, ports: &BTreeSet<u16>) -> String {
    let mut options = vec![
        "command=\"/usr/bin/false\"".to_string(),
        "no-agent-forwarding".to_string(),
        "no-X11-forwarding".to_string(),
        "no-pty".to_string(),
        "no-user-rc".to_string(),
    ];
    options.extend(
        ports
            .iter()
            .map(|port| format!("permitlisten=\"127.0.0.1:{port}\"")),
    );
    format!("{} {public_key} silo-forward\n", options.join(","))
}

fn asset_key(project_root: &Path, public_key: &str, ports: &BTreeSet<u16>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_root.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(public_key.as_bytes());
    for port in ports {
        hasher.update(port.to_be_bytes());
    }
    digest_prefix(hasher.finalize())
}

fn digest_prefix(bytes: impl AsRef<[u8]>) -> String {
    hex_digest(Sha256::digest(bytes))[..DIGEST_HEX_LEN].to_string()
}

impl Tunnel {
    fn ensure(&self, address: Ipv4Addr) -> Result<()> {
        let root = control_state_root();
        ensure_private_directory(&root, "SSH control directory")?;
        let lock_path = root.join(format!("{}.lock", self.project_key));
        let _lock = FileLock::acquire(&lock_path, "SSH tunnel")?;
        let socket = control_socket_path(&root, &self.project_key, &self.asset_key, address);
        validate_control_socket_path(&socket, &root)?;
        if master_running(&socket, address) {
            return Ok(());
        }
        remove_stale_control_socket(&socket)?;

        let arguments = ssh_arguments(&socket, &self.host_alias, self.guest.ports(), address);
        let mut command = Command::new(SSH_BIN);
        command.args(&arguments);
        set_ssh_path_environment(&mut command, &self.identity, &self.known_hosts);
        let output = command
            .output()
            .context("could not start SSH host-forward tunnel")?;
        if !output.status.success() {
            return Err(anyhow!(
                "could not establish SSH host-forward tunnel: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if !master_running(&socket, address) {
            return Err(anyhow!(
                "SSH host-forward tunnel exited before becoming ready"
            ));
        }
        Ok(())
    }
}

fn control_state_root() -> PathBuf {
    Path::new(CONTROL_STATE_PARENT).join(format!("{CONTROL_STATE_PREFIX}{}", unsafe {
        libc::geteuid()
    }))
}

fn control_socket_path(
    root: &Path,
    project_key: &str,
    asset_key: &str,
    address: Ipv4Addr,
) -> PathBuf {
    root.join(format!("{project_key}-{asset_key}-{address}.sock"))
}

fn validate_control_socket_path(path: &Path, root: &Path) -> Result<()> {
    let valid_parent = path.parent() == Some(root);
    let valid_name = path.extension() == Some(OsStr::new("sock"));
    let valid_length = path.as_os_str().as_encoded_bytes().len() < DARWIN_UNIX_PATH_MAX;
    if !path.is_absolute() || !valid_parent || !valid_name || !valid_length {
        return Err(anyhow!(
            "refusing unsafe SSH control socket path `{}`",
            path.display()
        ));
    }
    Ok(())
}

fn master_running(socket: &Path, address: Ipv4Addr) -> bool {
    Command::new(SSH_BIN)
        .args(["-F", "/dev/null", "-S"])
        .arg(socket)
        .args(["-O", "check"])
        .arg(format!("silo@{address}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn remove_stale_control_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() } =>
        {
            fs::remove_file(path).with_context(|| {
                format!(
                    "could not remove stale SSH control socket `{}`",
                    path.display()
                )
            })
        }
        Ok(_) => Err(anyhow!(
            "refusing to replace unsafe SSH control socket `{}`",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("could not inspect SSH control socket `{}`", path.display())),
    }
}

fn ssh_arguments(
    socket: &Path,
    host_alias: &str,
    ports: &BTreeSet<u16>,
    address: Ipv4Addr,
) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = ["-F", "/dev/null", "-M", "-f", "-N", "-T", "-S"]
        .into_iter()
        .map(OsString::from)
        .collect();
    arguments.push(socket.into());
    for option in [
        "AddressFamily=inet".to_string(),
        "BatchMode=yes".to_string(),
        "ConnectTimeout=10".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "ForwardAgent=no".to_string(),
        "GlobalKnownHostsFile=/dev/null".to_string(),
        format!("HostKeyAlias={host_alias}"),
        "IdentitiesOnly=yes".to_string(),
        "IdentityAgent=none".to_string(),
        format!("IdentityFile=${{{SSH_IDENTITY_ENV}}}"),
        "ServerAliveCountMax=3".to_string(),
        "ServerAliveInterval=15".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        format!("UserKnownHostsFile=${{{SSH_KNOWN_HOSTS_ENV}}}"),
    ] {
        arguments.push("-o".into());
        arguments.push(option.into());
    }
    for port in ports {
        arguments.push("-R".into());
        arguments.push(format!("127.0.0.1:{port}:127.0.0.1:{port}").into());
    }
    arguments.push(format!("silo@{address}").into());
    arguments
}

fn set_ssh_path_environment(command: &mut Command, identity: &Path, known_hosts: &Path) {
    command
        .env(SSH_IDENTITY_ENV, identity)
        .env(SSH_KNOWN_HOSTS_ENV, known_hosts);
}

struct FileLock {
    file: fs::File,
}

impl FileLock {
    fn acquire(path: &Path, description: &str) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("could not open {description} lock `{}`", path.display()))?;
        validate_private_file(path, 0o600, &format!("{description} lock"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("could not lock {description}"));
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
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
