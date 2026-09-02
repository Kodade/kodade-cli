# kodade-cli

Ködade CLI is a lightweight terminal workspace (TUI multiplexer) for agent
CLIs: workspaces, tabs, and panes with agent-state awareness, mouse support,
and persistent sessions. It is the terminal-native companion to the Ködade
desktop app and follows the herdr layout model. "Done" for v1 is a single
binary that a Ködade user can run over SSH or in any terminal and manage
several agent panes without the desktop app.

## Status
v0.1.0 released (M0–M4 complete: multiplexer, mouse, agent detection, sidebar, scripting CLI, config/themes, copy mode, release pipeline). v0.2.0 milestone opened 2026-09-02 with issues #6–#24 (herdr/desktop parity review: help overlay, colored panes, Ködade theme, persistence, notifications). — last touched 2026-09-02

## Commands
```bash
cargo build
cargo run
cargo test
cargo fmt && cargo clippy
```

## Architecture
Rust workspace: `kodade-cli-proto` (socket types), `kodade-cli-daemon` (PTY host, session state, agent detection), `kodade-cli` (ratatui client + scripting CLI). See docs/DEVELOPMENT.md. v0.2.0 milestone plan and binding design decisions: docs/features/v0-2-parity/PLAN.md.

## Conventions & Gotchas
- Köd[Name] uses the umlaut in prose, docs, comments, and UI strings. ASCII
  only in filenames, identifiers, binary names, and URLs.
- Rust project — the desktop app's "keep Rust thin" rule does not apply here;
  this is all Rust by design.
- Wrap official agent CLIs through the user's login shell; never proxy
  credentials.
- Progressive disclosure: clear defaults, visible state, escape hatches.

## Out of Scope
- KödWork background tasks, GitHub/review integration, KödChat — desktop-app
  surfaces, not CLI surfaces.
- No hosted service, no telemetry, no credential handling.
- Not an estimating tool; general-purpose ADE companion.
