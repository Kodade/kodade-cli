# Ködade CLI

Ködade CLI is a terminal workspace for running agent CLIs such as Claude Code,
Codex, and other programs that run in a terminal. Workspaces contain tabs, and
tabs contain panes. It is the terminal-native companion to the
[Ködade desktop app](https://github.com/Kodade/kodade).

## Status

v0.1.0 is released: prebuilt binaries for macOS and Linux (arm64/x86_64) are
on the releases page, installable with the command below.

| Milestone | Scope | Status |
|---|---|---|
| M0 | Cargo workspace, daemon/client handshake, one PTY, rendering, keyboard passthrough | Done |
| M1 | Splits, tabs, workspaces, resize, mouse focus/resize/select, prefix keys, detach/reattach | Done |
| M2 | Agent detection, five states, sidebar rollup, agent subcommands, Claude Code hooks | Done |
| M3 | Themes, config, navigate mode, menus, scrollback/copy mode, OSC 52 | Done |
| M4 | CI builds, release binaries, install script, documentation | Done |

## Install

Install a prebuilt binary with:

```bash
curl -fsSL https://raw.githubusercontent.com/Kodade/kodade-cli/main/install.sh | sh
```

Or build from source:

```bash
cargo build
cargo run -p kodade-cli
```

The installer supports macOS and Linux on arm64 and x86_64.

## Quick usage

Run `kodade-cli` to attach to the default session; it starts the daemon when
needed. Use `kodade-cli -s SESSION` for a named session. Detach with the prefix
followed by `d`, and reattach by running `kodade-cli` again.

The default prefix is `ctrl+b`. After the prefix, the default actions are:

| Key | Action |
|---|---|
| `%` / `"` | Split right / down |
| `x` / `c` | Close pane / new tab |
| `n` | Navigate mode |
| `tab` / `p` | Next / previous tab |
| `z` / `d` | Zoom pane / detach |
| `r` / `W` | Rename / new workspace (`W` prompts for `NAME [PATH]`) |
| `G` | New git-worktree workspace (prompts for a branch) |
| `w` / `g` | Workspace picker / goto palette (fuzzy; type to filter, `enter` jumps) |
| `alt+w` | Next workspace (cycle without the picker) |
| `b` / `[` | Cycle sidebar (full → compact → hidden) / copy mode |
| `h` `j` `k` `l` or arrows | Focus left, down, up, right |
| `H` `J` `K` `L` | Resize left, down, up, right |
| `1`–`9` | Jump to that tab |
| `X` / `T` | Close / rename tab |
| `D` / `R` | Close / rename workspace |
| `alt+h` `alt+j` `alt+k` `alt+l` | Swap the pane with its neighbour |
| `o` / `O` / `;` | Next / previous / last pane |
| `!` / `=` | Break pane to a new tab / equalize the layout |
| `q` | Flash big pane ids (tmux `display-panes`) |
| `alt+r` | Resize mode (`hjkl` 1 cell, `HJKL` 5, `esc` exits) |
| `s` / `ctrl+r` | Settings menu / reload config and theme |
| `]` | Paste the internal buffer (last paste, copy-mode yank, or mouse selection) |
| `m` | Toggle mouse capture (hands the mouse back to the host terminal) |
| `N` | Jump to the most recent unread agent notification |

The status bar shows `session · workspace · tab` on the left and configurable
widgets on the right (`[zoom]`, a `● N blocked` counter, hostname, time — set
`status.right`). Pane borders carry `#id name — state` plus the cwd basename,
and the host terminal title tracks the active workspace/tab. See
[docs/CONFIG.md](docs/CONFIG.md#status-bar).

Mouse is enabled by default: click panes, tabs, and sidebar rows to focus;
drag pane borders to resize; scroll over a pane to scroll; right-click a pane,
tab, or workspace for its menu; the pane menu can break a pane out to its own
tab or equalize the layout, and the tab menu can reorder tabs. In navigate mode, `j`/`k` move through the
sidebar and `enter` selects a row (folding/unfolding a workspace, or activating a tab/pane); `q` or `esc` exits.

The sidebar has three shapes cycled by `prefix b`: the full list, a compact
3-column rail of workspace state dots, and a hidden 1-column gutter. It is
configurable via `[sidebar]` (`width`, `collapsed`, `auto_hide_below`,
`agents_panel`) and auto-hides on narrow terminals. Every workspace can be
folded (`enter` in navigate, `*` expands all; remembered per session), an agents
panel below the workspaces lists agent panes by urgency, and each workspace
carries a color swatch (right-click → `Color…`, or an auto-hashed fallback). See
[docs/CONFIG.md](docs/CONFIG.md#settings).

Dragging inside a pane selects text and copies it on release
(`mouse.copy_on_select`, OSC 52 so it works over SSH); double-click selects a
word, triple-click a line, and ctrl/cmd-click opens the URL under the pointer
with `ui.link_command`. Panes running a mouse-aware program (vim, lazygit,
htop) get the events themselves unless `mouse.passthrough = false`, and
`prefix m` turns capture off when you want the terminal's own selection. See
[docs/CONFIG.md](docs/CONFIG.md#mouse).

Copy mode (`prefix [`) freezes the pane's full scrollback and navigates it with
vi motions over the whole history, not just the visible screen. The status bar
shows `copy · LINE/TOTAL · / search · v V select · y copy · e editor · esc`.

| Key | Action |
|---|---|
| `h` `j` `k` `l` / arrows | Move by cell / line |
| `w` `b` / `W` `B` / `E` | Word / WORD motions (`e` is the editor, so word-end is `E`) |
| `0` `^` `$` | Line start / first non-blank / line end |
| `gg` / `G` | Top / bottom of the buffer |
| `{` / `}` | Previous / next blank line |
| `ctrl+u` `ctrl+d` | Half page up / down |
| `ctrl+b` `ctrl+f` / `PageUp` `PageDown` | Page up / down |
| `H` `M` `L` | Cursor to viewport top / middle / bottom |
| `v` / `V` / `ctrl+v` | Char / line / block selection anchor |
| `/` `?` then `n` `N` | Search forward / back (case-insensitive), step matches |
| `y` | Copy the selection (or current line) via OSC 52 and the paste buffer |
| `e` | Open the buffer in `$EDITOR` (fallback `vi`) in a new split |
| `esc` | Clear search, then the selection, then exit; `q` exits |

Copying sends the selection through OSC 52, including over SSH; copy payloads
are limited to 100 KB. The buffer is refetched (throttled) while the pane keeps
producing output. Copy mode draws plain text — the frozen cell colors of the
live screen are not reproduced there.

Paste is bracketed so a program can tell it from typing. Pasted text is
sanitized by default (`paste.sanitize`): CRLF is normalized, embedded escape
sequences — including a smuggled OSC 52 or CSI — are dropped, and control bytes
other than tab and newline are stripped. Large pastes are chunked and paced.
The last paste (or yank) is kept in a buffer that `]` re-sends.

When an agent transitions into `blocked` or `done`, the daemon notifies every
attached client: a status-bar toast in the state's color, a terminal bell, and
optionally a host-terminal desktop notification (`notify.toast = "system"`) or a
sound command (`notify.sound`). `prefix N` jumps to the pane of the most recent
unread notification. Notifications are configurable and can be turned off
entirely; see [docs/CONFIG.md](docs/CONFIG.md#configuration).

Bindings accept a chord or an array of chords, in any modifier order
(`split_right = ["%", "ctrl+alt+v"]`). A `ctrl`/`alt` chord that is not written
as `prefix+…` is global: it fires without the prefix. `prefix ctrl+r` reloads
`config.toml` and the theme in place, and `prefix s` opens a settings menu that
writes your choices back to `config.toml` without disturbing comments. See
[docs/CONFIG.md](docs/CONFIG.md) for all bindings and configuration.

Sessions survive a daemon restart: the layout (workspaces, tabs, pane trees,
names, cwds, and zoom — never scrollback) is saved under
`~/.local/state/kodade-cli/sessions/` (macOS: `~/Library/Application Support/…`)
and rebuilt with fresh panes on the next cold start; a corrupt file degrades to
a clean start and `kodade-cli ls` marks a restored session `(restored)`. Set
`[session] resume_agents = true` to re-run an agent's resume command on restore.

The CLI ships the Ködade look: warm neutrals with the amber accent `#E7A33B`
and a purple-free ANSI palette, matching the desktop app. `theme = "auto"`
(the default) picks `kodade-dark` or `kodade-light` from the terminal
background; `tokyo-night` and custom themes are also available. See the
[Themes](docs/CONFIG.md#themes) section for the schema.

Run `kodade-cli --help` for the full command list and `kodade-cli --version`
for the installed version. The scripting commands are:

- `kodade-cli ls` — list sessions, workspaces, tabs, panes, and states.
- `kodade-cli new -w NAME [PATH]` — create a workspace with an optional root
  directory and print its id (selects the workspace if the name already exists).
- `kodade-cli run [-w NAME] [-t TAB] [--name NAME] -- CMD ARGS…` — run a command
  in a new pane through the login shell and print the new pane id.
- `kodade-cli split [--down] [-p PANE] [-- CMD…]` — split the focused (or given)
  pane and print the new pane id.
- `kodade-cli new-tab [-w NAME] [--name NAME]` — open a new tab and print its
  pane id.
- `kodade-cli pane ls|read|send-keys|kill|focus|zoom|swap|move|resize|wait-output` —
  inspect and drive panes.
- `kodade-cli pane read PANE [--lines N] [--scrollback]` — print a pane's text
  (visible screen by default; `--scrollback` includes the full history, `--lines N`
  keeps only the last N lines).
- `kodade-cli tab ls|new|close|rename|select` — tabs of the active workspace
  (TAB is a name or an id).
- `kodade-cli workspace ls|new|close|rename|select|color WS HEX|off` —
  workspaces (WS is a name or an id); `new` is the same as the top-level `new`,
  and `color` sets the sidebar swatch.
- `kodade-cli session ls|path|kill [NAME]|rename NAME` — every session on this
  machine; `ls` probes each socket and marks it `(restored)` or `(dead)`, and
  `path` prints the daemon socket path (`--remote` prints the host's). With
  `--remote` every `session` verb runs on the host.
- `kodade-cli layout export [FILE]|apply FILE` — save and restore a layout.
  **`apply` runs the commands saved in the file** (through the login shell, in
  each pane's saved directory), so only apply layout files you trust.
- `kodade-cli worktree add BRANCH [--from REF] [-w NAME]` — `git worktree add`
  a branch on the workspace's repo and open a `repo:branch` workspace rooted in
  it (prints the new workspace id). `worktree list` shows every branch workspace
  with its root and parent; `worktree remove WS|BRANCH [--keep]` closes it and
  removes the worktree unless `--keep`.
- `kodade-cli events [--json]` — stream session events until interrupted.
- `kodade-cli completion zsh|bash|fish` — print a completion script.
- `kodade-cli agent ls` — list recognized agents and states.
- `kodade-cli agent attach PANE` — focus a pane and attach the TUI.
- `kodade-cli agent rename PANE NAME` — rename a pane.
- `kodade-cli agent explain PANE` — print a pane's state, reason, and the bottom-8-line window it matched.
- `kodade-cli agent wait PANE --state STATE [--timeout S]` — block until a pane
  reaches a state; exits 0 when it does and 2 on timeout.
- `kodade-cli agent report PANE STATE` — report an agent state to the daemon.
- `kodade-cli agent update-manifests` — opt-in refresh of agent-detection manifests from GitHub.
- `kodade-cli send PANE TEXT` — send text followed by a newline (`--no-newline` is also supported).
- `kodade-cli kill-session` — stop the current session.
- `kodade-cli config path|show|validate` — print the config path, the effective
  config as TOML, or check the file (exits non-zero on problems).

Text and names may start with `-` (`kodade-cli send 1 -y`), and `--` forces
the next value through verbatim when it collides with a flag
(`kodade-cli send 1 -- --no-newline`).

`-w` and `-t` accept either a name or a numeric id. A workspace can have a root
directory (`new -w NAME PATH` or the `prefix W` prompt); new panes, splits, and
tabs start in the focused pane's live working directory, falling back to the
workspace root, so agents keep landing in the right repo.

`ls`, `agent ls`, `agent explain`, `pane ls`, `tab ls`, `workspace ls`, and
`session ls` also accept `--json`, which prints the matching protocol snapshots
for scripts.

Scripts wait on agents instead of polling by hand:

```bash
# Block until pane 3 needs you, then read the last five lines it printed.
kodade-cli agent wait 3 --state blocked && kodade-cli pane read 3 | tail -5

# The full session snapshot, for jq.
kodade-cli ls --json | jq '.panes[] | {id: .id, state: .state}'

# React to every state change as it happens.
kodade-cli events --json | jq -r 'select(.AgentStateChanged) | .AgentStateChanged.pane'
```

`pane wait-output PANE --match TEXT` waits for text on a pane's **visible
screen** (not its scrollback), so text that has already scrolled off is not
matched. The match is a plain substring, not a regular expression — Ködade CLI
ships without a regex dependency, so `--match` is documented as text rather than
`REGEX`. Both waits work on panes in background tabs and workspaces.

`pane send-keys` accepts tmux-style key names — `Enter`, `Escape`, `Tab`,
`Space`, `BSpace`, arrows, `Home`, `End`, `PageUp`, `PageDown`, `Insert`,
`Delete`, `F1`–`F12`, `C-c` (control) and `M-x` (alt) — and sends anything else
as literal text: `kodade-cli pane send-keys 3 "npm test" Enter`. A capitalized
word that is not a known key name is rejected rather than typed, so a typo like
`Entr` fails loudly; use `--literal` to send such text verbatim
(`kodade-cli pane send-keys 3 --literal "Hello there"`).

`session rename NAME` moves the live session's socket, so the same daemon and
panes answer under the new name. Shells that were already running keep the old
`KODADE_SESSION` / `KODADE_SOCKET` values, and attaching with the old name
starts a new empty session.

Panes a session spawns get `KODADE_PANE`, `KODADE_SESSION`, `KODADE_SOCKET`,
and `KODADE_BIN` in their environment, which is everything a custom agent needs
to report its own state. The socket protocol itself — framing, every message,
the `Subscribe` event stream, and the schema query — is documented in
[docs/SOCKET-API.md](docs/SOCKET-API.md).

### Two agents on two branches

From a workspace rooted in a git repo, `prefix G` (or `worktree add`) spins up an
isolated worktree per branch so two agents never step on each other:

```sh
kodade-cli new -w repo ~/src/repo      # workspace rooted in the repo (on main)
kodade-cli worktree add feat-a         # → workspace repo:feat-a in a new worktree
kodade-cli worktree add feat-b --from main
# run an agent in each branch's workspace
kodade-cli run -w repo:feat-a -- claude
kodade-cli run -w repo:feat-b -- codex
```

The sidebar nests each worktree under `repo` with its branch (`⎇ feat-a`), and
`repo` shows its own branch dimmed after the name. `worktree remove feat-a`
closes the workspace and deletes the worktree directory.

## Remote

`kodade-cli --remote USER@HOST` attaches to a daemon on another host over an
SSH-forwarded socket, so you can drive agent panes on a build box or server from
your laptop. The same flag works with the scripting subcommands
(`kodade-cli --remote HOST -s work agent ls`) and with `-s NAME` to pick a
session. `--remote HOST session ls` lists the host's sessions (prefixed with
`host:`).

Requirements and behavior:

- `kodade-cli` must be installed on the remote host and on your PATH there. If it
  is missing, the command prints the install one-liner and exits. (Auto-install
  is a later phase.)
- Auth is your existing SSH setup — config, agent, and keys. Ködade never sees
  or proxies credentials.
- The daemon is started on the host automatically if it is not already running,
  and it keeps your session alive between connections. Dropping the link and
  re-running `--remote` reattaches to the same session; a control-master
  connection lingers ~60 s (`ControlPersist`) so reconnecting is fast.
- Set `ServerAliveInterval` in your `~/.ssh/config` for the host if you attach
  over flaky links, so a dead connection is noticed promptly.
- Copy mode and the clipboard run on your local machine, so yanking from a
  remote pane copies to your local clipboard as usual.

Requires OpenSSH with Unix-domain forwarding (`-L localsock:remotesock`), which
is standard on current macOS and Linux.

`kodade-cli integrate list` shows the available integrations.
`kodade-cli integrate <agent>` prints the hook/notify settings and
`--write` installs them: `claude-code` and `gemini-cli` merge hooks into their
`settings.json`; `codex` merges a `notify` entry into `~/.codex/config.toml`
(add `--force` to replace an existing one). See
[docs/AGENT-DETECTION.md](docs/AGENT-DETECTION.md) for details.

See [docs/CONFIG.md](docs/CONFIG.md), [docs/AGENT-DETECTION.md](docs/AGENT-DETECTION.md),
[docs/SOCKET-API.md](docs/SOCKET-API.md),
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md), and [docs/RELEASING.md](docs/RELEASING.md)
for reference and contributor details. The product direction is in
[docs/PRD.md](docs/PRD.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
