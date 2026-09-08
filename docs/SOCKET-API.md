# Socket API

Ködade CLI's daemon speaks newline-delimited JSON over one Unix socket per
session. Anything that can open a Unix socket can drive a session: the `kodade-cli`
binary is only the first client, and the Ködade desktop app is expected to be
the second.

## Framing and transport

- One socket per session. Path resolution:
  1. `$XDG_RUNTIME_DIR/kodade-cli/SESSION.sock` when `XDG_RUNTIME_DIR` is set.
  2. On macOS, `/tmp/kodade-cli-$UID/SESSION.sock`.
  3. Otherwise `$HOME/.local/state/kodade-cli/SESSION.sock`, falling back to
     `/tmp/kodade-cli-$UID/SESSION.sock`.
- Every message is one UTF-8 JSON value followed by `\n`. There is no length
  prefix and no framing beyond the newline; embedded newlines are escaped by
  JSON itself. Client requests are capped at 16 MiB before deserialization;
  partial uploads survive concurrent screen updates.
- Enums use serde's external tagging: a unit variant is a bare string
  (`"ZoomPane"`), a struct variant is a single-key object
  (`{"FocusPaneId":{"id":3}}`), and a newtype variant wraps its payload
  (`{"Query":"Layout"}`).
- Ids (`PaneId`, `TabId`, `WorkspaceId`) are plain integers on the wire.
- PTY bytes travel as JSON arrays of numbers (`{"Input":{"bytes":[108,115,13]}}`).
- The daemon answers every request on the same connection, in order. Most
  messages are answered with a `Layout` snapshot; the exceptions are listed
  below.
- Each connection that sends `Hello` owns its selected workspace, selected tab, focused pane, and
  scrollback offsets. These are transient client-view state: closing a socket
  drops them and never changes another connection's view or the persisted
  session default.
- Unknown fields are ignored on read, so a newer daemon can add fields without
  breaking an older client. An unknown *variant* is a decode error and closes
  the connection.

Connect, mutate, read the reply:

```bash
printf '%s\n' '{"Query":"Layout"}' | nc -U /tmp/kodade-cli-$UID/default.sock
```

## Client messages

| Message | Example | Reply |
| --- | --- | --- |
| `Query(Layout)` | `{"Query":"Layout"}` | `Layout` |
| `Query(Pane)` | `{"Query":{"Pane":3}}` | `Pane` |
| `Query(Image)` | `{"Query":{"Image":{"pane":3,"id":7,"revision":1}}}` | `Image { pane, image }` |
| `PasteImage` | `{"PasteImage":{"pane":3,"data":"base64 PNG"}}` | `ImagePasted { pane, path }` |
| `Query(Session)` | `{"Query":"Session"}` | `Session` |
| `Query(Version)` | `{"Query":"Version"}` | `Version` |
| `Query(Schema)` | `{"Query":"Schema"}` | `Schema` |
| `Subscribe` | `"Subscribe"` | `Layout`, then `Event`s |
| `ApplyLayout` | `{"ApplyLayout":{"version":1,…}}` | `Layout` |
| `Hello` | `{"Hello":{"cols":120,"rows":40,"version":1}}` | `Welcome` + `Layout` |
| `Input` | `{"Input":{"bytes":[108,115,13]}}` | `Layout` |
| `Resize` | `{"Resize":{"cols":120,"rows":40}}` | `Layout` |
| `SplitRight` / `SplitDown` | `"SplitRight"` | `Layout` |
| `ClosePane` | `"ClosePane"` | `Layout` |
| `CloseTab` | `{"CloseTab":{"id":2}}` | `Layout` |
| `CloseWorkspace` | `{"CloseWorkspace":{"id":1}}` | `Layout` |
| `FocusPane` | `{"FocusPane":{"direction":"Left"}}` | `Layout` |
| `FocusPaneId` | `{"FocusPaneId":{"id":3}}` | `Layout` |
| `FocusPaneCycle` | `{"FocusPaneCycle":{"forward":true}}` | `Layout` |
| `SendToPane` | `{"SendToPane":{"id":3,"bytes":[121,13]}}` | `Layout` |
| `PromptAgent` | `{"PromptAgent":{"pane":3,"expected_agent":"Codex","expected_generation":1,"bytes":[121]}}` | `Pane` |
| `RenamePane` / `RenameTab` / `RenameWorkspace` | `{"RenameTab":{"name":"agents"}}` | `Layout` |
| `RenamePaneId` / `RenameTabId` / `RenameWorkspaceId` | `{"RenameTabId":{"id":2,"name":"agents"}}` | `Layout` |
| `RenameSession` | `{"RenameSession":{"name":"work"}}` | `Layout` |
| `KillSession` | `"KillSession"` | `Shutdown` |
| `NewTab` | `"NewTab"` | `Layout` |
| `NextTab` / `PrevTab` | `"NextTab"` | `Layout` |
| `SelectTab` | `{"SelectTab":{"id":2}}` | `Layout` |
| `SelectTabIndex` | `{"SelectTabIndex":{"index":1}}` | `Layout` |
| `MoveTab` | `{"MoveTab":{"delta":1}}` | `Layout` |
| `MovePaneToTab` | `{"MovePaneToTab":{"pane":3,"tab":2}}` | `Layout` |
| `SetWorkspaceColor` | `{"SetWorkspaceColor":{"id":1,"color":"#e7a33b"}}` | `Layout` |
| `NewWorktreeWorkspace` | `{"NewWorktreeWorkspace":{"repo_root":"/src/repo","branch":"feat-a","from":"main","path":null}}` | `Layout` |
| `OpenWorktreeWorkspace` | `{"OpenWorktreeWorkspace":{"repo_root":"/src/repo","path":"../linked"}}` | `Layout` |
| `RemoveWorktreeWorkspace` | `{"RemoveWorktreeWorkspace":{"id":9,"keep":false}}` | `Layout` |
| `SwapPane` | `{"SwapPane":{"direction":"Right"}}` | `Layout` |
| `BreakPane` | `"BreakPane"` | `Layout` |
| `EqualizeLayout` | `"EqualizeLayout"` | `Layout` |
| `SelectWorkspace` | `{"SelectWorkspace":{"id":1}}` | `Layout` |
| `SelectWorkspaceDelta` | `{"SelectWorkspaceDelta":{"delta":1}}` | `Layout` |
| `NewWorkspace` | `{"NewWorkspace":{"name":"repo","root":"/src/repo","env":{"EDITOR":"vim"}}}` | `Layout` |
| `NewPane` | `{"NewPane":{"workspace":null,"tab":null,"split":"Horizontal","command":["codex"],"name":null}}` | `Layout` |
| `ResizePane` | `{"ResizePane":{"direction":"Right","cells":5}}` | `Layout` |
| `ScrollPane` | `{"ScrollPane":{"id":3,"delta":10}}` | `Layout` |
| `ReadPane` | `{"ReadPane":{"id":3,"scrollback":true,"lines":50}}` | `PaneText` |
| `ZoomPane` | `"ZoomPane"` | `Layout` |
| `AgentState` | `{"AgentState":{"pane":3,"state":"blocked","source":"hook"}}` | `Layout` |
| `Query(Manifests)` | `{"Query":"Manifests"}` | `Manifests` |
| `ReloadManifests` | `"ReloadManifests"` | `Manifests` |

`MovePaneToTab` accepts a tab in any workspace (tab ids are global). A
cross-workspace move follows the pane: the target workspace and tab become
active, as `FocusPaneId` would. A workspace whose last tab is emptied by the
move receives a fresh shell tab, because a workspace always has at least one.

`NewWorktreeWorkspace` runs `git worktree add` for `branch` (created from `from`,
or checked out when it already exists) under the `[worktrees] directory` config
(default `~/.kodade/worktrees`) or its explicit `path`, then opens a
`repo:branch` workspace rooted in the new worktree. `OpenWorktreeWorkspace`
opens only an already registered linked checkout of `repo_root`; it never
creates, copies, or removes a directory. `RemoveWorktreeWorkspace` runs a non-forced `git worktree
remove` before closing the workspace unless `keep` is set. If Git refuses a
dirty or otherwise unremovable checkout, the daemon replies `Error` and leaves
the workspace and its panes intact. The directory is only ever removed when Git
reports it as a registered worktree. Each `WorkspaceInfo` in a `Layout` carries a
`branch` (the workspace root's current git branch, refreshed on the daemon's 2 s
tick, `null` outside a repo) and a `parent` (the workspace id whose root is a
worktree workspace's main repo, when that workspace is open, else `null`).

`NewWorkspace.env` is explicit per-workspace environment metadata. The daemon
validates shell-style names, rejects Ködade's routing variables, persists it,
and applies it only to future panes in that workspace; it never changes a shell
profile or already-running panes.

`ApplyLayout` **executes code**: every pane the file names that is not already
alive is spawned with the saved `command`, through the login shell, in the saved
`cwd`. Treat a layout file like a shell script and only apply files you trust.
The file must also be internally consistent — no id used twice, every tree leaf
backed by a pane entry, and no pane entry outside its tab's tree — or the daemon
answers `Error` and changes nothing.

Messages that act on "the focused pane" (`ClosePane`, `ZoomPane`, `SwapPane`,
`ResizePane`, `BreakPane`, …) have no id argument: send `FocusPaneId` first.
That is exactly what `kodade-cli pane kill|zoom|swap|resize` does.

`PromptAgent` is the guarded automation write used by `agent prompt`. The
daemon re-detects the pane immediately before writing and requires both the
recognized agent label and `PaneSnapshot.agent_generation` to match; it rejects
blocked, exited, or replaced agents without writing any bytes. Its `bytes`
payload is one ordered PTY submission and is capped at 64 KiB.
The check uses a fresh foreground-process probe; a process may still exit in
the kernel interval between that probe and the PTY write, so automation should
surface the command's result and retry only after resolving the target again.

`Query(Manifests)` returns the active detection manifest metadata (name,
display label, process/title matchers, rule count, and `builtin` or `user
override` source). `ReloadManifests` parses every bundled and local override
before swapping the running cache. A parse failure returns `Error` and keeps
the prior cache active; no daemon restart is needed after fixing the file.

Only `Hello` establishes an independent interactive view. Connections that
have not sent `Hello` use the persisted script selection, so a CLI `FocusPaneId`
followed by an action on a fresh socket preserves its targeting behavior.

`Hello` and `Resize` record the connection's requested dimensions. A shared
PTY has one physical size, so the client that most recently sends a
view-changing request owns that PTY geometry until another client interacts.
Read-only queries never resize PTYs. This is intentional last-interacting
arbitration: clients retain independent selections and scrollback while an
interactive terminal application sees one stable size at a time.

An `Error` rejects that request without changing the session. Attached clients
remain connected after recoverable command errors and may issue another
request; one-shot command clients can still treat the `Error` reply as failure.

## Server messages

- `Welcome` — `{"Welcome":{"session":"default","version":1}}`. Sent once, in
  reply to `Hello`.
- `Version` — `{"Version":{"version":1}}`. Reply to `Query(Version)`.
- `Pane` — `{"Pane":{…PaneSnapshot…}}`. Reply to `Query(Pane)`. Unlike `Layout`,
  which only carries the active tab's panes, this reaches any pane in the
  session; an unknown id answers `Error`.
- `Layout` — `{"Layout":{…LayoutSnapshot…}}`. The full session state: active
  workspace and tab, the workspace/tab lists used by the sidebar, the pane
  tree, and one `PaneSnapshot` per visible pane (title, focus, agent, state,
  `state_reason`, `state_age_secs`, cwd, and the `Screen`). A `Screen` carries
  plain `contents` (used by copy mode and `pane read`) plus `rows`, a list of
  styled `Run`s per terminal row, the cursor position, and the pane's
  bracketed-paste and mouse-reporting modes.
- `Notification` — `{"Notification":{"pane":3,"workspace":1,"tab":2,"agent":"codex","state":"blocked","seq":7}}`.
  Pushed to attached clients that have **not** subscribed when a known agent
  transitions into `blocked` or `done`. `seq` is monotonic per session so a
  client can drop duplicates.
- `PaneText` — `{"PaneText":{"id":3,"text":"…","scrollback_lines":120}}`. Reply
  to `ReadPane`; `scrollback_lines` counts the lines in `text` after any
  `lines` truncation.
- `Event` — `{"Event":{…}}`. Only sent to subscribed connections (see below).
- `Session` — `{"Session":{"version":1,…}}`. The persisted-layout view of the
  session; the same JSON `layout export` writes and `ApplyLayout` accepts.
- `Schema` — `{"Schema":{"version":1,"client_messages":[…],"server_messages":[…]}}`.
- `Manifests` — `{"Manifests":[{"name":"codex","source":"builtin",…}]}`.
- `Error` — `{"Error":{"message":"pane 9 not found"}}`. The daemon closes the
  connection after an error reply.
- `Shutdown` — `"Shutdown"`. The session is going away.

## The subscribe stream

Send `"Subscribe"` on any connection. The daemon replies with one `Layout`
snapshot (so ids in later events can be resolved) and then pushes a
`ServerMessage::Event` for every session event until the connection closes:

```json
{"Event":{"AgentStateChanged":{"pane":3,"from":"working","to":"blocked"}}}
{"Event":{"PaneOpened":{"pane":4}}}
{"Event":{"PaneClosed":{"pane":4}}}
{"Event":{"TabOpened":{"tab":5}}}
{"Event":{"TabClosed":{"tab":5}}}
{"Event":{"TabRenamed":{"tab":5,"name":"agents"}}}
{"Event":{"WorkspaceOpened":{"workspace":6}}}
{"Event":{"WorkspaceClosed":{"workspace":6}}}
{"Event":{"WorkspaceRenamed":{"workspace":6,"name":"repo"}}}
{"Event":{"Notification":{"pane":3,"workspace":1,"tab":2,"agent":"codex","state":"blocked","seq":7}}}
{"Event":{"SessionRenamed":{"name":"work","socket":"/run/kodade-cli/work.sock"}}}
```

Notes:

- Nothing is buffered. A connection only receives events raised after it
  subscribed; there is no replay and no cursor.
- `AgentStateChanged` is raised by agent detection, which runs when the session
  snapshots. While at least one connection is subscribed the daemon snapshots
  every 2 s on its own, so an events stream works with no client attached.
- A subscriber that stops reading is dropped from the backlog rather than
  stalling the session — it silently misses events. Re-read state with
  `Query(Layout)` after a gap.
- `Event::Notification` and `ServerMessage::Notification` carry the same
  payload. A subscribed connection receives only the `Event` form, so an
  attached TUI that also subscribes never sees a toast twice.
- A connection can subscribe and still drive the session: `Subscribe` does not
  change how other messages are answered.

`kodade-cli events [--json]` is this stream printed one event per line.

## Schema and versioning

`Query(Schema)` returns the message names this build knows plus a schema
version:

```json
{"Schema":{"version":1,"client_messages":["Query","Subscribe",…],"server_messages":["Welcome","Layout",…]}}
```

Version 1 is the current protocol (`PROTOCOL_VERSION` in `kodade-cli-proto`).
Both ends check it at attach time so a stale binary fails fast (#23):

- The client's first message is `Hello { cols, rows, version }`. `version`
  carries `#[serde(default)]`, so a pre-versioning client that omits it decodes
  as `0` rather than failing to parse.
- On a match the daemon answers `Welcome { session, version }` followed by the
  first `Layout`. On a mismatch it answers
  `{"Error":{"message":"protocol version mismatch: client N, daemon M — upgrade kodade-cli on both ends"}}`
  and closes the connection.
- The client verifies `Welcome.version` before it puts the terminal into raw
  mode, so a mismatch never leaves a half-drawn screen.
- `Query(Version)` is a cheap probe that keeps the connection open; `--remote`
  uses it to check compatibility before attaching.

The rules a client can rely on:

- Additive changes (a new message, a new optional field) do **not** bump
  `version`. Clients must ignore unknown fields and tolerate messages they do
  not know by discarding them.
- A breaking change to an existing message bumps `version`; a client that
  requires a specific shape should check it at connect time.
- `SessionFile.version` is separate and tracks the persistence format (#9).

## Remote sockets

`kodade-cli --remote USER@HOST` forwards the remote daemon's socket to a local
path with `ssh -L` and then speaks this exact protocol over it — a forwarded
socket is indistinguishable from a local one. `kodade-cli session path [-s NAME]`
prints the socket path a client should connect to, which is how the forward is
set up. See [DEVELOPMENT.md](DEVELOPMENT.md#remote-mode-23).

## Renaming a live session

`RenameSession` links the bound socket to the new path and unlinks the old one,
so the same daemon (and the same PTYs) answers at `<new-name>.sock`. Everything
already connected keeps working; subscribers get
`Event::SessionRenamed { name, socket }` and should switch any stored path to
the new one.

Two caveats worth passing to users:

- `KODADE_SESSION` in shells that were already running is **stale**. Generated
  hooks use their inherited private `KODADE_SOCKET` endpoint instead of `-s`,
  so they continue to report to the renamed daemon. Scripts that derive a
  public path from the old session name must update it themselves.
- Nothing answers at the old path anymore. Attaching with the old `-s NAME`
  starts a brand-new empty daemon rather than reattaching.

## Environment inside a pane

Every pane the daemon spawns gets:

| Variable | Value |
| --- | --- |
| `KODADE_PANE` | The pane's id |
| `KODADE_SESSION` | The session name |
| `KODADE_SOCKET` | The session's socket path |
| `KODADE_BIN` | The `kodade-cli` binary that hosts the pane |

That is enough for an agent or a script running in a pane to report its own
state without hard-coding anything:

```bash
"$KODADE_BIN" agent report "$KODADE_PANE" blocked -s "$KODADE_SESSION"
```

### CLI endpoint selection

Without an explicit endpoint flag, the CLI uses its inherited `KODADE_SESSION`
and `KODADE_SOCKET`. `--session NAME` overrides both, `--socket PATH` selects a
socket directly, and `--remote HOST` selects an SSH host. `session path` prints
the resolved socket. Session names are validated before deriving paths: 1–64
bytes, no path separators or control characters, and neither `.` nor `..`.

`doctor --json` probes `Query(Version)` without starting the daemon. Its JSON
contains `version`, `session`, `socket`, and `checks` (`name`, `status`, `detail`).

## Images

`Screen.graphics` carries image revision, placement ID, cell position/extent,
source pixel crop, and z order. `Query(Image)` fetches one exact revision as
`ImageData { id, revision, format, width, height, data }`; stale revisions fail.
Image bytes use base64, bounded to 8 MiB decoded. `PasteImage` validates a PNG,
saves it privately on the daemon host, and pastes its quoted path without
submitting input. See [GRAPHICS.md](GRAPHICS.md) for modes, limits, and cleanup.

## Compact client views

`SetCompactView { enabled }` applies after `Hello` to that connection alone.
The layout tree projects the focused pane while `panes` retains active-tab
identities for switching. Other processes and the persisted split tree stay
alive; disabling restores the split projection. Focus and input reapply the
interacting client's PTY dimensions. Ködade's auto mode enables this below
70 columns; compact headers provide previous/next pane, Switch, and Hosts
controls, with the usual keyboard bindings available.
