# Worklog

### 2026-09-10 07:04 EDT - Repair local blank-screen installation

- Outcome: Reproduced the installed Homebrew v0.3.0 rendering failure with `python3 scripts/terminal-links-tui-smoke.py /opt/homebrew/bin/kodade-cli`: timed out waiting for a frame to be presented before the next draw. The v0.3.0 tag lacks the synchronized-output flush already fixed in commit `7a89546`.
- Repair: Built clean main at `50ef980` with `cargo build --release --locked` and atomically replaced the local Homebrew Cellar executable. Preserved the original at `~/.local/state/kodade-cli/backups/0.3.0-before-frame-fix-20260910/kodade-cli`. The installed command matches the release build by SHA-256. No source changes were needed.
- Checks: Both `terminal::tests` passed; formatting and whitespace checks passed. The terminal-links smoke passed against both the fresh release build and the installed command. The terminal-keyboard smoke passed using Python 3.13, covering real input, two live upgrades, focus, modal handling, and detach restoration.
- Limitations: Python 3.14's keyboard fixture timed out waiting for its first recorder; Python 3.13 passed. The separate `tui-smoke-test.py` hung draining a closed PTY at line 44 under both Python versions and was interrupted. Direct Ghostty inspection was denied by the computer-use tool's app restriction. Full workspace tests and clippy were not run for this installation-only repair.
- Remaining: This is a local patch retaining version 0.3.0, not a new public release. A Homebrew reinstall of that release would restore the faulty binary. Publish the existing rendering and keyboard fixes in a subsequent release. Investigate the cleanup fixture's EOF handling separately.

### 2026-09-10 07:54 EDT - Open launch directories by default

- Outcome: Plain local TUI launches now open or reuse a workspace by canonical launch directory. New workspace shells start there; custom names and existing panes keep their state. Explicit session/socket/remote targets and inherited pane context retain resume behavior.
- Implementation: Added capability-checked `OpenDirectory` to the socket protocol. Lookup and creation share the daemon dispatch lock, preventing duplicate workspaces from concurrent launches. Interactive selection remains per-client. Updated CLI help, usage/protocol documentation, changelog, and CI.
- Checks: The new real-PTY regression failed against the previous binary and passed against debug, release, and installed builds. It covers cold startup, existing-session selection, shell cwd, symlink reuse, independent clients, and explicit/inherited resume. The concurrent daemon test also covers custom names, basename collisions, and invalid paths. All 484 tests passed with `RUST_TEST_THREADS=2 SHELL=/bin/sh cargo test --locked`; unrestricted tests with the local login shell initially hit nine existing PTY setup/timeouts. Formatting, Clippy with warnings denied, and whitespace checks passed. Live TUI handoff passed after making its intentional resume use `-s default`.
- Installation: Backed up the previous local executable and session metadata under `~/.local/state/kodade-cli/backups/before-launch-directory-20260910/`, installed the release build, and live-upgraded the default daemon. Verified the installed hash, new daemon capability, identical exported session layout, and original shell PID/start time after upgrade.
- Remaining: Existing `pioneer-trail` is still the custom name of the workspace rooted at `projects`; reuse deliberately retains that name and its live shell directory. This local installation still reports v0.3.0; public release packaging remains separate.

### 2026-09-10 09:51 EDT - Repair Homebrew release rendering in Ghostty

- Outcome: User reported a black screen and unusable input in Ghostty after installing the published Homebrew v0.3.0. The installed binary failed `terminal-links-tui-smoke.py` waiting for a frame to be presented before the next draw. The tag lacks the synchronized-output flush and subsequent keyboard fixes already on main.
- Repair: Built clean main at `09958c8` with `cargo build --release --locked`, backed up the original executable under `~/.local/state/kodade-cli/backups/0.3.0-release-20260910-0951/`, and atomically replaced the Homebrew Cellar binary. Installed and built SHA-256 values match. No application source, user settings, or saved sessions were changed.
- Checks: All 269 CLI binary unit tests passed in release mode. Rendering smoke passed against both build and installation. Keyboard smoke with Python 3.12 first timed out at the frame after live upgrade 1; a repeat passed, and the installed-binary run also passed, including both live upgrades and detach restoration. Diagnostic sessions and the manually created temporary directory were cleaned up.
- Remaining: Direct Ghostty visual confirmation is still needed from the user. This local build retains version 0.3.0; reinstalling the published release would restore the bug. Publish the existing fixes in a subsequent release. Full workspace tests and clippy were not run for this installation-only repair.

### 2026-09-10 09:56 EDT - Prepare v0.3.1 and gate publication

- Outcome: User confirmed the repaired build works in Ghostty and authorized a permanent release. Bumped all three workspace packages and lock entries to 0.3.1 without dependency changes; added release notes for rendering, keyboard, and launch-directory fixes.
- Decisions: Reuse the existing Linux/macOS CI workflow from the tagged commit and require it alongside all platform builds before publication. The Homebrew automation secret is present; tap update and installation verification remain part of publication.
- Checks: All 484 workspace tests passed with `RUST_TEST_THREADS=2 SHELL=/bin/sh cargo test --release --locked`. Formatting, Clippy with warnings denied, release-gate YAML structure, and whitespace checks passed. Release-binary PTY checks passed for rendering/links, enhanced keyboard (Python 3.12), launch directories, graphics media, live client handoff, and terminal colors.
- Remaining: Push preparation, verify hosted Linux/macOS CI, publish v0.3.1, verify tap checksums, and upgrade the local Homebrew installation. No release has been published at this milestone.

### 2026-09-10 10:05 EDT - Correct pre-release detach-test race

- Outcome: Linux CI passed; macOS first timed out at the frame-presentation check, which passed unchanged on rerun. That rerun exposed a separate startup smoke false failure: the client exited successfully between the detach predicate and the harness liveness assertion.
- Repair: Recheck the completion predicate after observing exit before failing the wait. Unexpected exits still fail; no application behavior or timeouts changed.
- Checks: An isolated execution of the actual wait method with deterministic process-poll results `[None, 0, 0]` failed before the fix and passed after it. A false completion predicate with exit code 1 still raises. The complete startup-directory debug-binary smoke passed locally. The rendering smoke also passed locally against the debug build.
- Remaining: Rerun hosted CI before tagging; investigate frame-check timing further if it recurs. Publication remains pending.

### 2026-09-10 10:16 EDT - Publish and install v0.3.1

- Outcome: Tagged `9cf7119` as v0.3.1 after Linux and macOS CI run `34486780156` passed. Release run `34487114430` published all four platform archives plus SHA256SUMS and automatically updated `Kodade/homebrew-tap` at `b61eb62`.
- Checks: The release-time macOS job first timed out starting the keyboard fixture's first recorder; an unchanged failed-job retry passed the complete gate. All four downloaded release archives passed SHA-256 verification. The tap formula exactly matches the generator output for the published checksums. Both the GitHub-built Apple Silicon artifact and the installed Homebrew binary passed rendering and keyboard PTY checks; their executable hashes match. `brew test kodade/tap/kodade-cli` passed, with the existing process-enumeration sandbox warning.
- Installation: Upgraded only `kodade/tap/kodade-cli` through Homebrew from 0.3.0 to 0.3.1. Disabled automatic update and cleanup for the scoped upgrade. The local command now reports 0.3.1; no user sessions were killed or settings modified. The prior local patch is no longer required.
- Remaining: The published release includes the permanent fixes and CI publication gate. Intermittent macOS fixture-start/frame-check timeouts remain a test-reliability follow-up; checks were not disabled or weakened to publish. The deterministic detach-test race was corrected before tagging.
