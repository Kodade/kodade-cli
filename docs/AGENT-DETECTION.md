# Agent detection

Ködade CLI selects one of five states for each pane. States are also rolled
up to parent tabs and workspaces by urgency, with `blocked` taking priority,
then `working`, `done`, `idle`, and `unknown`.

- `blocked`: The agent is waiting for user attention, such as a permission,
  approval, or question prompt. Screen manifests must use a confident match
  before producing this state.
- `working`: The agent is actively producing output, or a hook or manifest
  explicitly reports this state.
- `done`: A hook or manifest explicitly reports that the agent completed work.
  The current built-in manifests do not define a `done` screen rule.
- `idle`: A recognized agent has no matching screen rule and has produced no
  output in the working window. An ordinary shell also uses `idle`.
- `unknown`: The foreground process is not a recognized agent and is not one
  of the supported shell names (`sh`, `bash`, `zsh`, `fish`, or `nu`).

## Authority and fallback

Detection uses the highest available authority in this order:

1. **Lifecycle hook.** A hook report wins when it is no more than 30 seconds
   old. The daemon constant is `HOOK_TTL = Duration::from_secs(30)`; a report
   exactly 30 seconds old is still valid. The report supplies the state and a
   source string. An expired report is ignored.
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
`crates/kodade-cli-daemon/src/manifest.rs`. The shipped agents are:

- Claude Code (`claude-code.toml`)
- Codex (`codex.toml`)
- Grok Build (`grok.toml`)
- OpenCode (`opencode.toml`)
- Gemini CLI (`gemini-cli.toml`)
- Aider (`aider.toml`)

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

## Planned interfaces

The PRD describes `kodade-cli agent explain <pane>` to show why a state was
chosen and `kodade-cli integrate <agent>` to install lifecycle-hook
integration. Those commands are planned; the current CLI does not implement
them. The daemon's protocol can accept an `AgentState` report, but the hook
installer and agent-facing integration are not yet present.
