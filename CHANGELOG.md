# Changelog

## 0.3.0 — release candidate

This section describes the pending Linux/macOS release. It is not a published
release until its tag, GitHub Release, four archives, and Homebrew formula are
created.

### Workspace and agent flows

- Work with local and saved SSH machines in one workspace, with independent
  client views and reconnects.
- Find commands, agents, projects, and settings from the command center; use
  attention history to return to blocked or completed work.
- Run guarded agent start, read, prompt, and wait commands against the intended
  pane. Native conversation restoration requires an exact integration-reported
  identity; Ködade never guesses a conversation with `--last`.
- Add local plugins, selected-text and URL actions, event hooks, command
  shortcuts, workspace environment variables, and existing Git worktrees.

### Terminal and maintenance

- Replace the terminal parser with its maintained fork and fix one-row, wide
  character, wrapping, and resize crashes.
- Preserve negotiated keyboard modes, modified shortcuts, and key event types
  while retaining legacy input for applications that do not request them.

- Answer terminal color queries from the actual client theme, including cursor
  and all 256 palette entries; preserve theme ownership through reconnects.
- Add bounded Kitty graphics, PNG paste, OSC 8 links, synchronized terminal
  frames, and native local clipboard support with OSC 52 fallback.
- Add Unix live daemon handoff for sessions already running v0.3.0. A failed
  handoff leaves the source daemon and panes running; clients reconnect after a
  successful handoff and do not replay queued input.
- Add verified standalone stable/preview update checks and replacement.

### Platform support

v0.3.0 will publish four standalone archives only: Linux and macOS on arm64 and
x86_64. Native Windows binaries, installation, upgrade, and SSH preparation
are deferred.

### v0.2.1 migration

An existing v0.2.1 daemon cannot live-handoff to v0.3.0. Install the new binary,
leave the v0.2.1 session and its panes running, and start a separately named
v0.3.0 session. Move work when it is safe; do not kill the old session to
migrate it.
