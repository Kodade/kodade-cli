# Ködade CLI versus HerdR

Reference: [HerdR `9e01168`](https://github.com/herdrdev/herdr/tree/9e01168b140ce8e3821131345dc82bc2bf9994eb), compared with Ködade's original `84af09e` and this delivery. This is a source and behavior comparison, not a claim about every future HerdR release or every version of an agent CLI. The [delivery ledger](PLAN.md) records merge and verification status.

## What HerdR had that was missing

The largest gaps were outside basic splitting and navigation. Ködade already had durable PTYs, workspaces, tabs, panes, copy/search, themes, Git worktrees, SSH attach and a scriptable socket. HerdR connected those primitives into a more complete everyday agent workspace.

| Area | Original gap | Result and evidence |
| --- | --- | --- |
| Concurrent clients | Shared selection and size could redirect another client's input | Independent view state; PTY resize before input; real clients with different selections and dimensions (#28, #40) |
| Command discovery | Several navigation pickers, little action discovery | Searchable command center, first-run guidance, attention history, visible reasons for agent state (#29) |
| Agent automation | Raw pane primitives and brittle waits | Agent start/read/prompt/wait, guarded identities, activity revisions, regex/history waits, actionable exit statuses (#31) |
| Multiple machines | Single SSH attach, tunnel ownership races | Saved machines, combined sidebar, independent reconnect, endpoint-qualified actions, remote images and extensions (#30, #32) |
| Extensions | No comparable local action system | Local inspectable plugins, event hooks, background commands, terminal actions, selected-text context and URL handlers (#33, #50) |
| User commands | No named actions and shortcuts | Configured command keys and palette entries, scoped context and workspace cwd (#51) |
| Integrations | Incomplete hook installers and identification | 17 lifecycle adapters, 24 manifests, live reload, explicit per-agent lifecycle and native-session contracts (#34, #49, #56) |
| Conversation continuity | Guessed `resume --last` could select another conversation | Exact validated IDs or paths, unique restore targets, retirement of stale native references (#46) |
| Cold screen restore | Only live daemon scrollback | Optional bounded private history, consistent layout/history identity and cleanup (#47) |
| Workspace setup | No isolated workspace variables or opening an existing registered worktree | Environment on all pane/tab spawn paths, safe linked-worktree open, custom paths and base refs (#52) |
| Automation onboarding | Commands spread across help and docs | Offline `agent guide`, with its documented workflow executed in real PTYs (#53) |
| Images | No terminal graphics or image paste | Bounded Kitty image subset, private PNG attachments, remote paste and host cleanup (#35) |
| Small terminals | Full layout became unusable at narrow widths | Focused projection with accessible pane switching; original split tree preserved (#40) |
| Installation maintenance | No verified built-in update/bootstrap flow | Version and checksum checks, bounded downloads/extraction, failed-install preservation, stable/preview selection (#41, #45) |
| Windows | Unix-only transport | Native ConPTY and authenticated local transport, Windows archives and SSH bridge (#36; see final runner evidence in PLAN.md) |
| Live upgrades | Restarting the daemon restarted its panes | Transactional Unix PTY transfer, rollback, client reconnect and preserved live context (#42; see final runner evidence in PLAN.md) |
| Extended graphics | File, temporary-file, shared-memory, compressed media and virtual/relative placement gaps | Bounded media decoding and ownership, Unicode placeholders and relative geometry; real PTY and host rendering proof (#61) |
| Keyboard negotiation | Enhanced input and terminal capability queries missing | Per-pane keyboard negotiation and supported capability replies (#64; final input/runner review pending in PLAN.md) |
| Windows remote bootstrap | Windows client could not prepare Unix hosts | Shared verified installer and exact compatibility preflight (#66; final native runner evidence pending in PLAN.md) |
| Terminal polish | Labeled OSC 8 links, synchronized output and native copy were absent | Tracked and verified as #58, alongside parser crash regressions in #59 |

HerdR source anchors: [`src/client`](https://github.com/herdrdev/herdr/tree/9e01168b140ce8e3821131345dc82bc2bf9994eb/src/client), [`src/cli`](https://github.com/herdrdev/herdr/tree/9e01168b140ce8e3821131345dc82bc2bf9994eb/src/cli), [`src/integration`](https://github.com/herdrdev/herdr/tree/9e01168b140ce8e3821131345dc82bc2bf9994eb/src/integration), and [`src/platform`](https://github.com/herdrdev/herdr/tree/9e01168b140ce8e3821131345dc82bc2bf9994eb/src/platform).

## What was worth removing

The useful capabilities did not need a purge. The unnecessary parts were unsafe assumptions and duplicated ownership:

- Removed ordinary shells from the agent-only panel and repeated local-machine headings. Saved offline machines remain visible; the command center displays a bounded, scrollable result list.
- Removed inherited host-terminal names as the default pane capability contract. Panes default to widely available `xterm-256color`.
- Removed guessed conversation selection through `--last`. An exact native reference is required before an agent is resumed.
- Removed shared client focus as an input-routing authority. Each attached client keeps its own acknowledged view.
- Removed stale endpoint generations and unqualified pane IDs as routing authorities. A pane belongs to a specific connection.
- Removed forced dirty-worktree removal from routine cleanup. Failed removal preserves the workspace and its checkout.
- Removed recoverable request errors as a reason to disable the entire connection.
- Removed per-input terminal snapshots from resize bookkeeping. Resize uses the layout metadata it actually needs.
- Replaced scattered historical implementation status in the README with practical usage and a separate engineering ledger.

Keep the keyboard escape hatches, JSON/socket interface, built-in themes, copy mode and low-level pane commands. They serve different workflows and do not need to be removed merely because the command center offers another entry point. A hosted marketplace, account service or credential proxy would add operating cost without improving these local capabilities.

## Code and design decisions

Keep the three public crates: protocol, persistent PTY daemon and client/CLI. Put behavior behind modules that own its rules: endpoint routing, guarded automation, integration installers, extension execution, graphics, history, updating and terminal replay. Shared reconnect view handling restores acknowledged focus and scroll state without reusing an input queue.

Use transactional ownership where data or processes outlive a request. Private extension context and pasted images belong to their runtime owner; failed import cannot acquire deletion rights. Worktree deletion, binary replacement, remote bootstrap and live handoff all verify the proposed replacement before releasing the existing usable state.

Defaults should make the first session useful. Advanced commands, hooks and machine configuration remain discoverable through the palette and guide. Cold screen persistence remains opt-in; terminal output is not silently written to disk. Every status should explain the state or the next useful action.

## How to judge the result

The strongest reasons to choose Ködade are the combined workflows: open local and SSH projects together; send a guarded prompt to the intended agent; run an extension with the exact selected text and pane context; resume the correct conversation; and update the workspace without losing running shells. These claims have focused real-process tests in addition to Rust unit tests.

The documented limits remain part of the product contract. [Graphics](../../GRAPHICS.md) describes the supported Kitty subset. [Agent detection](../../AGENT-DETECTION.md) separates verified hook/native contracts from heuristic screen rules. Native live PTY handoff is a Unix capability. The ledger must remain explicit about actual operating-system runner results and release artifacts; a compiling platform branch or a printed fixture label is insufficient evidence.
