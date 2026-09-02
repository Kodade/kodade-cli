# Configuration

Ködade CLI reads `~/.config/kodade-cli/config.toml`. A missing file, invalid
TOML, or omitted setting uses the defaults below.

## Settings

| Setting | Default | Description |
|---|---|---|
| `theme` | `"auto"` | `"auto"`, `"kodade-dark"`, `"kodade-light"`, `"tokyo-night"`, `"dark"`/`"light"` (aliases), or a user theme name. See [Themes](#themes). |
| `mouse` | `true` | Enable mouse capture and mouse interaction. |
| `sidebar` | `true` | Show the sidebar when the TUI starts. |
| `keys.prefix` | `"ctrl+b"` | Prefix key pressed before a remappable action. |

Key overrides live under `[keys]`. Setting an action replaces its default
binding; the action's other default aliases are removed. Unknown action names
and invalid chords are ignored with a warning.

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

`close_tab` asks for confirmation in the status bar when a pane in the tab is
working, and `close_workspace` when any agent in it is working or blocked;
`y` confirms, anything else cancels. `resize_mode` is a persistent mode:
`hjkl` resize by one cell, `HJKL` by five, and `esc` or `enter` exits.
`break_pane` moves the focused pane into a new tab without restarting it.

Example:

```toml
theme = "dark"
mouse = true
sidebar = true

[keys]
prefix = "ctrl+space"
split_right = "s"
focus_left = "alt+h"
copy_mode = "F5"
```

## Key-chord syntax

Chords are one optional `ctrl+` or `alt+` modifier followed by one character,
`up`, `down`, `left`, `right`, or a function key from `F0` through `F9`.
`shift+` and multi-key sequences are not supported. Uppercase characters are
distinct keys, so `L` and `l` can be separate bindings.

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
