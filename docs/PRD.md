# Ködade CLI — Product Requirements Document

**Status:** Draft v0.1 · 2026-09-01
**Owner:** @ContractorKeith
**Repo:** github.com/Kodade/kodade-cli · Apache License 2.0

## 1. Summary

Ködade CLI is a lightweight terminal workspace for running and supervising
agent CLIs (Claude Code, Codex, Grok Build, OpenCode, and anything else that
runs in a terminal). It brings the core Ködade idea — one place to watch many
agents work, with clear state and quick attention routing — to a plain
terminal: local, over SSH, or inside any existing terminal emulator.

The layout and interaction model follow herdr (herdr.dev): workspaces contain
tabs, tabs contain panes, panes host real terminal processes; a sidebar rolls
agent state upward so you can see at a glance which project needs you. Mouse
works everywhere, and keybindings use a tmux-compatible prefix so muscle
memory transfers.

It deliberately excludes the desktop app's heavier surfaces: no KödWork
background tasks, no GitHub/review integration, no KödChat. Terminals,
workspaces, and agents only.

## 2. Why

- Ködade (desktop) is macOS-only today. A terminal companion runs anywhere a
  shell runs — Linux servers, SSH sessions, Windows via WSL — and needs no
  installer, signing, or notarization.
- People already living in a terminal (tmux/zellij users) want agent-state
  awareness without adopting a desktop app.
- herdr proved the shape works: multiplexer + agent detection + state rollup
  is the right minimal product. Ködade CLI is our take on that shape, sharing
  Ködade's design language and conventions.

## 3. Goals / Non-Goals

### Goals (v1)
1. Single static binary, installable via `curl | sh`, Homebrew, and `cargo install`.
2. Workspaces → tabs → panes, persistent across detach/reattach (daemon model).
3. Agent detection with five states — `blocked`, `working`, `done`, `idle`,
   `unknown` — rolled up pane → tab → workspace → sidebar.
4. Mouse-native: click to focus, drag borders to resize, right-click menus,
   click workspaces/tabs/agents in the sidebar, native text selection.
5. tmux-compatible prefix keybindings (`ctrl+b` default) plus a persistent
   navigate mode; fully remappable.
6. A `kodade-cli` command surface for scripting: list/attach/send/rename
   panes and agents from the shell.
7. Themes (dark/light/auto) matching Ködade's visual language.

### Non-Goals (v1)
- KödWork tasks, GitHub/PR/review integration, KödChat chat UI.
- Editor or file-browser panes; agents open their own editors.
- Hosted/remote sync service, accounts, telemetry.
- Windows native (WSL is the supported path; native later).
- Plugin marketplace (config-file extensibility only in v1).

## 4. Users

- **Ködade users on a server** — same mental model as the desktop app when
  SSH'd into a build box or homelab machine.
- **Terminal-first agent operators** — run 3–10 agent CLIs concurrently and
  need to know which one is blocked without cycling panes.
- **tmux refugees** — want a multiplexer that understands agents; must not be
  punished for tmux muscle memory.

## 5. Product design

### 5.1 Object model (herdr layout)

| Object | What it is |
|---|---|
| **Session** | A runtime namespace held by the daemon. `kodade-cli` attaches to the default session; `kodade-cli -s name` gives an isolated one. Survives client disconnect. |
| **Workspace** | Container for one project or investigation — typically one repo. Owns tabs and rolls up child agent state. |
| **Tab** | A named pane layout inside a workspace (e.g. "agents", "logs", "server"). Tabs keep running in the background. |
| **Pane** | A real PTY process. Splittable right or down, resizable, renamable, closable; scrollback preserved. |
| **Agent** | A recognized foreground process in a pane, with a tracked state. |

### 5.2 Screen layout

```
┌─ sidebar ──────┬─ tab bar: [agents] [logs] [server]  ─────────────┐
│ ▸ kodade    ●  │ ┌───────────────────────┬──────────────────────┐ │
│ ▾ kodade-cli ◐ │ │ claude — working ◐    │ codex — blocked ●    │ │
│    agents   ◐  │ │                       │                      │ │
│    logs        │ │  (terminal output)    │  (terminal output)   │ │
│ ▸ dotfiles     │ ├───────────────────────┴──────────────────────┤ │
│                │ │ zsh — idle                                   │ │
│                │ └──────────────────────────────────────────────┘ │
└────────────────┴─ status: session · workspace · prefix hint ──────┘
```

- Sidebar lists workspaces with rolled-up state dots; expand to tabs/agents.
  Collapsible (`prefix b` or click the gutter) for small terminals.
- A blocked agent tints its pane border, tab, and workspace row — attention
  flows to the sidebar without checking each pane.
- Status bar shows session name, active workspace/tab, and pending prefix.

### 5.3 Agent detection

Tiered status authority per pane, most authoritative wins:

1. **Lifecycle hooks** — for CLIs that support them (e.g. Claude Code hooks),
   an installed hook reports `working`/`idle`/`blocked` directly to the daemon
   socket. Most reliable; offered via `kodade-cli integrate <agent>`.
2. **Screen manifests** — pattern rules matched against the live bottom
   buffer to classify known approval/question/permission UI. Conservative:
   only mark `blocked` on a confident match; novel prompts read as `idle`.
3. **Process + title heuristics** — foreground process name identifies the
   agent; terminal title and OSC progress sequences add evidence.

Manifests ship with the binary, update from the repo without a restart, and
can be overridden in `~/.config/kodade-cli/agent-detection/`.
`kodade-cli agent explain <pane>` prints why a state was chosen.

v1 detects at minimum: Claude Code, Codex, Grok Build, OpenCode, Gemini CLI,
Aider, plus a generic "shell" fallback. The manifest format is public so
adding an agent is a PR-sized change.

### 5.4 Input model

Three keyboard modes (herdr/tmux model):

- **Terminal mode** (default) — keys go to the focused pane.
- **Prefix mode** — `ctrl+b`, then an action key: `%`/`"` split, arrows or
  `hjkl` move focus, `c` new tab, `d` detach, `w` workspace picker, `[` scroll
  /copy mode, `z` zoom pane, `x` close pane.
- **Navigate mode** — a persistent mode (`prefix n`) where single keys walk
  workspaces/tabs/agents in the sidebar; `enter` focuses, `esc` returns.

Mouse: click focuses panes and sidebar items, drag resizes splits, scroll
wheel scrolls history, right-click opens a context menu (split, rename,
close, attach), selection copies to the system clipboard (OSC 52 over SSH).
Mouse capture can be disabled per-pane or globally.

All bindings remappable in `~/.config/kodade-cli/config.toml`.

### 5.5 CLI surface

```bash
kodade-cli                          # attach default session (starts daemon if needed)
kodade-cli -s work                  # named session
kodade-cli ls                       # sessions, workspaces, panes, agent states
kodade-cli new -w myrepo            # create workspace (cwd = repo root)
kodade-cli run -w myrepo -- claude  # spawn a pane running an agent
kodade-cli agent ls                 # agents with states
kodade-cli agent attach <id>        # jump straight to an agent's pane
kodade-cli agent rename <id> <name>
kodade-cli agent explain <id>       # why this state
kodade-cli send <pane> "text"       # send input to a pane
kodade-cli detach / kill-session
```

The same protocol is exposed on the daemon's Unix socket (JSON messages) so
scripts and the Ködade desktop app can drive it later.

### 5.6 Persistence

- Daemon owns PTYs; clients are thin. Closing the terminal detaches, nothing
  dies — same contract as tmux.
- Layout, workspace/tab/pane names, cwds, and scrollback survive reattach.
- On daemon restart (reboot), layout and pane commands are restored from a
  state file; agent processes are relaunched only with explicit
  `--resume`-style opt-in per pane, never silently.

### 5.7 Theming

Dark, light, and auto (terminal background query). Ködade brand accent for
selection/active borders; state colors: blocked = red/amber, working = blue,
done = green, idle = dim. Themes are TOML files; user themes drop into
`~/.config/kodade-cli/themes/`.

## 6. Technical approach

- **Language:** Rust throughout. Single crate workspace:
  - `kodade-cli-daemon` — PTY host (`portable-pty`), session state, agent
    detection, Unix socket server (`tokio`).
  - `kodade-cli` — client binary: TUI (`ratatui` + `crossterm`) and the
    scripting subcommands; both talk to the daemon socket.
  - `kodade-cli-proto` — shared protocol types (`serde`).
- **Terminal emulation:** a VT parser (`vt100` or `wezterm-term`) in the
  daemon so scrollback and screen snapshots (for manifests) exist server-side.
- **Spawn through login shell** so `PATH` matches the user's environment —
  same rule as Ködade. No credential handling of any kind.
- **Targets:** macOS (arm64/x86_64) and Linux (x86_64/arm64). Windows via WSL.
- **Distribution:** GitHub Releases with prebuilt binaries; `cargo install
  kodade-cli`; Homebrew tap once stable.

Why these dependencies: `ratatui`/`crossterm` are the standard maintained
Rust TUI stack with mouse support built in; `portable-pty` is the proven PTY
layer from WezTerm; `tokio` is required for multiplexed socket + PTY IO.

## 7. Milestones

| # | Milestone | Scope |
|---|---|---|
| M0 | Skeleton | Cargo workspace, daemon/client handshake, spawn one PTY, render it, keyboard passthrough. |
| M1 | Multiplexer core | Splits, tabs, workspaces, resize, mouse focus/resize/select, prefix keys, detach/reattach. |
| M2 | Agents | Process detection, manifest engine, five states, sidebar rollup, `agent` subcommands, Claude Code hook integration. |
| M3 | Polish | Themes, config file, navigate mode, right-click menus, scrollback/copy mode, OSC 52. |
| M4 | Release | CI builds, release binaries, install script, docs site page under kodade.com. |

## 8. Success criteria

- A Ködade user can SSH to a server, run one command, and supervise 5 agents
  with the same at-a-glance state model as the desktop app.
- A tmux user's `ctrl+b` habits (split, move, detach, scroll) work unchanged
  on day one.
- Blocked-state detection produces no false "blocked" alarms in a week of
  daily use (conservative matching holds).
- Binary under ~10 MB, cold attach under 100 ms, idle CPU near zero.

## 9. Open questions

1. Short binary alias — ship a `kod` symlink alongside `kodade-cli`?
2. Should the desktop app attach to the CLI daemon as a remote backend
   (KödSSH synergy), and does that constrain the socket protocol now?
3. Scrollback persistence size/limits — full history vs. last N lines per pane.
4. Do we vendor herdr-compatible manifest syntax or define our own? (Own
   format, but document the mapping, is the current lean.)
