# Configuration

Ködade CLI reads `~/.config/kodade-cli/config.toml`. A missing file, invalid
TOML, or omitted setting uses the defaults below.

## Settings

| Setting | Default | Description |
|---|---|---|
| `theme` | `"auto"` | `"auto"`, `"kodade-dark"`, `"kodade-light"`, `"tokyo-night"`, `"dark"`/`"light"` (aliases), or a user theme name. See [Themes](#themes). |
| `mouse` | `true` | Enable mouse capture and mouse interaction. Accepts a boolean or a `[mouse]` table. |
| `mouse.enabled` | `true` | Table form of `mouse`. |
| `mouse.copy_on_select` | `true` | Copy a mouse selection as soon as the drag ends. |
| `sidebar` | `true` | Show the sidebar when the TUI starts. |
| `notify` | `true` | Agent-state notifications. Accepts a boolean or a `[notify]` table. |
| `notify.enabled` | `true` | Table form of `notify`. |
| `keys.prefix` | `"ctrl+b"` | Prefix key pressed before a remappable action. |
| `status.right` | `["zoom", "blocked"]` | Right-side status bar widgets, in order. See [Status bar](#status-bar). |
| `ui.window_title` | `"Ködade · {workspace} · {tab}"` | Host terminal title template (OSC 0). See [Window title](#window-title). |

`mouse = true` and `[mouse]` are both valid, so a pre-0.2 config keeps working:

```toml
mouse = true            # still supported

[mouse]                 # equivalent, plus the new key
enabled = true
copy_on_select = false
```

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

| Action name | Default |
|---|---|
| `split_right` | `%` |
| `split_down` | `"` |
| `close_pane` | `x` |
| `new_tab` | `c` |
| `next_tab` | `tab` |
| `prev_tab` | `p` |
| `zoom` | `z` |
| `detach` | `d` |
| `rename` | `r` |
| `workspace_next` | `w` |
| `new_workspace` | `W` |
| `sidebar_toggle` | `b` |
| `focus_up` | `up`, `k` |
| `focus_down` | `down`, `j` |
| `focus_left` | `left`, `h` |
| `focus_right` | `right`, `l` |
| `resize_up` | `K` |
| `resize_down` | `J` |
| `resize_left` | `H` |
| `resize_right` | `L` |
| `navigate` | `n` |
| `copy_mode` | `[` |
| `select_tab_1` … `select_tab_9` | `1` … `9` |
| `close_tab` | `X` |
| `rename_tab` | `T` |
| `rename_workspace` | `R` |
| `close_workspace` | `D` |
| `swap_up` | `alt+k` |
| `swap_down` | `alt+j` |
| `swap_left` | `alt+h` |
| `swap_right` | `alt+l` |
| `move_tab_left` | unbound |
| `move_tab_right` | unbound |
| `next_pane` | `o` |
| `prev_pane` | `O` |
| `last_pane` | `;` |
| `workspace_prev` | unbound |
| `resize_mode` | `alt+r` |
| `break_pane` | `!` |
| `layout_even` | `=` |
| `reload_config` | `ctrl+r` |
| `settings` | `s` |
| `display_panes` | `q` |

`close_tab` asks for confirmation in the status bar when a pane in the tab is
working, and `close_workspace` when any agent in it is working or blocked;
`y` confirms, anything else cancels. `resize_mode` is a persistent mode:
`hjkl` resize by one cell, `HJKL` by five, and `esc` or `enter` exits.
`break_pane` moves the focused pane into a new tab without restarting it.

`reload_config` re-reads this file and the theme in place, and `settings`
opens the [settings menu](#settings-menu).

Example:

```toml
theme = "dark"
sidebar = true

[mouse]
enabled = true
copy_on_select = true

[keys]
prefix = "ctrl+space"
split_right = ["%", "ctrl+alt+v"]   # prefixed and global
focus_left = "prefix+alt+h"         # keep it behind the prefix
copy_mode = "F5"
```

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
