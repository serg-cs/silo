# Silo

**Project-scoped Linux workspaces for coding agents and developer tools on macOS.**

Silo gives each project a clean container workspace without making every session disposable. Your source stays available, chosen state persists, and multiple tools can work in the same project container at once.

> [!IMPORTANT]
> **[Read the Silo documentation](https://serg-cs.github.io/silo/)** for getting started, configuration, concepts, the command reference, and troubleshooting.

## Why Silo?

- One stable, shared workspace per project
- An agent-ready image with practical development tooling
- A stable runtime base for project-specific tool images
- Explicit persistent, shared, and host-mounted state
- One-shot isolated sessions when a clean environment matters
- Simple project and user-level configuration

## Quick start

Silo requires an Apple silicon Mac running macOS 26 or later, [Apple container](https://github.com/apple/container) 1.0.0 or later, and [Rust](https://www.rust-lang.org/tools/install) 1.91 or newer. [Getting started](https://serg-cs.github.io/silo/docs/) covers the image build and the first workspace.

```sh
cargo install --git https://github.com/serg-cs/silo --locked
cd ~/code/my-project
silo image build
silo run
```

Run `silo image build` again after upgrading Silo so the image matches the
installed binary.

## License

[MIT](LICENSE)
