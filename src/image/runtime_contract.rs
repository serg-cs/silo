//! Runtime contract embedded into every Silo container image.

use std::process::Command;

pub(crate) const ENTRYPOINT: &str = include_str!("assets/silo-entrypoint.sh");
pub(crate) const LIFECYCLE: &str = include_str!("assets/silo-lifecycle.sh");
pub(crate) const SSHD_CONFIG: &str = include_str!("assets/silo-sshd_config");

pub(crate) const ENTRYPOINT_COMMAND: &str = "/usr/local/bin/silo-entrypoint";
pub(crate) const LIFECYCLE_COMMAND: &str = "/usr/local/bin/silo-lifecycle";
pub(crate) const CONTAINER_HOME: &str = "/home/silo";
pub(crate) const GUEST_READY_PATH: &str = "/run/silo/ready";
pub(crate) const CONTAINER_RUNTIME_DIR: &str = "/run/silo";
pub(crate) const BREW_PREFIX: &str = "/home/linuxbrew/.linuxbrew";
pub(crate) const BASH_PATH: &str = "/bin/bash";
pub(crate) const ZSH_PATH: &str = "/home/linuxbrew/.linuxbrew/bin/zsh";
pub(crate) const FISH_PATH: &str = "/home/linuxbrew/.linuxbrew/bin/fish";
pub(crate) const NU_PATH: &str = "/home/linuxbrew/.linuxbrew/bin/nu";

pub(crate) struct RuntimeAsset {
    pub(crate) build_arg: &'static str,
    pub(crate) image_path: &'static str,
    pub(crate) contents: &'static str,
}

pub(crate) const RUNTIME_ASSETS: &[RuntimeAsset] = &[
    RuntimeAsset {
        build_arg: "SILO_INTERNAL_ASSET_ENTRYPOINT",
        image_path: ENTRYPOINT_COMMAND,
        contents: ENTRYPOINT,
    },
    RuntimeAsset {
        build_arg: "SILO_INTERNAL_ASSET_LIFECYCLE",
        image_path: LIFECYCLE_COMMAND,
        contents: LIFECYCLE,
    },
    RuntimeAsset {
        build_arg: "SILO_INTERNAL_ASSET_SSHD_CONFIG",
        image_path: "/etc/ssh/silo_sshd_config",
        contents: SSHD_CONFIG,
    },
];

/// Makes Silo's contract authoritative over derivative-image defaults.
pub(crate) fn append_runtime_contract(command: &mut Command, sudo: bool, host_ports: bool) {
    command
        .args(["--user", "root", "--entrypoint", ENTRYPOINT_COMMAND])
        .arg("--env")
        .arg(format!("BREW_PREFIX={BREW_PREFIX}"))
        .arg("--env")
        .arg(format!("SILO_RUNTIME_DIR={CONTAINER_RUNTIME_DIR}"))
        .arg("--env")
        .arg(format!("SILO_SUDO={}", u8::from(sudo)))
        .arg("--env")
        .arg(format!("SILO_INTERNAL_HOST_PORTS={}", u8::from(host_ports)));
}
