# Ködade CLI competitive improvement

This effort is authorized to build, test, review, and ship the improvements,
without a separate plan-approval gate. It extends the earlier CLI scope to
cover HerdR's terminal capabilities, including extensions and multiple machines.
The local, single-binary design remains: no hosted service, telemetry, or
credential proxy. Desktop business workflows remain separate.

## Evidence and completion

- Ködade baseline: `84af09e138542fcf24e815fc24fcf665087a984d` (v0.2.1).
- HerdR reference: [`9e01168b140ce8e3821131345dc82bc2bf9994eb`](https://github.com/herdrdev/herdr/tree/9e01168b140ce8e3821131345dc82bc2bf9994eb).
- Compare implemented behavior, not feature names or staged documentation.
- A task is complete only after its behavior has been exercised, the full
  relevant diff reviewed, all checks pass, and it is merged and pushed.
- Overall parity is not complete while capabilities below remain missing.
  Untested platforms and hypothetical prompt fixtures do not count as proof.

## Capability ledger

| Capability | Ködade at baseline | Work |
| --- | --- | --- |
| Durable PTYs, detach and layout restore | Present; fresh processes after daemon restart | [#28](https://github.com/Kodade/kodade-cli/issues/28) reliability |
| Workspace/tab/pane layout, splits, swaps, zoom | Present | Preserve and exercise |
| Copy mode, search, selection, OSC 52, safe paste | Present, including keyboard scrollback | Preserve and exercise |
| Agent states, reasons, notifications and hooks | Present; uneven agent coverage | [#34](https://github.com/Kodade/kodade-cli/issues/34) |
| Agent launch/prompt/read by name; activity-aware waits | Partial; raw pane primitives only | [#31](https://github.com/Kodade/kodade-cli/issues/31) |
| CLI/JSON/socket automation, events, completions | Present; output waits lack regex/history | [#31](https://github.com/Kodade/kodade-cli/issues/31) |
| Git worktrees | Present; unsafe forced removal found | [#28](https://github.com/Kodade/kodade-cli/issues/28) |
| Concurrent clients with independent views | Missing; focus and size shared globally | [#28](https://github.com/Kodade/kodade-cli/issues/28) |
| SSH attach | Present; forwarding socket ownership race | [#30](https://github.com/Kodade/kodade-cli/issues/30) |
| Saved machines, combined sidebar, independent reconnect | Missing | [#32](https://github.com/Kodade/kodade-cli/issues/32) |
| Plugins, custom command actions, event hooks, terminal panes | Missing | [#33](https://github.com/Kodade/kodade-cli/issues/33) |
| Action launcher, onboarding, attention history | Partial; navigation pickers and unread jump only | [#29](https://github.com/Kodade/kodade-cli/issues/29) |
| Themes, settings, keybindings, sidebar modes | Present; preserve clear defaults | [#29](https://github.com/Kodade/kodade-cli/issues/29), [#37](https://github.com/Kodade/kodade-cli/issues/37) |
| Startup diagnostics, status, manifest management | Partial | [#30](https://github.com/Kodade/kodade-cli/issues/30), [#34](https://github.com/Kodade/kodade-cli/issues/34) |
| Terminal images/graphics and image paste | Missing | [#35](https://github.com/Kodade/kodade-cli/issues/35) |
| Native Windows transport, PTYs and distribution | Missing; WSL only | [#36](https://github.com/Kodade/kodade-cli/issues/36) |
| Live server handoff | Missing | [#42](https://github.com/Kodade/kodade-cli/issues/42) |
| Narrow terminal projection and accessible switching | Missing | [#40](https://github.com/Kodade/kodade-cli/issues/40) |
| Verified stable/preview updates | Missing | [#41](https://github.com/Kodade/kodade-cli/issues/41) |
| Verified remote bootstrap | Missing | [#45](https://github.com/Kodade/kodade-cli/issues/45) |
| Exact native conversation restore | Manifest `resume --last` can select the wrong session | [#46](https://github.com/Kodade/kodade-cli/issues/46) |
| Optional screen replay after cold restart | Live scrollback only | [#47](https://github.com/Kodade/kodade-cli/issues/47) |
| Complete lifecycle adapters and exact native IDs | Uneven installed hooks | [#49](https://github.com/Kodade/kodade-cli/issues/49), [#56](https://github.com/Kodade/kodade-cli/issues/56) |
| Selection/context-aware extensions and URL handlers | Missing | [#50](https://github.com/Kodade/kodade-cli/issues/50) |
| Named configured commands and shortcuts | Missing | [#51](https://github.com/Kodade/kodade-cli/issues/51) |
| Workspace environments and existing worktree open | Missing | [#52](https://github.com/Kodade/kodade-cli/issues/52) |
| Discoverable local automation guide | Missing | [#53](https://github.com/Kodade/kodade-cli/issues/53) |

HerdR implementation references: `src/cli/{machine,agent,plugin,status}.rs`,
`src/client/endpoint/`, `src/client/shell/`, `src/app/api/plugins/`,
`src/integration/`, `src/kitty_graphics.rs`, and `src/platform/windows.rs`.
Ködade equivalents are in the three `crates/` packages; existing command and
behavior contracts are in README and `docs/SOCKET-API.md`.

## Implementation order

1. Reliability (#28), command center (#29), automation (#31), and operations
   (#30) run in isolated branches. Integrate in that order, with root operations
   last. Review uncommitted work first, then review each full branch diff.
2. Machines (#32), extensions (#33), and detection/integrations (#34).
3. Graphics (#35), native Windows (#36), remaining capability ledger and
   product verification (#37).

Every wave runs `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo build`, and `cargo test`,
plus behavioral CLI/TUI/transport checks suited to that wave. CI must pass on
the integrated branch before merge. Delete only worktrees and branches created
for this effort. Do not write automatic project-memory checkpoints.

## Simplification decisions

Keep the three-crate split and existing user escape hatches. Extract coherent
startup, integration, automation, and overlay modules as those areas change.
Remove repeated routing rules and unsafe shared state where evidence warrants
it. Do not delete useful features just to shorten the checklist, grow a hosted
marketplace, or replace the terminal engine without compatibility evidence.

## Verified audit corrections

An initial reviewer suspected Linux cannot hard-link a bound Unix socket. A
real bind/link/unlink/connect experiment succeeded; that is not a proven bug.
Layout application already documents that saved commands execute. Preserve this
intentional automation contract instead of adding an unrelated approval gate.

## Delivery evidence — 2026-09-08

Merged to main through PRs #39 and #44: reliability and independent views
(#28), command center/attention/onboarding (#29), startup and SSH operations
(#30), guarded agent automation (#31), local extensions (#33), agent hooks and
live manifest reload (#34), and terminal cleanup (#38). Linux and macOS CI
passed. This does not establish native Windows or universal agent compatibility.

PR #48 merged machines (#32), images (#35), narrow views (#40), and Unix
standalone updates (#41) after Linux and macOS CI passed. Its workspace gates passed:
214 client tests, 93 daemon tests, 7 protocol tests, and the agent automation
integration test. Additional behavioral proof:

- Isolated SSH server: multiple concurrent tunnels, combined machine switching,
  colliding pane IDs, offline endpoint isolation, remote PNG upload, graphics,
  rename, and tunnel cleanup.
- Real controlling terminal: a rejected daemon action leaves the connection
  usable; detach and transport failure restore terminal modes and cursor.
- Graphics: real PTY image frames, PNG validation and private attachments,
  modal hide/restore without re-upload, viewport resize, detach asset cleanup,
  session attachment cleanup. A real Kitty host also displayed the image.
- Narrow view: a real 40-column TUI measures 37-column shell PTYs, switches
  original shell processes through mouse controls and keyboard, runs alongside
  an independent wide Hello client, then restores the original split at 120
  columns. Width measurements, process identities, and mouse switcher opening
  are asserted.
- Updates: checksums, safe bounded archive extraction, rejected
  installation preservation, canonical symlink targets, and same-directory
  replacement are covered. An isolated CLI run also downloaded the published
  v0.2.1 archive, verified it, installed it to a temporary destination, and ran
  its version command successfully. Windows ZIP updates remain part of the Windows
  integration work.

Still in progress: native Windows runner evidence (#36), complete live daemon
upgrade and client reconnect (#42), terminal hyperlink/clipboard/rendering polish (#58), parser crash fixes (#59),
and the final fresh competitive/UX/release review (#37). The graphics
implementation has a documented supported protocol subset in docs/GRAPHICS.md;
this ledger does not claim complete Kitty protocol coverage.

## Concrete code and product simplifications

- Replace duplicated routing and unqualified pane IDs with endpoint-qualified
  dispatch; ignore packets from retired endpoint generations.
- Keep recoverable action errors separate from connection failure, so a rejected
  action cannot silently disable subsequent typing.
- Keep per-client focus and compact projection outside the persisted split tree.
  Resize reads layout metadata directly instead of constructing full terminal
  and agent snapshots on each input event.
- Use one compact-header rectangle map for rendering and mouse actions.
- Preserve local, inspectable extensions and bounded subprocess execution;
  no hosted marketplace or account system is needed for their capabilities.
- Remove the stale README implementation-milestone table. Put current behavior
  and shortcuts in front of users, with detailed engineering status here.

Remote bootstrap (#45) merged through PR #54 after Linux/macOS CI, with 220 client, 93 daemon,
7 protocol tests and the agent automation integration test passing. A real
isolated SSH fixture installs a checksum-verified binary into a home path with
spaces and proves checksum refusal, incompatible-release refusal and truncated
upload all preserve the prior executable. The combined SSH/image/machine smoke
also passes with the integrated release binary.

A fresh code comparison additionally tracks verified lifecycle adapter coverage
(#49), structured extension context and URL handlers (#50), configured commands
(#51), workspace environment and existing-worktree open (#52), and a discoverable
local agent guide (#53). Their reviewed implementation and behavioral evidence are recorded below; the overall ledger still requires the remaining platform and terminal work.

Exact conversation restore (#46) and optional screen history (#47) merged
through PR #55 after Linux/macOS CI. Full workspace gates and both real daemon
restart smokes pass.
Native restore runs the generated hook reporter, preserves two distinct IDs in
one directory across two restarts, and clears disabled/duplicate/retired references.
Retiring an agent in a hidden pane marks the persisted layout dirty.

Screen replay is off by default. The smoke restores a frame taller and wider than
the daemon's initial 80x24 size and proves rename/disable/kill cleanup and default
output privacy. Review also fixed long Unicode history discarding the active
frame, metadata/history identity drift, per-pane/total encoding budgets, and
terminal-control validation. History and layout now share one atomic private-file
writer; a failed publish is retried.


PR #57 integrates the agent workflows (#49–53, #56): 17
installation adapters with documented per-agent lifecycle and resume contracts;
structured extension context scoped to the selected endpoint and pane;
selection-aware actions, URL handlers, configured command keys/palette entries;
workspace environments, existing linked worktree open, and a local `agent guide`.
Real controlling-terminal fixtures execute selected-text actions and shortcuts,
show a slow pane action allows another pane command to complete, and verify
context file cleanup when the owning pane closes. Real Git/PTY fixtures exercise
isolated workspace variables, nondefault worktree paths and dirty-checkout
preservation. Native restore, history, and the executable guide example also
pass against the combined binary.

A fresh manifest comparison found further bundled identification gaps (#56),
including hook-backed agents without terminal-title evidence. That follow-up
also completes Grok's lifecycle mapping from the vendor's actual event table.
The resulting 24 bundled manifests now include every pinned HerdR manifest and
additional supported adapters. Real PTY automation identifies a Node-hosted
agent without OSC title evidence and rejects guarded input after it is replaced
by a different program. The wrapper identity is bound to the reported foreground
PID and approved executable; absent evidence fails closed. Grok lifecycle hooks
follow the vendor event table. This proves the tested adapter contracts and
retirement behavior, not every future release of every agent CLI.

Previous integrated workflow gates passed: 241 client tests, 112 daemon tests, eight
protocol tests, three real agent automation integration tests and one guide
integration test. The full branch review also corrected a stdout-dependent mouse
fixture and macOS canonical worktree-path assertion.

The final source comparison additionally tracks OSC 8 pane links, synchronized
output and native/remote clipboard behavior (#58). A real one-row wrap test
exposed a crash in the existing parser dependency; #59 upgrades the maintained
parser fork and adds wrapping/resize regressions before the final release.

## v0.3.0 Unix release candidate

The candidate is a Linux/macOS release only. It will publish four standalone
archives (Linux and macOS on arm64 and x86_64); native Windows binaries,
installation, upgrades, and SSH preparation are deferred and are not part of
this release's evidence.

Previously reviewed evidence is recorded above for machines, agent workflows,
updates, graphics, and terminal behavior. The Unix candidate also combines the
terminal links/clipboard, extended graphics, negotiated keyboard input, and
live-handoff changes. Those implementation reviews do not establish a v0.3.0
release by themselves.

Still pending before publication: the full integrated Unix diff review, the
required Unix test and CI gates, final Linux/macOS runner evidence, the v0.3.0
tag, the four release archives and `SHA256SUMS`, and Homebrew publication. The
release notes must continue to state the v0.2.1 migration limit: an existing
v0.2.1 daemon cannot hand off its running panes. Users can leave that session
running and begin a separately named v0.3.0 session; `session upgrade` applies
only after a session is already served by a v0.3.0 Unix daemon.
