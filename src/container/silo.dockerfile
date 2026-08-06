FROM ubuntu:latest

# ------------------------------------------------------------------------------
# silo development image
#
# Package manager strategy: brew (linuxbrew) is the single package manager for
# every tool. apt exists only for brew's own prerequisites and system-level
# services. Work runs as the non-root `silo` user, remapped at start to the
# host user's ids (see "User" and "Entrypoint").
#
# To add a tool:
#   - brew package:   add to the "Homebrew packages" step.
#   - system service: add to the "System packages" step.
# ------------------------------------------------------------------------------

# ---- Environment -------------------------------------------------------------
# brew's prefix; every tool is symlinked under its bin/. CARGO_HOME/RUSTUP_HOME
# point into the work user's home (see "User" below).
ENV BREW_PREFIX=/home/linuxbrew/.linuxbrew \
    CARGO_HOME=/home/silo/.cargo \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    RUSTUP_HOME=/home/silo/.rustup
ENV PATH="${BREW_PREFIX}/bin:${BREW_PREFIX}/sbin:${CARGO_HOME}/bin:${PATH}"

# ---- System packages (apt) ---------------------------------------------------
# Homebrew-on-Linux prerequisites (curl, git, build-essential, sudo) and C deps
# for cargo builds (libssl-dev, pkg-config).
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        libssl-dev \
        pkg-config \
        sudo \
    && rm -rf /var/lib/apt/lists/* \
    && rm -rf /home/ubuntu

# ---- Homebrew ----------------------------------------------------------------
# brew refuses to run as root, so a dedicated user owns it; its bin/ is on
# root's PATH (Environment above), so all processes see the tools.
RUN useradd --create-home --shell /bin/bash linuxbrew \
    && echo 'linuxbrew ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers \
    && su - linuxbrew -c 'NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"'

# brew tightens its home to 750 after install; the tools below it must stay
# reachable by the `silo` user, so restore world traversal.
RUN chmod 755 /home/linuxbrew

# ---- Homebrew packages -------------------------------------------------------
# Every tool at its latest release; rebuild to update. `su -` resets the
# environment (including root's BREW_PREFIX), so brew's path is explicit.
RUN su - linuxbrew -c 'export PATH="/home/linuxbrew/.linuxbrew/bin:${PATH}"; brew tap anomalyco/tap' \
    && su - linuxbrew -c 'export PATH="/home/linuxbrew/.linuxbrew/bin:${PATH}"; brew install --formula \
        anomalyco/tap/opencode \
        jj \
        node \
        nushell \
        pi-coding-agent \
        rust \
        rust-analyzer' \
    && su - linuxbrew -c 'export PATH="/home/linuxbrew/.linuxbrew/bin:${PATH}"; brew install --cask codex'

# ---- User --------------------------------------------------------------------
# Work happens as the `silo` user (passwordless sudo), never as root. The
# shared project directory is mounted into its home as /home/silo/<name> (see
# "Entrypoint" and `silo run`); the entrypoint remaps `silo` to the host
# user's ids so files stay writable from both sides.
RUN useradd --create-home --shell /bin/bash silo \
    && usermod --shell "${BREW_PREFIX}/bin/nu" silo \
    && echo 'silo ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers

# ---- Entrypoint --------------------------------------------------------------
# Runs as root: remaps `silo` to the host ids (SILO_UID/SILO_GID, set by
# `silo run`; `usermod` re-owns the home directory), then drops privileges and
# execs nushell directly so it stays PID 1 and receives signals. The mount
# target is created by the container runtime.
RUN cat > /usr/local/bin/silo-entrypoint <<'EOF'
#!/bin/sh
set -eu

if [ -n "${SILO_UID:-}" ]; then
    groupmod -o -g "${SILO_GID:-$(id -g silo)}" silo
    usermod -o -u "$SILO_UID" silo
fi

exec setpriv --reuid "$(id -u silo)" --regid "$(id -g silo)" --clear-groups \
    env HOME=/home/silo /home/linuxbrew/.linuxbrew/bin/nu
EOF
RUN chmod +x /usr/local/bin/silo-entrypoint

# ---- Default shell -----------------------------------------------------------
# Start the entrypoint, which drops to the `silo` user and launches nushell.
CMD ["/usr/local/bin/silo-entrypoint"]
