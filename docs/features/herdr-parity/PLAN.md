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

The next integration branch combines machines (#32), images (#35), narrow
views (#40), and Unix standalone updates (#41). Its full workspace gates pass:
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
upgrade and client reconnect (#42), real remote bootstrap failure fixtures
(#45), exact native conversation restore (#46), optional cold screen replay
(#47), and the final fresh competitive/UX/release review (#37). The graphics
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
