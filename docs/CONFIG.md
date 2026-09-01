# Configuration

Ködade CLI reads `~/.config/kodade-cli/config.toml`. A missing file, invalid
TOML, or omitted setting uses the defaults below.

## Settings

| Setting | Default | Description |
|---|---|---|
| `theme` | `"auto"` | Select `"dark"`, `"light"`, `"auto"`, or a user theme name. |
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

The built-in choices are `dark`, `light`, and `auto`; the default is `auto`.
Auto queries the terminal background and selects the light or dark built-in
theme. Any other `theme` value is loaded as
`~/.config/kodade-cli/themes/<name>.toml`, falling back to dark if the file is
missing or invalid.

A theme file must contain every field below. Colors are six-digit `#RRGGBB`
values. This is the complete built-in dark theme:

```toml
name = "dark"
accent = "#7aa2f7"
border = "#3b4261"
text = "#c0caf5"
dim = "#565f89"
blocked = "#f7768e"
working = "#7aa2f7"
done = "#9ece6a"
idle = "#565f89"
tabbar_bg = "#1a1b26"
status_bg = "#1a1b26"
```
