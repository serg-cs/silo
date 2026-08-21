FROM ubuntu:latest AS silo_internal_runtime_base

# ------------------------------------------------------------------------------
# Silo runtime base
#
# This image owns the stable contract required by every Silo lifecycle. Derived
# images may add or replace development tools, but must preserve these users,
# programs, and paths.
# ------------------------------------------------------------------------------

# ---- Environment -------------------------------------------------------------
ENV BREW_PREFIX=/home/linuxbrew/.linuxbrew \
    HOMEBREW_NO_ANALYTICS=1 \
    HOMEBREW_NO_AUTO_UPDATE=1 \
    HOMEBREW_NO_ENV_HINTS=1 \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8
ENV PATH="${BREW_PREFIX}/bin:${BREW_PREFIX}/sbin:${PATH}"

# ---- Runtime and extension prerequisites ------------------------------------
# Homebrew is the extension package manager. apt remains available to derived
# images and supplies the host-identity, locking, and privilege primitives used
# by Silo itself.
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        file \
        findutils \
        git \
        libssl-dev \
        pkg-config \
        procps \
        python3 \
        sudo \
        util-linux \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*

# ---- Users -------------------------------------------------------------------
RUN useradd --create-home --shell /bin/bash silo \
    && groupadd --system sshd \
    && useradd \
        --system \
        --gid sshd \
        --home-dir /home/linuxbrew/.linuxbrew/var/lib/sshd \
        --no-create-home \
        --shell /usr/sbin/nologin \
        sshd \
    && install -d -o silo -g silo -m 0755 /home/linuxbrew

USER silo

# ---- Homebrew and supported shells ------------------------------------------
RUN NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" \
    && brew install \
        fish \
        nushell \
        openssh \
        zsh \
    && brew cleanup --prune=all \
    && rm -rf "$(brew --cache)"

# ---- Silo runtime ------------------------------------------------------------
USER root
RUN usermod --shell "${BREW_PREFIX}/bin/zsh" silo \
    && usermod --password '*' silo \
    && install -d -m 0755 /run/sshd \
    && install -d -o root -g root -m 0755 "${BREW_PREFIX}/var/lib/sshd" \
    && for shell in \
        /bin/bash \
        "${BREW_PREFIX}/bin/zsh" \
        "${BREW_PREFIX}/bin/fish" \
        "${BREW_PREFIX}/bin/nu"; do \
        grep -qxF "$shell" /etc/shells || printf '%s\n' "$shell" >> /etc/shells; \
    done

ARG SILO_INTERNAL_ASSET_ENTRYPOINT
ARG SILO_INTERNAL_ASSET_LIFECYCLE
ARG SILO_INTERNAL_ASSET_SSHD_CONFIG
RUN install -d -o root -g root -m 0755 /etc/ssh /usr/local/bin \
    && printf '%s' "${SILO_INTERNAL_ASSET_ENTRYPOINT}" | base64 --decode > /usr/local/bin/silo-entrypoint \
    && printf '%s' "${SILO_INTERNAL_ASSET_LIFECYCLE}" | base64 --decode > /usr/local/bin/silo-lifecycle \
    && printf '%s' "${SILO_INTERNAL_ASSET_SSHD_CONFIG}" | base64 --decode > /etc/ssh/silo_sshd_config \
    && chmod 0644 /etc/ssh/silo_sshd_config \
    && chmod 0755 \
        /usr/local/bin/silo-entrypoint \
        /usr/local/bin/silo-lifecycle

WORKDIR /home/silo

ENTRYPOINT ["/usr/local/bin/silo-entrypoint"]
CMD ["zsh"]

EXPOSE 22
