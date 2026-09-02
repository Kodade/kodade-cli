# Configuration

Ködade CLI reads `~/.config/kodade-cli/config.toml`. A missing file, invalid
TOML, or omitted setting uses the defaults below.

## Settings

| Setting | Default | Description |
|---|---|---|
| `theme` | `"auto"` | `"auto"`, `"kodade-dark"`, `"kodade-light"`, `"tokyo-night"`, `"dark"`/`"light"` (aliases), or a user theme name. See [Themes](#themes). |
| `mouse` | `true` | Enable mouse capture and mouse interaction. Accepts a boolean or a `[mouse]` table. |
| `mouse.enabled` | `true` | Table form of `mouse`. |
| `mouse.capture` | `true` | Alias for `mouse.enabled`. `prefix m` toggles it for the session. |
| `mouse.copy_on_select` | `true` | Copy a mouse selection as soon as the drag ends. |
| `mouse.scroll_lines` | `3` | Rows scrolled per wheel notch (1–100). |
| `mouse.passthrough` | `true` | Send mouse events to pane apps that ask for the mouse (vim, lazygit, htop). |
| `mouse.clear_on_output` | `false` | Drop a selection when its pane redraws. |
| `sidebar` | `true` | Show the sidebar when the TUI starts. Accepts a boolean (alias for `[sidebar] show`) or a `[sidebar]` table. |
| `sidebar.show` | `true` | Table form of `sidebar`. |
| `sidebar.width` | `24` | Sidebar width in columns, clamped to 16–40. |
| `sidebar.collapsed` | `"compact"` | What `prefix b` and auto-hide collapse to: `"compact"` (a 3-column rail of workspace dots) or `"hidden"` (a 1-column gutter). |
| `sidebar.auto_hide_below` | `100` | Collapse the sidebar when the terminal is narrower than this many columns; restore it when widened. |
| `sidebar.agents_panel` | `true` | Show the agents panel below the workspaces list. |
| `notify` | `true` | Agent-state notifications. Accepts a boolean or a `[notify]` table. |
| `notify.enabled` | `true` | Master switch for notifications (table form of `notify`). |
| `notify.on` | `["blocked", "done"]` | Which agent states raise a notification. |
| `notify.toast` | `"status"` | How a notification shows: `"status"` (status-bar toast), `"off"` (no toast), or `"system"` (host-terminal desktop notification via OSC 777 / OSC 9). |
| `notify.bell` | `true` | Ring the terminal bell when a notification fires. |
| `notify.sound` | `""` | Command run through `sh -c` (detached) on each notification, e.g. `"afplay /System/Library/Sounds/Glass.aiff"`. Empty means no sound. |
| `notify.only_when_unfocused` | `true` | Skip the notification when its pane is already the focused pane on screen. |
| `paste.sanitize` | `true` | Strip escape sequences and control bytes from pasted text before it reaches a pane. Off leaves only bracketed-paste wrapping (no stripping). Accepts a `[paste]` table or a boolean. |
| `keys.prefix` | `"ctrl+b"` | Prefix key pressed before a remappable action. |
| `status.right` | `["zoom", "blocked"]` | Right-side status bar widgets, in order. See [Status bar](#status-bar). |
| `ui.window_title` | `"Ködade · {workspace} · {tab}"` | Host terminal title template (OSC 0). See [Window title](#window-title). |
| `ui.link_command` | `"open"` (macOS), `"xdg-open"` elsewhere | Program run with the URL of a ctrl/cmd-clicked link. See [Mouse](#mouse). |

When an agent transitions into a state listed in `notify.on`, the toast reads
`● codex blocked in kodade-cli/agents · prefix N to jump`; `prefix N`
(`notification_jump`; `N` because `o`/`O` are next/prev pane) focuses the pane
of the most recent unread notification and repeated presses walk back through
the stack.

```toml
[notify]
enabled = true
on = ["blocked", "done"]
toast = "status"
bell = true
sound = ""
only_when_unfocused = true
```

`mouse = true` and `[mouse]` are both valid, so a pre-0.2 config keeps working:

```toml
mouse = true            # still supported

[mouse]                 # equivalent, plus the new keys
enabled = true
copy_on_select = false
```

## Mouse

With `mouse.enabled` on, the mouse drives the whole client: click a tab, a
sidebar row, or a pane to focus it, drag a border to resize, right-click for
the context menu, and scroll to page a pane's scrollback
(`mouse.scroll_lines` rows per notch).

Inside a pane:

| Gesture | Result |
|---|---|
| Left drag | Selects text; the release copies it through OSC 52 when `mouse.copy_on_select` is on (works over SSH). |
| Double click | Selects the word under the pointer (`A-Za-z0-9_./~-`). |
| Triple click | Selects the whole line. |
| Ctrl-click / cmd-click | Opens the `http(s)://` token under the pointer with `ui.link_command`. |
| Single click | Focuses the pane and clears any selection. |

A selection clears on the next keystroke, when focus moves to another pane,
and — with `mouse.clear_on_output = true` — when the pane redraws.

When the program in a pane turns on mouse reporting (vim, lazygit, htop) and
`mouse.passthrough` is on, mouse events inside that pane are forwarded to it as
SGR (1006) sequences instead of selecting text. The tab bar, sidebar, pane
borders, and ctrl/cmd-clicks always stay with Ködade CLI.

`prefix m` toggles capture for the session without editing the config, which
hands the mouse back to the host terminal for its own selection and
right-click menu. The status bar confirms with
`mouse capture off · prefix m to re-enable`.

## Sidebar

The sidebar follows the same alias rule: a bare `sidebar = true` still works, or
use the `[sidebar]` table for the 0.2 options.

```toml
sidebar = true          # still supported (alias for [sidebar] show)

[sidebar]               # equivalent, plus the new keys
show = true
width = 24              # columns, clamped 16–40
collapsed = "compact"   # "compact" rail or "hidden" gutter
auto_hide_below = 100   # collapse under this terminal width
agents_panel = true
```

`prefix b` cycles the sidebar full → compact → hidden → full. Compact is a
3-column rail of one state dot per workspace; clicking a dot selects it. In the
full sidebar, `prefix n` (navigate) moves with `j`/`k`, `enter` folds/unfolds a
workspace (and selects it), `*` expands all, and the agents panel below the
workspaces lists every agent pane by urgency. Right-click a workspace for a
`Color…` menu that cycles its swatch through 8 presets; collapsed workspaces are
remembered per session in `~/.config/kodade-cli/state`.

Key overrides live under `[keys]`. Setting an action replaces its default
binding; the action's other default aliases are removed, and an empty array
(`zoom = []`) unbinds it. Unknown settings, unknown action names, invalid
chords, and a chord that takes a key away from another action are all reported
as warnings (see [`config validate`](#config-subcommands)); the rest of the
file still loads.

## Status bar

The status bar's left segment shows `session · workspace · tab` (the real
session name passed with `-s`). Mode prompts (rename, copy, navigate, resize,
prefix, and `y/n` confirms) temporarily replace the left segment.

`status.right` lists the widgets drawn at the right edge, in order. Unknown
names warn and are skipped.

| Widget | Shows |
|---|---|
| `zoom` | `[zoom]` while a pane is zoomed. |
| `blocked` | `● N blocked` (in the blocked color) when N panes across all workspaces are blocked. |
| `hostname` | The local host name. |
| `time` | Local `HH:MM`. |

`prefix q` briefly flashes each pane's `#id` (like tmux `display-panes`), so you
can `kodade-cli send <id>` without an `ls` first. Pane borders also carry
`#id name — state` on the left and the cwd basename on the right when the pane
is wide enough.

## Window title

On attach — and whenever the active workspace or tab changes — the host
terminal title is set via OSC 0 from `ui.window_title`. Placeholders
`{session}`, `{workspace}`, and `{tab}` are substituted. Nothing is restored on
exit; terminals reset their own title.

## Remappable actions

The following names are accepted in `[keys]`. Defaults are shown as the key or
keys pressed after the prefix. `focus_*` and `resize_*` intentionally have
single-letter and arrow-key aliases.

This table is generated from the default bindings; run `kodade-cli keys` for
your live config. `prefix+…` chords are prefix-relative; a chord with a bare
modifier (`ctrl+…`/`alt+…`) fires globally.

<!-- keys:start -->
| Action name | Default |
|---|---|
| `split_right` | `%` |
| `split_down` | `"` |
| `close_pane` | `x` |
| `zoom` | `z` |
| `rename` | `r` |
| `focus_up` | `k`, `up` |
| `focus_down` | `down`, `j` |
| `focus_left` | `h`, `left` |
| `focus_right` | `l`, `right` |
| `resize_up` | `K` |
| `resize_down` | `J` |
| `resize_left` | `H` |
| `resize_right` | `L` |
| `swap_up` | `prefix+alt+k` |
| `swap_down` | `prefix+alt+j` |
| `swap_left` | `prefix+alt+h` |
| `swap_right` | `prefix+alt+l` |
| `next_pane` | `o` |
| `prev_pane` | `O` |
| `last_pane` | `;` |
| `break_pane` | `!` |
| `layout_even` | `=` |
| `display_panes` | `q` |
| `new_tab` | `c` |
| `next_tab` | `tab` |
| `prev_tab` | `p` |
| `select_tab_1` | `1` |
| `select_tab_2` | `2` |
| `select_tab_3` | `3` |
| `select_tab_4` | `4` |
| `select_tab_5` | `5` |
| `select_tab_6` | `6` |
| `select_tab_7` | `7` |
| `select_tab_8` | `8` |
| `select_tab_9` | `9` |
| `close_tab` | `X` |
| `rename_tab` | `T` |
| `move_tab_left` | unbound |
| `move_tab_right` | unbound |
| `workspace_next` | `prefix+alt+w` |
| `workspace_picker` | `w` |
| `new_workspace` | `W` |
| `worktree_new` | `G` |
| `rename_workspace` | `R` |
| `close_workspace` | `D` |
| `workspace_prev` | unbound |
| `goto` | `g` |
| `navigate` | `n` |
| `copy_mode` | `[` |
| `resize_mode` | `prefix+alt+r` |
| `settings` | `s` |
| `help` | `?` |
| `detach` | `d` |
| `sidebar_toggle` | `b` |
| `reload_config` | `prefix+ctrl+r` |
| `paste_buffer` | `]` |
| `mouse_toggle` | `m` |
| `notification_jump` | `N` |
<!-- keys:end -->

`close_tab` asks for confirmation in the status bar when a pane in the tab is
working, and `close_workspace` when any agent in it is working or blocked;
`y` confirms, anything else cancels. `resize_mode` is a persistent mode:
`hjkl` resize by one cell, `HJKL` by five, and `esc` or `enter` exits.
`break_pane` moves the focused pane into a new tab without restarting it.

`workspace_picker` (`prefix w`) opens a fuzzy switcher over every workspace,
and `goto` (`prefix g`) opens a fuzzy palette over every workspace, tab, and
agent pane. Type to filter (a subsequence match that favours word starts and
runs), `ctrl+n`/`ctrl+p` or the arrows move, `enter` jumps, `esc` closes;
blocked entries always sort to the top. Cycling straight to the next workspace
without the picker is `workspace_next` (`prefix alt+w`); `workspace_prev`
ships unbound — bind it to a bare `alt+…` chord to cycle backwards globally.

`reload_config` re-reads this file and the theme in place, and `settings`
opens the [settings menu](#settings-menu).

`paste_buffer` re-sends the last paste (or copy-mode yank, or mouse selection) into the focused
pane; it reports `paste buffer empty` when nothing has been copied yet. See
[Paste](#paste).

Example:

```toml
theme = "dark"
sidebar = true

[mouse]
enabled = true
copy_on_select = true
scroll_lines = 3
passthrough = true

[ui]
link_command = "open"

[keys]
prefix = "ctrl+space"
split_right = ["%", "ctrl+alt+v"]   # prefixed and global
focus_left = "prefix+alt+h"         # keep it behind the prefix
copy_mode = "F5"
```

## Paste

Ködade CLI turns on [bracketed paste](https://cirw.in/blog/bracketed-paste) so
a program in the focused pane can tell a paste from typing. Before the bytes
reach the pane the client sanitizes them (when `paste.sanitize` is on, the
default): it normalizes `\r\n` to `\n`, drops any embedded escape sequences —
including a smuggled OSC 52 clipboard write or cursor-moving CSI — and strips
C0 control bytes other than tab and newline. Large pastes are split into 64 KB
chunks and paced so the daemon is not flooded. Turning `paste.sanitize` off
keeps the bracketed-paste framing but sends the text unchanged.

```toml
[paste]
sanitize = true         # default; false wraps only, no stripping
```

The last paste (or copy-mode yank, or mouse selection) is kept in an internal buffer that
`paste_buffer` (`]` by default) re-sends into the focused pane.

## Session persistence

The daemon persists each session's layout and restores it on a cold start (see
[DEVELOPMENT.md](DEVELOPMENT.md#session-persistence-and-restore)). One key in
the same `config.toml`, read by the daemon, controls restore behavior:

| Setting | Default | Description |
|---|---|---|
| `session.resume_agents` | `false` | When `true`, a restored pane whose saved command matches an agent manifest with a `resume` string re-runs that resume command (e.g. `codex resume --last`) instead of starting a plain shell. Panes with no matching manifest always restore as shells. |

```toml
[session]
resume_agents = true
```

## Git worktrees

`prefix G` (the `worktree_new` action) and `kodade-cli worktree add BRANCH`
open a git-worktree workspace: the daemon runs `git worktree add` for the
active workspace's repository and opens a `repo:branch` workspace rooted in the
new worktree. One key, read by the daemon, sets where worktrees are created:

| Setting | Default | Description |
|---|---|---|
| `worktrees.directory` | `~/.kodade/worktrees` | Root directory new worktrees are created under, as `<directory>/<repo-name>/<branch>`. A leading `~` expands to your home directory. |

```toml
[worktrees]
directory = "~/.kodade/worktrees"
```

Closing a worktree workspace (`prefix D` or the sidebar menu) asks
`remove worktree <branch>? y/n/k(eep)`: `y` runs `git worktree remove`, `k`
closes the workspace but leaves the directory, `n` cancels. Ködade only ever
removes a directory that git reports as a registered worktree.

## Key-chord syntax

A chord is `[prefix+][ctrl+][alt+][shift+]key`; modifiers may appear in any
order. Key names:

- a single character (`%`, `x`, `[`, `=`)
- `enter`, `esc`, `space`, `backspace`, `tab`, `home`, `end`, `pageup`,
  `pagedown`, `delete`
- `up`, `down`, `left`, `right`
- `F1` through `F12`

`shift+` on a letter is the same binding as the uppercase letter — `shift+x`
and `X` are identical — so `l` and `L` stay separate bindings. On other keys
`shift+` is kept as a modifier (`shift+tab`). Multi-key sequences (`"g t"`) are
still not supported.

## Binding arrays and global chords

A binding value is one chord or an array of chords:

```toml
[keys]
split_right = ["%", "ctrl+alt+v"]
```

A chord that carries `ctrl` or `alt` and is *not* written as `prefix+…` is a
**global** chord: it fires on its own, without the prefix, and the pane never
sees the key. Everything else is prefix-relative. So in the example above,
`prefix %` and a bare `ctrl+alt+v` both split right.

Built-in defaults are always prefix-relative, including the ones printed as
`alt+k` in the table above. To rebind one and keep it behind the prefix, write
`prefix+alt+k`; writing `alt+k` makes it global.

Global chords are inert while a mode or overlay is active — rename, copy mode,
navigate, resize mode, a context menu, a confirmation, or the settings menu all
see the key first.

## Live reload

`prefix ctrl+r` (`reload_config`) re-reads `config.toml` and the theme file
without detaching: new bindings, mouse setting, and colors apply immediately.
A broken config keeps the previous one and shows
`config error: … · previous config kept` in the status bar for five seconds.

`theme = "auto"` is resolved once at startup, because its terminal query cannot
run while the TUI owns the terminal; reload keeps the current theme in that
case. Name the theme explicitly to have reload recolor.

## Settings menu

`prefix s` opens the settings overlay: theme (cycles `auto`, the built-ins, and
every theme in `~/.config/kodade-cli/themes/`), mouse, sidebar, copy on select,
and notifications. Enter toggles or cycles the highlighted row, applies it
immediately, and writes it back to `config.toml`. Comments, formatting, and
keys the menu does not know about are preserved; the file is created if it does
not exist yet. `j`/`k`, arrows, or ctrl+n/ctrl+p move, `esc` or `q` closes.

## Config subcommands

| Command | Behavior |
|---|---|
| `kodade-cli config path` | Prints the config file path. |
| `kodade-cli config show` | Prints the effective config (defaults merged) as TOML. |
| `kodade-cli config validate` | Prints every warning and exits `1` when the file has problems, `0` when it is clean. A missing file prints `not found (defaults in use)` and exits `0`. |

## Upgrading from 0.1

The chord grammar changed, so three things behave differently:

- **Bare modifier chords are now global.** `focus_left = "alt+h"` used to mean
  `prefix alt+h`; it now fires without the prefix, and `prefix alt+h` falls
  through to its default, `swap_left`. Write `focus_left = "prefix+alt+h"` to
  keep the 0.1 behavior.
- **`prefix = "ctrl+space"` now works.** Multi-character key names are
  supported, so this no longer falls back to `ctrl+b`.
- **`F0` is gone.** Function keys are `F1` through `F12`.

Everything else is compatible: `mouse = true`, single-chord strings, and
uppercase letters (`L`) all keep their meaning.

## Themes

Built-in themes: `kodade-dark`, `kodade-light`, and `tokyo-night` (the old
Tokyo Night palette, kept as an extra). The Ködade themes are warm neutrals
with the Ködade amber accent `#E7A33B` and a purple-free ANSI palette.

`theme` accepts:

| Value | Resolves to |
|---|---|
| `"auto"` (default) | Queries the terminal background (OSC 11) and picks `kodade-dark` or `kodade-light`. This is the only value that queries the terminal. |
| `"dark"` | Alias for `kodade-dark`. |
| `"light"` | Alias for `kodade-light`. |
| `"kodade-dark"` / `"kodade-light"` / `"tokyo-night"` | The named built-in. |
| any other name | `~/.config/kodade-cli/themes/<name>.toml`, falling back to `kodade-dark` if missing or invalid. |

Built-in names resolve before the user themes directory.

### Theme file schema

Colors are six-digit `#RRGGBB`. The **ten required fields** are `name`,
`accent`, `border`, `text`, `dim`, `blocked`, `working`, `done`, `idle`,
`tabbar_bg`, `status_bg`. State colors: `blocked` = coral red, `working` =
sage green, `done` = amber, `idle` = dim.

Every other field is **optional** and falls back to an existing field, so a
theme with only the ten fields still loads:

| Optional field | Fallback |
|---|---|
| `bg` | `tabbar_bg` |
| `surface` | `status_bg` |
| `selection` | `border` |
| `cursor` | `accent` |
| `menu_bg` | `status_bg` |
| `menu_fg` | `text` |
| `tab_active_fg` | `accent` |
| `tab_active_bg` | `tabbar_bg` |
| `sidebar_bg` | `tabbar_bg` |
| `[ansi]` table | standard xterm 16-color palette |

The optional `[ansi]` table has sixteen keys: `black`, `red`, `green`,
`yellow`, `blue`, `magenta`, `cyan`, `white`, `bright_black`, `bright_red`,
`bright_green`, `bright_yellow`, `bright_blue`, `bright_magenta`,
`bright_cyan`, `bright_white`. Any missing key falls back to that xterm slot.

`[ansi]` drives pane colors: the 16 basic colors a program prints inside a pane
are drawn from this palette, so themes restyle terminal output without touching
the program. Colors 16–255 use the terminal's own 256-color cube and 24-bit
colors are passed through unchanged. A cell with no color set uses `text` on
`bg`.

The complete built-in `kodade-dark` theme:

```toml
name = "kodade-dark"
accent = "#E7A33B"
border = "#3a3733"
text = "#d6d2c9"
dim = "#a5a096"
blocked = "#d97a80"
working = "#a8c87f"
done = "#E7A33B"
idle = "#a5a096"
tabbar_bg = "#232120"
status_bg = "#232120"

bg = "#2a2825"
surface = "#232120"
selection = "#454038"
cursor = "#e2b86e"
menu_bg = "#232120"
menu_fg = "#d6d2c9"
tab_active_fg = "#E7A33B"
tab_active_bg = "#38352f"
sidebar_bg = "#232120"

[ansi]
black = "#2a2825"
red = "#d97a80"
green = "#a8c87f"
yellow = "#e2b86e"
blue = "#7fa3e0"
magenta = "#d98a5b"
cyan = "#7fc4d6"
white = "#d6d2c9"
bright_black = "#a5a096"
bright_red = "#e5949a"
bright_green = "#bcd89a"
bright_yellow = "#efce8f"
bright_blue = "#9db9e8"
bright_magenta = "#e5a67d"
bright_cyan = "#9dd4e2"
bright_white = "#f0ece3"
```
