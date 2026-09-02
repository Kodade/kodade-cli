# Development

Ködade CLI is a Rust workspace with three crates:

- `kodade-cli-proto` owns the shared client and server message types and JSON
  encoding/decoding (a single `lib.rs`).
- `kodade-cli-daemon` owns sessions, PTYs, terminal parsing, screen state,
  agent detection, and the Unix socket server. Its modules are `lib.rs` (the
  session and socket server), `agent.rs`, `manifest.rs`, `layout.rs`,
  `proc.rs`, `git.rs`, and `persist.rs`.
- `kodade-cli` owns the `kodade-cli` binary and its thin ratatui/crossterm TUI.
  Its modules are `main.rs`, `cli.rs`, `app.rs`, `config.rs`, `mode.rs`,
  `render.rs`, `input.rs`, `commands.rs`, `help.rs`, `keys.rs`, `notify.rs`,
  `overlay.rs`, `paste.rs`, `picker.rs`, `remote.rs`, `selection.rs`,
  `settings.rs`, and `state.rs`. `cli.rs` holds the clap definitions, `main.rs`
  only dispatches them, and `app.rs` holds the attached client's `App` state
  plus its key, mouse, layout, and draw handlers.

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

## Git worktree workspaces (#22)

`git.rs` isolates all git access. Branch labels are pure filesystem reads:
`repo_root` walks up to a `.git` entry, `current_branch` reads `.git/HEAD` (or,
for a linked worktree, follows the `gitdir:` file to the real HEAD), and
`main_worktree_root` follows `commondir` to find a worktree's main repo. These
run on the daemon's 2-second process tick — each `Workspace` caches its `branch`
so no subprocess runs per frame — and the cached branch plus the derived
`parent` (the open workspace whose root is the worktree's main repo) travel on
`WorkspaceInfo` so the sidebar can nest worktrees and dim their branch.

Mutations shell out: `NewWorktreeWorkspace` runs `git worktree add` under
`[worktrees] directory` (default `~/.kodade/worktrees`, read with the same tiny
loader as `[session]`) and opens a `repo:branch` workspace rooted there;
`RemoveWorktreeWorkspace` closes the workspace and, unless `keep`, runs
`git worktree remove`. Removal is only attempted when `main_worktree_root`
resolves, so nothing outside a registered worktree is ever deleted.

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
live daemon at that path. `session rename` renames the socket file in place: the
listener stays bound, so the same daemon and its PTYs answer on the new path.

Every message, its JSON shape, the `Subscribe` event stream, the schema query,
and the environment variables a pane receives are documented in
[SOCKET-API.md](SOCKET-API.md) — that file is the contract for any other client
(the desktop app included), so update it in the same change as a protocol
change.

## Protocol versioning (#23)

`kodade-cli-proto` exports `PROTOCOL_VERSION: u32` (currently `1`). Both ends
check it at attach time so a stale binary fails fast with a clear message
instead of misbehaving. Bump it whenever a client and daemon can no longer
understand each other. The handshake, the `Query(Version)` probe, and the
compatibility rules are documented in
[SOCKET-API.md](SOCKET-API.md#schema-and-versioning).

## Remote mode (#23)

`kodade-cli --remote USER@HOST [-s NAME] [subcommand]` attaches to (or scripts)
a daemon on another host over an SSH-forwarded Unix socket. It is implemented in
`remote.rs` with no new dependencies and no credential handling — everything
runs through the user's own `ssh` (their config, agent, and keys).

Every code path that needs a session socket goes through
`remote::resolve_socket(&cli)`: a local session returns
`kodade_cli_daemon::socket_path(session)` unchanged, while `--remote` sets up
the forward and returns the local end of it, so the TUI and every scripting
subcommand treat it exactly like a local daemon.

The forward is built with a multiplexed control master so the extra round-trips
are cheap:

1. `ssh -o ControlMaster=auto -o ControlPath=<runtime>/kodade-cli/cm-%C
   -o ControlPersist=60 USER@HOST kodade-cli --version` checks the remote
   binary; if it is missing, the install one-liner is printed and the command
   exits `1`.
2. `ssh … kodade-cli daemon NAME` (with `-f`) starts the remote daemon detached
   when one is not already running (a harmless error if it is).
3. `ssh … kodade-cli session path -s NAME` returns the remote socket path.
4. `ssh -N -L <local>:<remote-socket> …` forwards it (OpenSSH Unix-to-Unix
   forwarding) to `<runtime>/kodade-cli/remote-<host>-<NAME>.sock`. The command
   waits up to 10 s for that local socket to accept a connection.

On exit the forwarding process is stopped and the local socket file removed; the
control master lingers (`ControlPersist=60`) so re-running `--remote`
reconnects without a fresh handshake. Because the daemon keeps session state,
dropping the tunnel and re-running `--remote` reattaches — the client's `Hello`
re-sends the terminal size, so a differently-sized terminal simply re-fits.

`--remote HOST session ls|path|kill|rename` runs the verb on the host over the
control connection; `ls` prefixes each line with `host:`. The command builders (`version_args`,
`socket_path_args`, `start_daemon_args`, `tunnel_args`, `run_args`) and the
local socket resolver are unit-tested in `remote.rs`; the live tunnel path is
only exercised against a real host.

Out of scope (phase 2): multiple remotes in one sidebar, and auto-installing
`kodade-cli` on the remote host.

## Checks

Run the local gate from the repository root:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

CI mirrors these checks on `ubuntu-latest` and `macos-latest`. CI runs
`cargo fmt --check`, the same clippy command, `cargo build`, and `cargo test`.

See [RELEASING.md](RELEASING.md) for versioning, tagging, release artifacts,
and installer details.
