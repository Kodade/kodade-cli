# Local extensions

Ködade extensions are local directories with a `kodade-plugin.toml` manifest.
They run commands through your shell with no registry, telemetry, credential
proxy, or automatic installation.

```bash
kodade-cli plugin link ./examples/plugins/hello
kodade-cli plugin list
kodade-cli plugin run hello-plugin hello
kodade-cli plugin pane hello-plugin monitor
```

`link` records a canonical directory without copying it. `unlink` removes that
record only; it never deletes a linked directory. `install OWNER/REPO[/SUBDIR]`
clones a GitHub source into Ködade's managed plugin directory, validates its
manifest, optionally runs its declared build command, and records it. Only a
managed plugin can be removed by `uninstall`.

Plugins execute from the local client configuration. `plugin run` and `plugin
pane` deliberately reject `--remote`; install and invoke an extension on that
remote host instead of sending a local filesystem path over SSH.

## Manifest

```toml
manifest_version = 1
id = "hello-plugin"
name = "Hello plugin"
version = "0.1.0"
min_kodade_version = "0.2.1"

[[actions]]
id = "hello"
name = "Write greeting"
command = "printf 'hello from %s\\n' \"$KODADE_WORKSPACE\""
description = "Print a contextual greeting"

[[panes]]
name = "monitor"
command = "while true; do date; sleep 1; done"

[[startup]]
command = "printf 'started %s\\n' \"$KODADE_SESSION\""

[[events]]
event = "pane_opened"
command = "printf 'pane %s opened\\n' \"$KODADE_PANE\""
```

Action ids and plugin ids use lowercase letters, digits, `-`, and `_`.

Actions may declare `contexts = ["workspace", "tab", "pane", "selection"]`.
An action with no contexts remains globally available for compatibility. The
command center only shows an action when every declared context is available;
after selecting text, selection actions appear there too. A `global` context is
always available.

URL handlers claim ctrl-clicked links before the normal opener. The first
enabled manifest handler whose regular-expression `pattern` matches runs its
action; unmatched links keep the configured `ui.link_command` behavior.

```toml
[[link_handlers]]
id = "github-issue"
title = "Open issue helper"
pattern = "^https://github\\.com/.+/issues/[0-9]+$"
action = "inspect"
```
Ködade rejects unsupported manifest versions, malformed commands, duplicate
action ids, and registry/manifest mismatches before a command runs. Enabled
startup and event hooks are loaded by the daemon, so they continue while the
TUI is detached. Each hook has a 30-second limit and appends output to
`~/.config/kodade-cli/plugins/logs/<plugin>.log`.
Logs retain their most recent 256 KiB.

Actions appear in the command center (`prefix space`). A pane action opens a
new terminal pane; other actions run locally for at most 30 seconds. Commands
receive `KODADE_PLUGIN`, `KODADE_ACTION` (actions), `KODADE_EVENT` (hooks),
`KODADE_SESSION`, `KODADE_SOCKET`, `KODADE_WORKSPACE`, and `KODADE_PANE` when
that context exists. Context-aware actions also receive a private,
job-lifetime JSON path in `KODADE_PLUGIN_CONTEXT` (`KODADE_PLUGIN_CONTEXT_FORMAT=json`).
It contains endpoint, workspace/tab/pane ids, cwd, selected text, and clicked
URL as data; read the file rather than interpolating values into shell source.
Pane actions also receive `KODADE_TARGET_PANE`, the pane that was focused
before the new pane opened.

Run the fixture without network access after building:

```bash
scripts/plugin-smoke.sh
```
