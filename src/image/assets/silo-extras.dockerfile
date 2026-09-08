FROM silo-base:latest

# ------------------------------------------------------------------------------
# Default Silo extras
#
# Keep the base focused on the runtime contract. This layer is a small
# agent-ready workstation. A configured image.dockerfile replaces it entirely;
# it is not inherited.
# ------------------------------------------------------------------------------

USER silo

RUN brew install \
        bat \
        claude-code \
        codex \
        fd \
        fzf \
        gh \
        jj \
        jq \
        node \
        ripgrep \
        tmux \
        vim \
    && brew cleanup --prune=all \
    && rm -rf "$(brew --cache)"

USER root
