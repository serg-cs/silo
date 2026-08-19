FROM silo-base:latest

# ------------------------------------------------------------------------------
# Default Silo development extras
#
# Keep the base focused on the runtime contract. This layer preserves the full
# agent, language, browser, editor, and everyday CLI toolset offered by default.
# ------------------------------------------------------------------------------

ENV PLAYWRIGHT_BROWSERS_PATH=/home/linuxbrew/.linuxbrew/var/cache/ms-playwright

# Temporary build-time sudo lets Playwright install its Linux dependencies.
RUN printf '%s\n' 'silo ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/silo \
    && chmod 0440 /etc/sudoers.d/silo

USER silo

# ---- Agents and developer tools ---------------------------------------------
RUN brew install \
        actionlint \
        antigravity-cli \
        bat \
        claude-code \
        codex \
        copilot-cli \
        esengine/reasonix/reasonix \
        fd \
        fzf \
        gh \
        helix \
        jj \
        jq \
        just \
        lazygit \
        node \
        opencode \
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
    && brew cleanup --prune=all \
    && rm -rf "$(brew --cache)"

# ---- Agent browser tooling ---------------------------------------------------
RUN playwright-cli install-browser --with-deps \
    && sudo apt-get clean \
    && sudo rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*

# Runtime sudo remains opt-in through Silo's entrypoint.
USER root
RUN rm -f /etc/sudoers.d/silo
