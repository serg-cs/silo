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
    LC_ALL=C.UTF-8 \
    PLAYWRIGHT_BROWSERS_PATH=/home/silo/.cache/ms-playwright
ENV PATH="${BREW_PREFIX}/bin:${BREW_PREFIX}/sbin:${PATH}"

# ---- System packages (apt) ---------------------------------------------------
# brew prerequisites + cargo build deps.
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        file \
        git \
        libssl-dev \
        pkg-config \
        python3 \
        sudo \
        util-linux \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*

# ---- User --------------------------------------------------------------------
# Temporary build-time sudo lets Playwright install system dependencies. The
# grant is removed before the image is finalized and restored only on request.
RUN useradd --create-home --shell /bin/bash silo \
    && groupadd --system sshd \
    && useradd \
        --system \
        --gid sshd \
        --home-dir /home/linuxbrew/.linuxbrew/var/lib/sshd \
        --no-create-home \
        --shell /usr/sbin/nologin \
        sshd \
    && printf '%s\n' 'silo ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/silo \
    && chmod 0440 /etc/sudoers.d/silo

USER silo

# ---- Homebrew ----------------------------------------------------------------
# brew refuses to run as root, so silo installs (and owns) it.
RUN NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" \
    && rm -rf "$(brew --cache)"

# ---- Homebrew packages (formulae & casks) ----------------------------------
RUN brew install \
        actionlint \
        antigravity-cli \
        bat \
        claude-code \
        codex \
        copilot-cli \
        esengine/reasonix/reasonix \
        fd \
        fish \
        fzf \
        gh \
        helix \
        jj \
        jq \
        just \
        lazygit \
        node \
        nushell \
        opencode \
        openssh \
        pi-coding-agent \
        playwright-cli \
        python \
        qwen-code \
        ripgrep \
        ruff \
        rust \
        rust-analyzer \
        shellcheck \
        tmux \
        uv \
        vim \
        yazi \
        yamllint \
        zoxide \
        zsh \
    && brew cleanup --prune=all \
    && rm -rf "$(brew --cache)"

# ---- Agent browser tooling ---------------------------------------------------
# Preinstall Chromium and its Linux libraries for browser inspection,
# screenshots, and interactive agent sessions without a first-run download.
RUN playwright-cli install-browser --with-deps \
    && sudo apt-get clean \
    && sudo rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* \
    && sudo rm -f /etc/sudoers.d/silo

# ---- Entrypoint / shell ------------------------------------------------------
# Starts as root only to remap silo; drops to silo and execs the command
# (Zsh by default) as PID 1.
USER root
RUN usermod --shell "${BREW_PREFIX}/bin/zsh" silo \
    && passwd --delete silo \
    && install -d -m 0755 /run/sshd \
    && install -d -o root -g root -m 0755 "${BREW_PREFIX}/var/lib/sshd" \
    && for shell in \
        /bin/bash \
        "${BREW_PREFIX}/bin/zsh" \
        "${BREW_PREFIX}/bin/fish" \
        "${BREW_PREFIX}/bin/nu"; do \
        grep -qxF "$shell" /etc/shells || printf '%s\n' "$shell" >> /etc/shells; \
    done

COPY silo-supervisor.sh /usr/local/bin/silo-supervisor
COPY silo-session.sh /usr/local/bin/silo-session
COPY silo-reserve.sh /usr/local/bin/silo-reserve
COPY silo-status.sh /usr/local/bin/silo-status
COPY silo-stop-guard.sh /usr/local/bin/silo-stop-guard
COPY silo-sshd_config /etc/ssh/silo_sshd_config
RUN chmod 0755 \
        /usr/local/bin/silo-supervisor \
        /usr/local/bin/silo-session \
        /usr/local/bin/silo-reserve \
        /usr/local/bin/silo-status \
        /usr/local/bin/silo-stop-guard

RUN cat > /usr/local/bin/silo-entrypoint <<'EOF'
#!/bin/sh
set -eu

# Consume the creation-time forwarding marker before starting user processes.
ssh_forwarding="${SILO_INTERNAL_SSH_FORWARDING:-0}"
unset SILO_INTERNAL_SSH_FORWARDING

# Remap silo to the host ids (SILO_UID/SILO_GID, set by `silo run`).
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
EOF
RUN chmod +x /usr/local/bin/silo-entrypoint

WORKDIR /home/silo

ENTRYPOINT ["/usr/local/bin/silo-entrypoint"]
CMD ["zsh"]

EXPOSE 22
