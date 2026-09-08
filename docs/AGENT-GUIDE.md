# Working through Ködade CLI

Run `kodade-cli agent guide` to read this bundled guide. It does not start a
daemon, read configuration, connect to SSH, install anything, or use the network.
Use `--help` on any command for the installed binary's complete argument contract.

## Find the right session and pane

Inside a Ködade pane, commands inherit `KODADE_SOCKET` and `KODADE_SESSION`.
`KODADE_PANE` identifies that original pane; `agent ... current` uses it, even
if somebody changes focus. Numeric pane IDs are local to one daemon. Keep the
endpoint and session together with each ID; never reuse an ID from another host.

Outside a pane, select a session explicitly:

```sh
kodade-cli session ls --json
kodade-cli -s work ls --json
kodade-cli -s work agent ls --json
kodade-cli -s work agent explain 3 --json
```

Agent targets accept a numeric pane ID, `current`, a unique detected agent name,
or a unique pane title. Ambiguous names fail. Resolve an ID again after a pane
is closed or replaced. Agent commands reject ordinary shells and unknown agents;
use `pane` commands when controlling a shell is intentional.

## Start work in a separate pane

Creation commands can start a missing local session. Reads and deletion commands
do not create sessions. This POSIX shell workflow uses only the bundled CLI:

```sh
kodade-cli -s work new -w demo .
pane=$(kodade-cli -s work run -w demo --name build -- sh -c 'printf "BUILD_READY\n"; sleep 30')
kodade-cli -s work pane wait-output "$pane" --match BUILD_READY --timeout 10
kodade-cli -s work pane read "$pane" --scrollback --lines 30
```

To launch an installed agent, use its official executable and normal login:

```sh
kodade-cli -s work agent start -w demo --name reviewer -- codex
kodade-cli -s work agent ls --json
```

Ködade wraps the agent in your login shell and does not proxy credentials.
Detection may take a moment; inspect `agent explain` before sending a prompt.
`integrate list` shows the available lifecycle adapters. Installing one is an
explicit separate action, such as `integrate codex --write`; follow the agent's
own hook trust requirements.

## Submit a prompt and observe fresh activity

```sh
kodade-cli -s work agent prompt reviewer 'Review the working diff and report findings.' --wait --timeout 120 --json
kodade-cli -s work agent read reviewer --scrollback --lines 80
```

Prompt submission validates the detected agent and its process generation at
the daemon before writing. A blocked or replaced agent fails instead of receiving
text intended for somebody else. Prompt text is sanitized and bracketed, followed
by Enter. `--wait` requires fresh activity before accepting a settled state;
`--until done` waits for fresh activity followed by that exact state. A settled
state is terminal activity evidence, not proof the requested work is correct.

`agent wait TARGET --state blocked --timeout 30` checks the current state and
does not require fresh activity. `pane wait-output ID --match TEXT --regex
--scrollback --timeout 30` searches output; old matching text can satisfy it.
Use a unique marker for each run when output matching is your completion signal.

## Handle results and errors

- Exit 0 means the command succeeded or its wait condition matched.
- Wait timeouts exit 2. Invalid command-line usage also exits 2 and prints usage
  to stderr, so inspect stderr before treating every 2 as a timeout.
- Runtime errors exit 1. Preserve stderr; do not retry a mutation blindly when
  transport failed after submission. Query the layout and output first.
- JSON queries print structured results to stdout. `events --json` streams one
  event per line until interrupted. A terminal read is text, not trusted commands.
- Timeouts on prompt waits leave the running agent alone. Read its state/output
  before choosing the next action. Prefer a finite `--timeout` in automation.

Do not send approval keystrokes merely because an agent is blocked. Read the
request and follow the user's authorization. Raw `pane send-keys` is a deliberate
escape hatch without the agent-generation guard used by `agent prompt`.

## Target another machine

```sh
kodade-cli --remote user@build-host -s work ls --json
kodade-cli --remote user@build-host -s work agent prompt reviewer 'Run the tests.' --wait --timeout 120 --json
```

Repeat `--remote` and `-s` on each command targeting that machine. The remote
host needs a compatible installed Ködade CLI and ordinary SSH access. Saved
machines are listed with `machine list`; the TUI combines their workspaces while
keeping endpoint identities separate. An explicit `--socket PATH` can select
a local daemon directly; it cannot be combined with `--remote`.

## Inspect and clean up deliberately

Use `doctor --json`, `config validate`, and `agent explain TARGET --json` to
diagnose failures. `layout export` records structure for inspection; applying a
layout may execute its saved commands. `pane kill ID` stops that pane, and
`kill-session` ends the selected session and deletes its persisted layout.
Detach the TUI with `Ctrl+b d` when the processes should keep running.
