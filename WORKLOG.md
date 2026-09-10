# Worklog

### 2026-09-10 07:06 EDT - Repair local blank-screen installation

- Outcome: Reproduced the installed Homebrew v0.3.0 rendering failure with `python3 scripts/terminal-links-tui-smoke.py /opt/homebrew/bin/kodade-cli`: timed out waiting for a frame to be presented before the next draw. The v0.3.0 tag lacks the synchronized-output flush already fixed in commit `7a89546`.
- Repair: Built clean main at `50ef980` with `cargo build --release --locked` and atomically replaced the local Homebrew Cellar executable. Preserved the original at `~/.local/state/kodade-cli/backups/0.3.0-before-frame-fix-20260910/kodade-cli`. The installed command matches the release build by SHA-256. No source changes were needed.
- Checks: Both `terminal::tests` passed; formatting and whitespace checks passed. The terminal-links smoke passed against both the fresh release build and the installed command. The terminal-keyboard smoke passed using Python 3.13, covering real input, two live upgrades, focus, modal handling, and detach restoration.
- Limitations: Python 3.14's keyboard fixture timed out waiting for its first recorder; Python 3.13 passed. The separate `tui-smoke-test.py` hung draining a closed PTY at line 44 under both Python versions and was interrupted. Direct Ghostty inspection was denied by the computer-use tool's app restriction. Full workspace tests and clippy were not run for this installation-only repair.
- Remaining: This is a local patch retaining version 0.3.0, not a new public release. A Homebrew reinstall of that release would restore the faulty binary. Publish the existing rendering and keyboard fixes in a subsequent release. Investigate the cleanup fixture's EOF handling separately.
