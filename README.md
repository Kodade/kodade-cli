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
| `r` / `w` / `W` | Rename / next workspace / new workspace (`W` prompts for `NAME [PATH]`) |
| `b` / `[` | Toggle sidebar / copy mode |
| `h` `j` `k` `l` or arrows | Focus left, down, up, right |
| `H` `J` `K` `L` | Resize left, down, up, right |
| `1`–`9` | Jump to that tab |
| `X` / `T` | Close / rename tab |
| `D` / `R` | Close / rename workspace |
| `alt+h` `alt+j` `alt+k` `alt+l` | Swap the pane with its neighbour |
| `o` / `O` / `;` | Next / previous / last pane |
| `!` / `=` | Break pane to a new tab / equalize the layout |
| `alt+r` | Resize mode (`hjkl` 1 cell, `HJKL` 5, `esc` exits) |
| `s` / `ctrl+r` | Settings menu / reload config and theme |

Mouse is enabled by default: click panes, tabs, and sidebar rows to focus;
drag pane borders to resize; scroll over a pane to scroll; right-click a pane,
tab, or workspace for its menu; the pane menu can break a pane out to its own
tab or equalize the layout, and the tab menu can reorder tabs. In navigate mode, `j`/`k` move through the
sidebar and `enter` activates a row; `esc` exits.

Copy mode uses `v` to set a selection anchor, movement keys to select, and `y`
to copy. Copying sends the selection through OSC 52, including over SSH; copy
payloads are limited to 100 KB.

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
- `kodade-cli agent ls` — list recognized agents and states.
- `kodade-cli agent attach PANE` — focus a pane and attach the TUI.
- `kodade-cli agent rename PANE NAME` — rename a pane.
- `kodade-cli agent explain PANE` — print a pane's state, reason, and the bottom-8-line window it matched.
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

`ls`, `agent ls`, and `agent explain` also accept `--json`, which prints the
matching protocol snapshots for scripts.

`kodade-cli integrate list` shows the available integrations.
`kodade-cli integrate <agent>` prints the hook/notify settings and
`--write` installs them: `claude-code` and `gemini-cli` merge hooks into their
`settings.json`; `codex` merges a `notify` entry into `~/.codex/config.toml`
(add `--force` to replace an existing one). See
[docs/AGENT-DETECTION.md](docs/AGENT-DETECTION.md) for details.

See [docs/CONFIG.md](docs/CONFIG.md), [docs/AGENT-DETECTION.md](docs/AGENT-DETECTION.md),
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md), and [docs/RELEASING.md](docs/RELEASING.md)
for reference and contributor details. The product direction is in
[docs/PRD.md](docs/PRD.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
