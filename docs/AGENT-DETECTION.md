# Agent detection

Ködade CLI selects one of five states for each pane. States are also rolled
up to parent tabs and workspaces by urgency, with `blocked` taking priority,
then `working`, `done`, `idle`, and `unknown`.

- `blocked`: The agent is waiting for user attention, such as a permission,
  approval, or question prompt. Screen manifests must use a confident match
  before producing this state.
- `working`: The agent is actively producing output, or a hook or manifest
  explicitly reports this state.
- `done`: The agent's turn finished. Lifecycle "turn ended" hooks report this
  (`Stop` for Claude Code, `AfterAgent` for Gemini CLI, `Stop` for Codex), so it marks a
  completed turn rather than a decayed working state. The current built-in
  manifests do not define a `done` screen rule. A hook-reported `done`
  *sticks*: unlike the other states it has no TTL and stays until the pane
  prints new PTY output or the next hook report arrives (see "Authority and
  fallback"). The sidebar shows how long it has held.
- `idle`: A recognized agent has no matching screen rule and has produced no
  output in the working window. An ordinary shell also uses `idle`.
- `unknown`: The foreground process is not a recognized agent and is not one
  of the supported shell names (`sh`, `bash`, `zsh`, `fish`, or `nu`).

## Authority and fallback

Detection uses the highest available authority in this order:

1. **Lifecycle hook.** A hook report wins when it is still current. For
   `blocked`, `working`, and `idle` reports that means no more than 30 seconds
   old — the daemon constant is `HOOK_TTL = Duration::from_secs(30)`, and a
   report exactly 30 seconds old is still valid. A `done` report has **no
   TTL**: it sticks until the pane produces new PTY output (or a newer hook
   report replaces it), matching the "unviewed done" behavior of the desktop
   app. The report supplies the state and a source string. An expired report
   is ignored. The daemon also tracks how long the current state has held
   (`state_since`) and exposes it as `state_age_secs` for the sidebar.
2. **Screen manifest.** Ködade first identifies a known agent from its
   foreground process name or terminal title. It then examines only the
   bottom eight screen lines (`SCREEN_LINES = 8`) and applies the first rule
   whose `any` value contains a matching string. A rule can report `blocked`,
   `working`, or `done`.
3. **Process heuristic.** For a known agent with no matching screen rule,
   recent terminal output means `working`; otherwise the state is `idle`.
   The daemon constant is
   `OUTPUT_WORKING_WINDOW = Duration::from_secs(2)`. The comparison is
   strictly less than two seconds, so output age of exactly two seconds is
   idle. If no manifest identifies the process, a shell is idle and any other
   process is unknown.

The process matcher compares names exactly. Title matchers use substring
matching. Screen strings and title strings are case-sensitive.

## Manifest format

Manifests are TOML files. `name` is the stable replacement key; `display` is
the name shown in the UI. `process` and `title` are optional arrays. Each
`[[rule]]` has a state and an `any` array. Strings in one `any` array are
alternatives, and rules are checked in file order.

```toml
# Stable identifier. A user file with the same name replaces this manifest.
name = "example-agent"

# Human-readable label shown for a detected agent.
display = "Example Agent"

# Optional: command that resumes the agent's last session (metadata only).
resume = "example-agent --continue"

# Exact foreground process basenames that identify the agent.
process = ["example-agent", "example"]

# Case-sensitive substrings that identify the agent from the terminal title.
title = ["Example Agent"]

# Every rule is checked against only the bottom eight screen lines.
# This rule marks the pane blocked if any one of these strings is present.
[[rule]]
state = "blocked"
any = ["Allow access?", "Approve", "y/n"]

# This rule marks the pane working when the agent's working indicator appears.
[[rule]]
state = "working"
any = ["esc to interrupt"]

# A done rule is valid too, although no current built-in manifest uses one.
[[rule]]
state = "done"
any = ["Task complete"]
```

`state` must be `blocked`, `working`, or `done`. A manifest with no matching
rule falls through to the two-second output heuristic. A rule with broad or
ambiguous text can create false blocked alarms; do not add one unless the
match is specific to an attention prompt.

## Built-in manifests and overrides

Built-ins are source files in
`crates/kodade-cli-daemon/manifests/` and are embedded into the binary by
`crates/kodade-cli-daemon/src/manifest.rs`. "Verified" means the process,
title, and screen strings were checked against a real local session;
"unverified" manifests were sourced from the CLI's public docs (the source URL
is noted at the top of each file) and ship with process/title identification
only — no screen rules — until a real session confirms their prompt strings.

Verified:

- Claude Code (`claude-code.toml`) — blocked/working rules, `resume = "claude --continue"`
- Codex (`codex.toml`) — blocked/working rules, `resume = "codex resume --last"`
- Grok Build (`grok.toml`) — blocked rule, `resume = "grok --continue"`
- OpenCode (`opencode.toml`) — blocked/working rules
- Gemini CLI (`gemini-cli.toml`) — blocked rule, `resume = "gemini --resume latest"`
- Aider (`aider.toml`) — blocked rule

Unverified (identification only, no screen rules):

- Cursor CLI (`cursor-agent.toml`)
- GitHub Copilot CLI (`copilot.toml`)
- Cline (`cline.toml`)
- Amp (`amp.toml`)
- Droid / Factory (`droid.toml`)
- Kimi CLI (`kimi.toml`)
- Qwen Code (`qwen-code.toml`)
- Pi (`pi.toml`)
- Hermes (`hermes.toml`)

None of the built-ins define a `done` screen rule: the interactive agents
return to their prompt on completion with no distinctive, stable footer, so
`done` is reported through lifecycle hooks instead (`codex exec` prints a
`tokens used` footer, but that is the non-interactive path, not the TUI panes
Ködade hosts).

### `resume`

A manifest may set an optional `resume` string — the command that resumes the
agent's most recent session (for example `codex resume --last`). Session
restore uses it to relaunch an agent where it left off. It is metadata only and
does not affect state detection.

### The `y/n` caveat

`y/n` is a deliberately broad `blocked` string used by several agents. It is
only ever consulted for a pane whose identified process matches that agent, so
an ordinary shell prompt does not trip it. The one known false positive: a
shell prompt that reads `... y/n` *inside a Claude Code Bash tool* runs under
the `claude` process, so the `claude-code` manifest can briefly read that pane
as `blocked`. This is accepted as the price of catching real Claude approval
prompts; it clears as soon as the prompt scrolls past the bottom eight lines.

User manifests are loaded from:

```text
~/.config/kodade-cli/agent-detection/*.toml
```

Only files ending in `.toml` are read. A user manifest whose `name` equals a
built-in `name` replaces that built-in. A different `name` adds another
manifest. The replacement is complete: fields are not merged with the
built-in, so copy any built-in rules you want to retain. Invalid TOML or an
invalid manifest prevents the manifest load from succeeding. The current
implementation loads manifests when the daemon session is created; changing
a file does not yet hot-reload an already running session.

## Contributing a manifest

A new agent manifest is a PR-sized change:

1. Add a TOML file under
   `crates/kodade-cli-daemon/manifests/`.
2. Add its `include_str!` entry to `builtin()` in
   `crates/kodade-cli-daemon/src/manifest.rs`.
3. Use a stable `name`, a clear `display`, exact process names where known,
   and narrow title or screen strings.
4. Add focused parsing or detection coverage when the matching behavior is
   not already covered.
5. Run `cargo fmt`, `cargo test`, and `cargo clippy` before opening the PR.

Matching is deliberately conservative. Prefer `idle` or `unknown` over a
false `blocked` alarm. Mark a pane `blocked` only when the screen text is a
confident indication that the agent is waiting for user action. Avoid generic
words such as `error`, `continue`, or `ready` unless they are part of a
distinctive prompt.

## CLI commands

### `agent explain <pane>`

Prints the chosen state, the reason (which names the matched needle), the
detected agent, and the bottom eight screen lines the detector examined — the
same window the daemon matches against. Use it to see exactly why a pane read
as `blocked` or `idle`.

### `integrate`

Installs the lifecycle hook / notify entry that lets an agent self-report its
state, so detection does not depend on screen strings alone.

- `integrate list` — shows each known integration, its config file, and
  whether that config directory exists on this machine.
- `integrate claude-code [--write]` — merges hook entries into
  `~/.claude/settings.json` (`Stop` → done, `UserPromptSubmit` → working,
  `Notification` → blocked). Without `--write` it prints the snippet. The
  `Stop` event fires when the agent's turn ends, so it reports `done` (which
  then sticks) rather than `idle`.
- `integrate gemini-cli [--write]` — installs Gemini's `BeforeAgent` → working,
  `AfterAgent` → done, and `Notification` → blocked in `~/.gemini/settings.json`,
  following its [official hook events](https://geminicli.com/docs/hooks/).
  Earlier Ködade hooks under Claude event names are removed while unrelated
  hooks and their matcher metadata are preserved.
- `integrate codex [--write]` — installs Ködade-owned lifecycle commands in
  `~/.codex/hooks.json`: `UserPromptSubmit` → working, `Stop` → done, and
  `PermissionRequest` → blocked. It never replaces the separate legacy
  `notify` setting in `~/.codex/config.toml`. Codex must have hooks enabled
  for the workspace under its normal trust/settings policy; if it does not run
  the commands, Ködade continues with process and screen detection. Use
  `integrate codex --remove` to remove only Ködade's marked commands.
- `integrate copilot [--write]` — writes Ködade's separate, versioned user hook
  file at `~/.copilot/hooks/kodade-cli.json`: `userPromptSubmitted` → working,
  `agentStop` → done, and `errorOccurred` → blocked. Copilot documents those
  events and its `sessionId` payload in its [hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference), and resumes the exact reported ID with `copilot --resume=ID`.
- `integrate cursor [--write]` — merges `sessionStart` and
  `beforeSubmitPrompt` → working plus `stop`/`sessionEnd` → done into
  `~/.cursor/hooks.json`. Cursor documents that user hooks receive JSON on
  stdin and provides the `session_id` payload and `cursor-agent --resume ID`
  command in its [hooks](https://prod.cursor.com/docs/hooks) and [CLI parameter](https://docs.cursor.com/en/cli/reference/parameters) references.
- `integrate droid [--write]` — merges `SessionStart`/`UserPromptSubmit` →
  working, `Stop` → done, and permission notifications → blocked into
  `~/.factory/hooks.json`. Factory documents the file, events, JSON
  `session_id`, and `droid --resume ID` in its [hooks](https://docs.factory.ai/harness/hooks) and [CLI reference](https://docs.factory.ai/droid-cli/cli-reference).
- `integrate kimi [--write]` — appends a clearly marked managed `[[hooks]]`
  block to `~/.kimi-code/config.toml`: start/prompt → working, `Stop` → done,
  and permission notifications → blocked. Reinstalling replaces only that
  block; `--remove` deletes it. Kimi documents the TOML hook configuration and
  JSON `session_id` payload in its [hooks reference](https://github.com/MoonshotAI/kimi-cli/blob/main/docs/en/customization/hooks.md), and resumes exact sessions with `kimi --session ID` in its [command reference](https://github.com/MoonshotAI/kimi-code/blob/main/docs/en/reference/kimi-command.md).
- `integrate qwen [--write]` — merges `SessionStart`/`UserPromptSubmit` →
  working, `Stop` → done, and `PermissionRequest` → blocked into
  `~/.qwen/settings.json`. Qwen documents the hook event/payload contract and
  `qwen --resume ID` in its [hooks](https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/hooks.md) and [headless reference](https://github.com/QwenLM/qwen-code/blob/main/docs/users/features/headless.md).
- `integrate omp [--write]` — writes a Pi-compatible extension below OMP's
  configured agent directory (normally `~/.omp/agent/extensions`). It reports
  `session_start` → done and `agent_start`/`agent_settled` → working/done with
  the session ID or session file. OMP's local CLI help documents
  `--resume=<id-or-path>`; the installed extension is fixture-tested here.
- `integrate kilo [--write]` — writes an auto-loaded global plugin at
  `~/.config/kilo/plugin/`. It reports Kilo's documented session and
  permission events as working/done/blocked. Kilo documents the global plugin
  location and lifecycle events in its [plugin reference](https://github.com/Kilo-Org/kilocode/blob/main/packages/kilo-docs/pages/automate/extending/plugins.md).
  Its public CLI documentation only guarantees workspace-level `--continue`,
  so Ködade intentionally does not persist a Kilo native ID for restore.
- `integrate hermes [--write]` — writes a Python plugin under
  `~/.hermes/plugins/kodade_cli_agent_state/`. It reports documented
  `session_id` lifecycle callbacks and restores an exact conversation with
  `hermes --resume ID`, as specified in Hermes' [hooks](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/hooks.md)
  and [sessions](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/sessions.md) references.


The remaining registry names are not installed as lifecycle integrations yet.
Devin's public docs describe hosted integrations but no local CLI hook/session
contract. Antigravity documents configurable hooks, but does not publish a
session-identity or ID-resume contract in its [CLI codelab](https://codelabs.developers.google.com/genai-for-dev-antigravity-cli?hl=en).
Grok documents hook discovery under `~/.grok/hooks`, but this release's public
hook page was not available as a stable payload contract; its documented
[status line](https://docs.x.ai/build/features/status-line) does expose a
`session_id`, but is a display callback and cannot author `blocked` state.
Mastra Code exposes `/hooks` and project configuration, but its public site
currently does not specify a hook JSON payload or thread-resume CLI argument.
No Ködade adapter is written for those tools until their vendors publish those
contracts.

Merges are idempotent and never remove unrelated keys or hooks. A previously
installed Ködade report hook for the same event is upgraded in place (matched
on the `kodade-cli agent report $KODADE_PANE ` command prefix), so users who
installed the earlier `Stop` → `idle` hook are migrated to `done` without a
duplicate.

Configuration updates are atomic and preserve existing file permissions and
symlinks. Refreshing manifests uses the canonical `Kodade/kodade-cli` repository,
validates filenames and the daemon's full manifest schema before writing, and
bounds each download. A malformed download cannot replace working manifests.

### `agent update-manifests`

Opt-in manifest refresh. Downloads `index.txt` and each listed manifest from
`main` on GitHub into `~/.config/kodade-cli/agent-detection/` using the system
`curl` (no HTTP crate, no telemetry), printing every file it writes. This is
the only command that ever reaches the network; nothing runs automatically.
