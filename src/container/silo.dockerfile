FROM ubuntu:latest

# ------------------------------------------------------------------------------
# silo development image
#
# brew is the single package manager (apt only for brew prerequisites); one
# user `silo` owns everything. The entrypoint remaps silo to the host ids
# (SILO_UID/SILO_GID) at start. To add a tool, add it to "Homebrew packages".
# ------------------------------------------------------------------------------

# ---- Environment -------------------------------------------------------------
ENV BREW_PREFIX=/home/linuxbrew/.linuxbrew \
    HOMEBREW_NO_ANALYTICS=1 \
    HOMEBREW_NO_AUTO_UPDATE=1 \
    HOMEBREW_NO_ENV_HINTS=1 \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8
ENV PATH="${BREW_PREFIX}/bin:${BREW_PREFIX}/sbin:${PATH}"

# ---- System packages (apt) ---------------------------------------------------
# brew prerequisites + cargo build deps.
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        libssl-dev \
        pkg-config \
        python3 \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# ---- User --------------------------------------------------------------------
# Single user with passwordless sudo.
RUN useradd --create-home --shell /bin/bash silo \
    && printf '%s\n' 'silo ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/silo \
    && chmod 440 /etc/sudoers.d/silo

USER silo

# ---- Homebrew ----------------------------------------------------------------
# brew refuses to run as root, so silo installs (and owns) it.
RUN NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# ---- Homebrew packages (formulae & casks) ----------------------------------
RUN brew install \
        bat \
        codex \
        esengine/reasonix/reasonix \
        fd \
        jj \
        node \
        nushell \
        opencode \
        pi-coding-agent \
        ripgrep \
        rust \
        rust-analyzer \
        yazi \
        zoxide

# ---- Entrypoint / shell ------------------------------------------------------
# Starts as root only to remap silo; drops to silo and execs the command
# (nushell by default) as PID 1.
USER root
RUN usermod --shell "${BREW_PREFIX}/bin/nu" silo \
    && printf '%s\n' "${BREW_PREFIX}/bin/nu" >> /etc/shells

RUN cat > /usr/local/bin/silo-entrypoint <<'EOF'
#!/bin/sh
set -eu

# Remap silo to the host ids (SILO_UID/SILO_GID, set by `silo run`).
if [ -n "${SILO_UID:-}" ]; then
    usermod -o -u "$SILO_UID" silo
fi
if [ -n "${SILO_GID:-}" ]; then
    groupmod -o -g "$SILO_GID" silo
fi

# Re-own the brew prefix if it belongs to a stale uid (one-time per host uid).
if [ "$(stat -c %u /home/linuxbrew/.linuxbrew)" != "$(id -u silo)" ]; then
    chown -R silo:silo /home/linuxbrew
fi

exec setpriv --reuid "$(id -u silo)" --regid "$(id -g silo)" --init-groups \
    env HOME=/home/silo "$@"
EOF
RUN chmod +x /usr/local/bin/silo-entrypoint

WORKDIR /home/silo

ENTRYPOINT ["/usr/local/bin/silo-entrypoint"]
CMD ["nu"]
