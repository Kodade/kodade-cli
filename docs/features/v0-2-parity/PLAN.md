# v0.2.0 parity plan

Roadmap for GitHub milestone `v0.2.0` (issues #6–#24), the herdr / Ködade
desktop parity review filed 2026-09-02. This is the shared context for every
worker on the milestone. Decisions here are binding unless the orchestrator
changes them in this file.

## Goals per issue

| # | Goal (done when…) |
|---|---|
| 6 | `prefix ?` opens a help overlay generated from the live binding table; status hint, `kodade-cli keys [--json]`, unbound-key note, first-attach hint, and `docs/CONFIG.md` table all come from that one table. |
| 7 | Panes render colors, attributes, and the cursor from a cell-level `Screen` in proto; indexed colors map through the theme ANSI palette; wide chars occupy two cells; snapshot size and CPU stay within the issue's budgets. |
| 8 | Built-in themes are `kodade-dark` / `kodade-light` with the desktop tokens; the schema gains optional `bg`, `surface`, `selection`, `cursor`, `menu_*`, `tab_active_*`, `sidebar_bg`, and a 16-entry `[ansi]` palette; `tokyo-night` stays as an extra; docs updated. |
| 9 | Daemon persists layout, names, cwds, and zoom to a versioned state file (debounced) and restores it on a cold start; corrupt files degrade to a clean start with a warning; `ls` marks `(restored)`. |
| 10 | `blocked` / `done` transitions produce a status toast, terminal bell, and optional OSC 777 / sound; `prefix o` jumps to the most recent unread notification; `only_when_unfocused` honored; daemon emits a `Notification` message. |
| 11 | Status bar shows real session · workspace · tab plus `[zoom]` and a blocked counter; pane titles carry `#id` and cwd basename; ellipsis truncation; OSC 0 window title; `prefix q` flashes pane ids. |
| 12 | Left-drag selects text and copies via OSC 52 (copy-on-select), double/triple click select word/line, ctrl-click opens links, mouse events pass through when the pane app enabled mouse reporting, `prefix m` toggles capture. |
| 13 | Workspaces have a root directory; splits and new tabs inherit the focused pane's live cwd; `kodade-cli new -w NAME [PATH]`, `run`, `split`, `new-tab` exist; sidebar shows the root basename. |
| 14 | Jump to tab 1–9, close/rename tab and workspace, swap panes, move tabs, next/prev/last pane, previous workspace, resize mode, break pane, equalize — all remappable, unit-tested on the layout tree, and listed in help. |
| 15 | Copy mode searches the full scrollback (`/ ? n N`), has vi motions, line-wise select, `e` opens scrollback in `$EDITOR`, and `kodade-cli pane read` prints pane text. |
| 16 | clap CLI with `--help` / `--version` / `--json`; `pane`, `tab`, `workspace`, `session`, `agent wait`, `pane wait-output`, `layout export|apply`, `completion`; socket `Subscribe` event stream documented in `docs/SOCKET-API.md`; `KODADE_BIN` / `KODADE_SOCKET` exported. |
| 17 | `prefix w` opens a fuzzy workspace picker; `prefix g` a goto picker over workspaces, tabs, and agents with blocked entries first; both reuse the overlay widget. |
| 18 | More manifests (Cursor, Copilot, Cline, Amp, Droid, Kimi, Qwen, Pi, Hermes) with fixture tests, `done` rules where stable, `resume` field, `integrate --list` / codex / gemini, `agent explain` shows the matched window, opt-in `agent update-manifests`, state age in sidebar. |
| 19 | Sidebar width configurable, compact rail and hidden modes with auto-hide, per-workspace collapse, urgency-sorted agents panel, lowercase dim headings, optional color swatches, ellipsis truncation. |
| 20 | Key bindings accept arrays, `shift+`, and global (unprefixed) chords; `prefix R` reloads config and theme live; `prefix s` settings overlay writes back with `toml_edit`; `kodade-cli config path|show|validate`. |
| 21 | Bracketed paste enabled; pastes wrap only when the pane has bracketed mode on; control chars stripped; `prefix ]` pastes the internal buffer; large pastes chunked. |
| 22 | `kodade-cli worktree add|remove|list` and `prefix G` create git-worktree workspaces named `repo:branch`, grouped under the parent repo in the sidebar with a dimmed branch label. |
| 23 | `kodade-cli --remote user@host` runs the local TUI over an SSH-forwarded socket; `Hello` / `Welcome` carry a protocol version and refuse mismatches. |
| 24 | Every review nit fixed: `done` sticks, `y/n` documented, navigate keys documented, gutter hint, menu flips near the right edge, `pane_cols` gutter drift, OSC 11 skipped unless `auto`, `--version`, dependency bump evaluated. |

## Phases and integration order

Work runs in phases. Inside a phase, workers run in parallel on their own
worktree branch (`issue/<n>-<slug>`). Integration order inside a phase is the
listed order; later branches rebase onto `main` after earlier ones land.

| Phase | Work | Seat |
|---|---|---|
| 0 | Prep: clap CLI skeleton (`--help`, `--version`, `--json` on reads), extract client `App` state + handlers out of `main.rs`, evaluate ratatui/crossterm/vt100 bump. Partially #16, #24. | opus-5 |
| 1 | #8 theme → #7 colored panes → #13 cwd/root/run → #18 manifests + state age + `done` sticking (#24). | opus-4.8 ×3, opus-5 for #7 |
| 2 | #14 layout actions → #20 bindings/reload/settings → #9 persistence → #11 chrome (+ #24 menu flip, `pane_cols`, gutter hint, OSC 11 skip). | opus-5 for #14/#20, opus-4.8 for #9/#11 |
| 3 | #6 help overlay + overlay widget → #10 notifications → #12 mouse selection → #21 paste. | opus-4.8, opus-5 for #12 |
| 4 | #15 copy mode → #17 pickers → #19 sidebar → #16 remainder (verbs, events, socket docs). | opus-4.8, opus-5 for #16 |
| 5 | #22 worktrees → #23 remote + handshake → #24 sweep and close. | opus-4.8 |

## Binding design decisions

Fixed here so parallel workers converge.

### Theme schema (#8)

- Built-ins: `kodade-dark`, `kodade-light`, `tokyo-night`. `dark` and
  `light` remain accepted names and alias to the Ködade themes. `auto` picks
  between the two Ködade themes.
- State colors: blocked = coral red, working = sage green (matches the
  desktop), done = amber, idle = `textDim`.
- New optional fields with fallbacks (missing field → derived from existing
  fields so user themes keep loading): `bg`, `surface`, `selection`, `cursor`,
  `menu_bg`, `menu_fg`, `tab_active_fg`, `tab_active_bg`, `sidebar_bg`.
- `[ansi]` table keys: `black red green yellow blue magenta cyan white
  bright_black bright_red bright_green bright_yellow bright_blue
  bright_magenta bright_cyan bright_white`, exposed as `Theme.ansi:
  [Color; 16]` in that order. Missing table → standard xterm palette.

### Proto `Screen` (#7)

```rust
pub struct Screen {
    pub contents: String,           // kept for copy mode / pane read
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub rows: Vec<Vec<Run>>,        // one Vec<Run> per screen row
    pub bracketed_paste: bool,      // vt100 mode flags for #21 / #12
    pub mouse_reporting: bool,
}
pub struct Run { pub text: String, pub fg: CellColor, pub bg: CellColor, pub attrs: u8 }
pub enum CellColor { Default, Indexed(u8), Rgb(u8, u8, u8) }
// attrs bits: 1 bold, 2 italic, 4 underline, 8 dim, 16 inverse
```

Adjacent cells with identical style coalesce into one run. A wide char is
emitted once; its continuation cell is skipped.

### Client structure (Phase 0)

- `main.rs`: clap dispatch only.
- `cli.rs`: clap `Parser` / `Subcommand` definitions. Every read command
  accepts `--json`.
- `app.rs`: `App` struct holding all TUI state (formerly `loop_tui` locals)
  with `handle_key`, `handle_mouse`, `handle_layout`, `draw`. New modes
  (overlay, selection, paste, notify) get their own modules and hook in via
  `App`.
- Existing subcommands and flags keep working unchanged.

### Daemon model (#13, #9, #18, #10)

- `Workspace.root: Option<PathBuf>`.
- `Pane::spawn` takes `cwd: Option<PathBuf>` and `command: Option<Vec<String>>`
  (run through `$SHELL -lc 'exec …'`). New panes inherit the focused pane's live
  cwd, then the workspace root, then the daemon cwd.
- `proc.rs`: `pane_cwd(pid)` (Linux `/proc/<pid>/cwd`, macOS `lsof -a -p PID
  -d cwd -Fn`) and `foreground_command(pid)`. Cached like `process_name`.
- Proto: `NewWorkspace { name, root: Option<PathBuf> }`; `NewPane { workspace:
  Option<WorkspaceId>, tab: Option<TabId>, split: Option<SplitAxis>, command:
  Option<Vec<String>>, name: Option<String> }`. The new pane becomes focused, so
  the reply snapshot identifies it.
- Pane tracks `state_since` and `last_state`; `PaneSnapshot.state_age_secs` and
  `AgentInfo.state_age_secs` expose it. A hook-reported `done` sticks until the
  next PTY output or hook report (no TTL); `blocked` / `working` hooks keep the
  30 s TTL.
- `ServerMessage::Notification { pane, workspace, tab, agent, state }` is sent
  on `blocked` / `done` transitions to every attached client. #16 folds it into
  the general `Event` stream.

### Persistence (#9)

- Path: `$XDG_STATE_HOME/kodade-cli/sessions/<name>.json`, default
  `~/.local/state/kodade-cli/…`; macOS `~/Library/Application
  Support/kodade-cli/sessions/`.
- File carries `"version": 1`. Unknown version or parse failure → clean start
  plus a warning on stderr and in the first `Welcome`.

### Key bindings (#20)

- Grammar: `[ctrl+][alt+][shift+]key`. A value is a string or array. Bare
  `ctrl+…` / `alt+…` chords (no `prefix+`) are global and fire without the
  prefix. Uppercase letter = shift stays valid.
- `Config.bindings` (prefixed) and `Config.globals` (unprefixed), both
  `HashMap<KeyEvent, Action>`. `Config.chords_for(Action) -> Vec<String>` feeds
  the help overlay, status hint, docs table, and `kodade-cli keys`.

### Overlay widget (#6)

`overlay.rs`: a centered box with title, optional filter line, scrollable
rows, and selection. Help (#6), pickers (#17), and settings (#20) all use it.

## Worker conventions

- Branch from `main` in the assigned worktree; commit as `<type>: <summary>
  (#N)` with `Closes #N` in the body when the issue is complete.
- Gate before handoff: `cargo fmt && cargo clippy --all-targets -- -D warnings
  && cargo test`. Rebase onto `origin/main` and rerun the gate if `main` moved.
- Update `docs/CONFIG.md`, `docs/DEVELOPMENT.md`, `docs/AGENT-DETECTION.md`,
  and `README.md` in the same change when behavior they describe changes.
- Umlaut in prose and UI strings (`Ködade`); ASCII in identifiers and paths.
- Small-team code: readable over clever, brief comments marking functions and
  non-obvious constraints, a new dependency needs a one-line reason in
  `Cargo.toml`.

## Outcome (2026-09-02)

All v0.2.0 issues landed on `main`. Landing commit per issue:

| Issue | Landing commit | Summary |
| --- | --- | --- |
| #6 | `38413fd` | Help overlay, `keys` command, status hints from the binding table |
| #7 | `be6167a` | Cell-level pane rendering with colors, attributes, cursor |
| #8 | `c08a8a6` | Ködade themes, expanded theme schema, ANSI palette |
| #9 | `a585254` | Persist session layout, restore on daemon restart |
| #10 | `2b1f5ad` | Agent notifications: status toast, bell, OSC 777, sound, `prefix N` |
| #11 | `91d890b` | Status bar widgets, pane ids/cwd in chrome, window title, display-panes |
| #12 | `9015ef6` | Mouse text selection, copy-on-select, link clicks, passthrough |
| #13 | `f3644f2` | Workspace roots, cwd inheritance, new/run/split/new-tab |
| #14 | `1754231` | Tab, pane, and workspace management actions |
| #15 | `467e2c7` | Copy mode: search, vi motions, scrollback, editor, pane read |
| #16 | `6320cc0` | Scripting parity: pane/tab/workspace/session verbs, wait, events, completions |
| #17 | `4991074` | Fuzzy workspace picker and goto picker overlays |
| #18 | `23eca6e` | Broader agent detection, state age, sticky done, codex/gemini |
| #19 | `f737b85` | Configurable sidebar: compact rail, per-workspace collapse, agents panel, swatches |
| #20 | `0fb8a24` | Binding arrays, global chords, config reload, settings overlay, config subcommands |
| #21 | `550e64e` | Bracketed paste, paste sanitizing, internal paste buffer |
| #22 | `f679157` | Git worktree workspaces, `prefix G`, worktree CLI, branch labels |
| #23 | `262036a` | Remote mode over an SSH-forwarded socket, protocol version handshake |
| #24 | this sweep | Review-nit verification, doc sync, outcomes (the clap `--version` and dependency bumps landed earlier in `4fd3be9`) |

### Human follow-ups

- **#8 / #11:** capture README screenshots of the Ködade theme and the status-bar
  widgets (colored panes, pane ids/cwd, window title) — the docs describe them
  but ship no images yet.
- **#18:** replace the placeholder prompt strings in the unverified agent
  manifests with real captured prompts (the shipped manifests for less-common
  agents were written from docs, not observed output).
- **#23:** run a live remote test over a real SSH-forwarded socket; only the
  local socket resolver and handshake are unit-tested, the tunnel path is not.
