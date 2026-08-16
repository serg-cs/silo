# Silo

**Project-scoped Linux workspaces for coding agents and developer tools on macOS.**

Silo gives each project a clean container workspace without making every session disposable. Your source stays available, chosen state persists, and multiple tools can work in the same project container at once.

> [!IMPORTANT]
> **[Read the Silo documentation](https://serg-cs.github.io/silo/)** for installation, configuration, concepts, command reference, and troubleshooting.

## Why Silo?

- One stable, shared workspace per project
- An agent-ready image with practical development tooling
- Explicit persistent, shared, and host-mounted state
- One-shot isolated sessions when a clean environment matters
- Simple project and user-level configuration

## Quick start

Silo requires an Apple silicon Mac running macOS 26 or later and Apple's [`container`](https://github.com/apple/container) runtime.

```sh
cargo install --git https://github.com/serg-cs/silo --locked
cd ~/code/my-project
silo image build
silo run
```

For the complete setup guide and current behavior, continue to the **[documentation](https://serg-cs.github.io/silo/docs/)**.

## License

[MIT](LICENSE)
