# Machines

Save an SSH host once, then browse its workspaces, tabs, and agents alongside
your local session. Each host owns its connection, cached layout, unread alerts,
and collapse state. OpenSSH uses your existing host aliases, keys, agent, and
host-key configuration.

```sh
kodade-cli machine add buildbox --label Build --remote-session agents
kodade-cli machine list
kodade-cli
```

Click a machine or any of its workspace/tab/pane rows to switch. `prefix M`
cycles enabled machines; `prefix n` navigates the combined sidebar with the
keyboard. The command center and normal pane actions act on the selected
machine. Local extension actions explain their local-only scope when a remote
machine is selected.

```sh
kodade-cli machine disable Build
kodade-cli machine enable Build
kodade-cli machine rename Build --label Builder
kodade-cli machine remove Builder
```

These commands accept a profile id or its exact label. Changes take effect in
attached clients within a second. Disabling/removing a profile closes its
client connection and leaves the remote daemon and processes running. If that
machine was selected, the client returns to local. A failed host reconnects
independently with increasing delays; its last layout remains visible and input
is discarded with a notice while disconnected. Queued messages from a replaced
connection cannot update its replacement's state. Recoverable command errors
keep the live connection usable.

Profiles are stored beside UI state in `~/.config/kodade-cli/machines.toml`.
The file is validated and written atomically. An omitted remote session uses
the local session name. For one-shot commands or a single remote attachment,
use `kodade-cli --remote buildbox -s agents ...`; that mode does not load the
local machine catalog. `machine list --json` exposes the saved catalog for
scripts. The remote host needs a compatible Ködade binary available on PATH.

Verification includes separate localhost OpenSSH client/server environments,
concurrent tunnels, a real controlling PTY switching between colliding local
and remote pane ids, live profile disable, and an unreachable host. Unit tests
cover endpoint-scoped notification/collapse state and rejecting messages from
retired connection generations.
