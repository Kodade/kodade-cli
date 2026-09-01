# Ködade CLI

Ködade CLI is a terminal workspace for running agent CLIs such as Claude Code,
Codex, and other programs that run in a terminal. Its layout model follows
herdr: workspaces contain tabs, and tabs contain panes. It is the
terminal-native companion to the [Ködade desktop app](https://github.com/Kodade/kodade).

## Status

Pre-alpha. M0 is complete and M1 is in progress. Today, the CLI starts a
daemon, owns one login-shell PTY, renders it in a thin TUI client, and keeps the
PTY alive when the client detaches. Multiplexer features such as splits, tabs,
workspaces, mouse controls, and agent-state awareness are planned but are not
available yet.

| Milestone | Scope | Status |
|---|---|---|
| M0 | Cargo workspace, daemon/client handshake, one PTY, rendering, keyboard passthrough | Done |
| M1 | Splits, tabs, workspaces, resize, mouse focus/resize/select, prefix keys, detach/reattach | In progress |
| M2 | Agent detection, five states, sidebar rollup, agent subcommands, Claude Code hooks | Planned |
| M3 | Themes, config, navigate mode, menus, scrollback/copy mode, OSC 52 | Planned |
| M4 | CI builds, release binaries, install script, documentation site page | Planned |

## Build and run

Build and run from the repository:

```bash
cargo build
cargo run -p kodade-cli
```

The client starts the daemon automatically when needed. Detach with `ctrl+b`
followed by `d`; the PTY remains owned by the daemon. Reattach by running
`cargo run -p kodade-cli` again. A named session can be selected with
`cargo run -p kodade-cli -- -s SESSION`.

There is no installer or public prebuilt release yet.

## Roadmap

See [docs/PRD.md](docs/PRD.md) for the product requirements and roadmap.

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
