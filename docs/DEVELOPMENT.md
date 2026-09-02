# Development

Ködade CLI is a Rust workspace with three crates:

- `kodade-cli-proto` owns the shared client and server message types and JSON
  encoding/decoding.
- `kodade-cli-daemon` owns sessions, PTYs, terminal parsing, screen state,
  agent detection, and the Unix socket server. Its current modules include
  `agent.rs`, `manifest.rs`, `layout.rs`, `proc.rs`, and `persist.rs`.
- `kodade-cli` owns the `kodade-cli` binary and its thin ratatui/crossterm TUI.
  Its current modules are `cli.rs`, `app.rs`, `config.rs`, `mode.rs`,
  `render.rs`, `input.rs`, and `commands.rs`. `cli.rs` holds the clap
  definitions, `main.rs` only dispatches them, and `app.rs` holds the attached
  client's `App` state plus its key, mouse, layout, and draw handlers.

The daemon runs the user's shell as a login shell and keeps the PTY alive when
a client disconnects. The client connects to the daemon, forwards input and
resize events, renders screen updates, and also provides the scripting
subcommands.

## Daemon model: workspace roots and cwd inheritance

A workspace can carry a `root` directory. When the daemon spawns a pane it
resolves the working directory in this order: an explicit cwd, then the focused
pane's live cwd in the same workspace, then the workspace root, then the
daemon's own cwd. `proc.rs` reads a pane's foreground leader (via the PTY's
process-group leader) to get its live cwd and command line — Linux reads
`/proc/<pid>/cwd`, macOS shells out to `lsof`, and both are cached on the pane
with the same 2-second cadence as the process name. Commands from `run`/`split`
always execute through the login shell (`$SHELL -lc 'exec …'`, arguments
single-quoted) so agent CLIs keep their environment and never proxy
credentials. `NewPane` opens a new tab when `split` is `None`, otherwise it
splits the focused pane; the new pane becomes focused so the reply snapshot
identifies it.

## Session persistence and restore

The daemon persists a session's layout so a restart (logout, crash, or
`kill -TERM`) restores it. `persist.rs` writes a versioned JSON file to:

1. `$XDG_STATE_HOME/kodade-cli/sessions/SESSION.json` when `XDG_STATE_HOME` is
   set.
2. On macOS, `~/Library/Application Support/kodade-cli/sessions/SESSION.json`.
3. On other platforms, `~/.local/state/kodade-cli/sessions/SESSION.json`.

The file records `"version": 1`, the active workspace, and every workspace →
tab → pane: names, roots, zoom, the pane tree, each pane's title, its live cwd,
and the command it was spawned with. **Scrollback is never persisted** (secrets
risk); only layout and metadata are.

Writes are debounced ~500 ms and driven by a `layout_generation` counter that
only layout-changing mutations advance — PTY output never triggers a write. The
file is written atomically (temp file + rename). SIGTERM flushes a final save;
an explicit `kill-session` deletes the file so a stopped session is not revived.

On a cold start (no live daemon owns the socket) the daemon rebuilds the layout
with **fresh panes**: each pane spawns in its saved cwd, falling back to the
workspace root and then the pane default. Pane, tab, and workspace ids are
re-allocated. Restored panes start as plain shells unless `[session]
resume_agents` is enabled and a saved command matches an agent manifest that
defines a `resume` string, in which case that resume command runs instead.

A file with an unknown `version` or a parse/validation error never crashes the
daemon: it is renamed to `SESSION.json.broken`, a warning is logged to stderr,
and a clean session starts. Unknown fields are ignored. A restored session
reports `restored: true` in its layout snapshot (and `kodade-cli ls` prints
`(restored)`) until the first client attaches.

## Client and daemon protocol

The client and daemon communicate with newline-delimited JSON over one Unix
socket per session. Each message is one UTF-8 JSON value followed by a newline;
PTY input bytes are represented by serde JSON byte arrays. The shared protocol
types live in `kodade-cli-proto`.

Pane contents travel in a `Screen`: a plain `contents` string (used by copy
mode and `pane read`) plus `rows`, one styled run list per visible terminal
row. A `Run` is a stretch of adjacent cells sharing foreground color,
background color, and attribute bits (bold, italic, underline, dim, inverse);
colors are `Default`, `Indexed(u8)`, or `Rgb`. Wide characters are emitted once
and their continuation cell dropped, so a run's display width equals the
columns it covers. `Screen` also carries the cursor position and visibility and
the pane's bracketed-paste and mouse-reporting modes.

Socket paths are selected in this order:

1. `$XDG_RUNTIME_DIR/kodade-cli/SESSION.sock` when `XDG_RUNTIME_DIR` is set.
2. On macOS, `/tmp/kodade-cli-$UID/SESSION.sock`.
3. On other platforms, `$HOME/.local/state/kodade-cli/SESSION.sock` when a
   home directory is available; otherwise `/tmp/kodade-cli-$UID/SESSION.sock`.

Session names must be non-empty path components. They cannot contain `/` or be
`.` or `..`. The daemon removes a socket file only when it cannot connect to a
live daemon at that path.

## Checks

Run the local gate from the repository root:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

CI mirrors these checks on `ubuntu-latest` and `macos-latest`. CI runs
`cargo fmt --check`, the same clippy command, `cargo build`, and `cargo test`.

See [RELEASING.md](RELEASING.md) for versioning, tagging, release artifacts,
and installer details.
