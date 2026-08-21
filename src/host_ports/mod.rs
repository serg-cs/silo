//! Restricted access to selected host loopback ports.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::net::Ipv4Addr;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, ensure};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::digest::hex as hex_digest;
use crate::storage::{
    Lock, effective_uid, ensure_owned_private_directory, ensure_private_directory, open_owned_file,
    protect_file, state_root_from_env,
};

const SSH_BIN: &str = "ssh";
const SSH_KEYGEN_BIN: &str = "ssh-keygen";
const SSH_IDENTITY_ENV: &str = "SILO_SSH_IDENTITY_FILE";
const SSH_KNOWN_HOSTS_ENV: &str = "SILO_SSH_KNOWN_HOSTS_FILE";
const HOST_PORTS_STATE_DIR: &str = "host-ports";
const IDENTITY_DIR: &str = "identity";
const PROJECTS_DIR: &str = "projects";
const CLIENT_KEY_NAME: &str = "id_ed25519";
const HOST_KEY_NAME: &str = "ssh_host_ed25519_key";
const AUTHORIZED_KEYS_NAME: &str = "authorized_keys";
const KNOWN_HOSTS_NAME: &str = "known_hosts";
const CONTROL_STATE_PARENT: &str = "/tmp";
const CONTROL_STATE_PREFIX: &str = "silo-ssh-";
const DIGEST_HEX_LEN: usize = 24;
const MAX_HOST_PORTS: usize = 256;

pub(crate) fn validate_ports(ports: &BTreeSet<u16>) -> Result<()> {
    for port in ports {
        ensure!(
            *port >= 1024,
            "invalid host port `{port}`: port must be between 1024 and 65535"
        );
    }
    ensure!(
        ports.len() <= MAX_HOST_PORTS,
        "too many host ports: at most {MAX_HOST_PORTS} may be configured"
    );
    Ok(())
}

/// Immutable guest-side SSH material mounted into one shared container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshAssets {
    pub(crate) source: PathBuf,
    pub(crate) ports: BTreeSet<u16>,
}

/// Host-port tunnel prepared for one compatible shared container.
pub(crate) struct Tunnel {
    project_key: String,
    asset_key: String,
    identity: PathBuf,
    known_hosts: PathBuf,
    host_alias: String,
    pub(crate) assets: SshAssets,
}

/// Creates the managed SSH material required by the configured host ports.
pub(crate) fn prepare(ports: &BTreeSet<u16>, project_root: &Path) -> Result<Option<Tunnel>> {
    if ports.is_empty() {
        return Ok(None);
    }

    let state_root = host_ports_state_root()?;
    prepare_at(ports, project_root, &state_root).map(Some)
}

fn prepare_at(ports: &BTreeSet<u16>, project_root: &Path, state_root: &Path) -> Result<Tunnel> {
    ensure_private_directory(state_root, "host-port state directory")?;
    let _state_lock = Lock::acquire(&state_root.join(".lock"), "SSH host-port state")?;
    let identity_dir = state_root.join(IDENTITY_DIR);
    ensure_private_directory(&identity_dir, "SSH identity directory")?;
    let identity = identity_dir.join(CLIENT_KEY_NAME);
    let client_public = ensure_key_pair(&identity, "silo-host-ports")?;

    let project_key = digest_prefix(project_root.as_os_str().as_encoded_bytes());
    let asset_key = asset_key(&client_public, ports);
    let asset_dir = state_root
        .join(PROJECTS_DIR)
        .join(&project_key)
        .join(&asset_key);
    ensure_private_directory(&asset_dir, "SSH host-port asset directory")?;
    let host_key = asset_dir.join(HOST_KEY_NAME);
    let host_public = ensure_key_pair(&host_key, "silo-host-ports-server")?;
    let host_alias = format!("silo-host-ports-{project_key}-{asset_key}");

    // Derive the public authentication files from the validated keys.
    write_managed_file(
        &asset_dir.join(AUTHORIZED_KEYS_NAME),
        authorized_keys(&client_public, ports).as_bytes(),
        0o600,
    )?;
    let known_hosts = asset_dir.join(KNOWN_HOSTS_NAME);
    write_managed_file(
        &known_hosts,
        format!("{host_alias} {host_public}\n").as_bytes(),
        0o600,
    )?;

    Ok(Tunnel {
        project_key,
        asset_key,
        identity,
        known_hosts,
        host_alias,
        assets: SshAssets {
            source: asset_dir,
            ports: ports.clone(),
        },
    })
}

fn host_ports_state_root() -> Result<PathBuf> {
    Ok(state_root_from_env()
        .context("host ports require XDG_STATE_HOME or HOME to be absolute")?
        .join(HOST_PORTS_STATE_DIR))
}

fn ensure_key_pair(private_key: &Path, comment: &str) -> Result<String> {
    let public_key = private_key.with_extension("pub");
    let private_exists = match fs::symlink_metadata(private_key) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not inspect SSH key `{}`", private_key.display()));
        }
    };
    if !private_exists {
        match fs::symlink_metadata(&public_key) {
            Ok(_) => {
                return Err(anyhow!(
                    "managed SSH key `{}` is missing its private half",
                    private_key.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not inspect SSH key `{}`", public_key.display())
                });
            }
        }

        let output = Command::new(SSH_KEYGEN_BIN)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
            .arg(private_key)
            .output()
            .context("could not start ssh-keygen for managed host-port identity")?;
        if !output.status.success() {
            return Err(anyhow!(
                "could not create managed SSH key `{}`: {}",
                private_key.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    let _private_key = open_owned_file(private_key, 0o600, "managed SSH private key")?;

    // Derive the value used by Silo directly from the protected private key.
    let output = Command::new(SSH_KEYGEN_BIN)
        .args(["-y", "-f"])
        .arg(private_key)
        .output()
        .context("could not start ssh-keygen to inspect managed host-port identity")?;
    if !output.status.success() {
        return Err(anyhow!(
            "could not read managed SSH key `{}`: {}",
            private_key.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let public = String::from_utf8(output.stdout).context("managed SSH public key is not UTF-8")?;
    normalize_public_key(public.trim())
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
    let file = temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not install managed SSH file `{}`", path.display()))?;
    protect_file(&file, path, mode, "managed SSH file")
}

fn authorized_keys(public_key: &str, ports: &BTreeSet<u16>) -> String {
    let mut options = vec![
        "restrict".to_string(),
        "port-forwarding".to_string(),
        "command=\"/usr/bin/false\"".to_string(),
    ];
    options.extend(
        ports
            .iter()
            .map(|port| format!("permitlisten=\"127.0.0.1:{port}\"")),
    );
    format!("{} {public_key} silo-host-ports\n", options.join(","))
}

fn asset_key(public_key: &str, ports: &BTreeSet<u16>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key.as_bytes());
    for port in ports {
        hasher.update(port.to_be_bytes());
    }
    hex_digest(hasher.finalize())[..DIGEST_HEX_LEN].to_string()
}

fn digest_prefix(bytes: impl AsRef<[u8]>) -> String {
    hex_digest(Sha256::digest(bytes))[..DIGEST_HEX_LEN].to_string()
}

impl Tunnel {
    /// Reuses or starts the project tunnel after the guest is ready.
    pub(crate) fn ensure(&self, address: Ipv4Addr) -> Result<()> {
        let root = control_state_root();
        ensure_owned_private_directory(&root, "SSH control directory")?;
        let lock_path = root.join(format!("{}.lock", self.project_key));
        let _lock = Lock::acquire(&lock_path, "SSH host-port tunnel")?;
        let socket = control_socket_path(&root, &self.project_key, &self.asset_key, address);
        if master_running(&socket, address) {
            return Ok(());
        }
        remove_stale_control_socket(&socket)?;

        let arguments = ssh_arguments(&socket, &self.host_alias, &self.assets.ports, address);
        let mut command = Command::new(SSH_BIN);
        command.args(&arguments);
        set_ssh_path_environment(&mut command, &self.identity, &self.known_hosts);
        let output = command
            .output()
            .context("could not start SSH host-port tunnel")?;
        if !output.status.success() {
            return Err(anyhow!(
                "could not establish SSH host-port tunnel: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if !master_running(&socket, address) {
            return Err(anyhow!("SSH host-port tunnel exited before becoming ready"));
        }
        Ok(())
    }
}

fn control_state_root() -> PathBuf {
    Path::new(CONTROL_STATE_PARENT).join(format!("{CONTROL_STATE_PREFIX}{}", effective_uid()))
}

fn control_socket_path(
    root: &Path,
    project_key: &str,
    asset_key: &str,
    address: Ipv4Addr,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(project_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(asset_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(address.octets());
    let key = hex_digest(hasher.finalize());
    root.join(format!("{}.sock", &key[..DIGEST_HEX_LEN]))
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
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == effective_uid() => {
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

#[cfg(test)]
mod tests;
