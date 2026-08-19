#!/bin/sh
set -eu

# Consume the creation-time forwarding marker before starting user processes.
ssh_forwarding=${SILO_INTERNAL_SSH_FORWARDING:-0}
unset SILO_INTERNAL_SSH_FORWARDING

# Remap silo to the host ids supplied by Silo.
if [ -n "${SILO_UID:-}" ]; then
    usermod -o -u "$SILO_UID" silo
fi
if [ -n "${SILO_GID:-}" ]; then
    groupmod -o -g "$SILO_GID" silo
fi

# Sudo is installed for opt-in sessions but grants no access by default.
rm -f /etc/sudoers.d/silo
if [ "${SILO_SUDO:-0}" = 1 ]; then
    printf '%s\n' 'silo ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/silo
    chmod 0440 /etc/sudoers.d/silo
fi

# Runtime-only coordination belongs to the container, not the host config.
rm -rf /run/silo
install -d -o silo -g silo -m 0700 \
    /run/silo /run/silo/reservations /run/silo/sessions

# Re-own image-layer home contents without crossing configured mounts.
find /home/silo -xdev \
    \( -type d -exec mountpoint -q {} \; -prune \) -o \
    -exec chown -h silo:silo {} +

# Re-own the brew prefix if it belongs to a stale uid (one-time per host uid).
if [ "$(stat -c %u /home/linuxbrew/.linuxbrew)" != "$(id -u silo)" ]; then
    chown -R silo:silo /home/linuxbrew
fi
install -d -o root -g root -m 0755 \
    /run/sshd "${BREW_PREFIX}/var/lib/sshd"

# Start the restricted tunnel server only when container creation enabled it.
if [ "$ssh_forwarding" = 1 ]; then
    "${BREW_PREFIX}/sbin/sshd" -t -f /etc/ssh/silo_sshd_config
    "${BREW_PREFIX}/sbin/sshd" -f /etc/ssh/silo_sshd_config -E /run/silo/sshd.log
fi
touch /run/silo/ready

exec setpriv --reuid "$(id -u silo)" --regid "$(id -g silo)" --init-groups \
    env HOME=/home/silo "$@"
