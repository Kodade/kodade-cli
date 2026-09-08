//! Command-line surface for the `kodade-cli` binary.
//!
//! clap owns parsing, help, and `--version`; `main.rs` only dispatches the
//! parsed tree. Every read command accepts `--json` so scripts can consume the
//! proto snapshots directly.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use kodade_cli_proto::{AgentStateKind, Direction, PaneId};

fn env_assignment(value: &str) -> Result<(String, String), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| "environment must be KEY=VALUE".to_owned())?;
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return Err("environment key must be a shell-style identifier".into()),
    }
    if !chars.all(|character| character == '_' || character.is_ascii_alphanumeric()) {
        return Err("environment key must be a shell-style identifier".into());
    }
    Ok((key.into(), value.into()))
}

pub const DEFAULT_SESSION: &str = "default";

#[derive(Debug, Parser, PartialEq, Eq)]
#[command(
    name = "kodade-cli",
    version,
    about = "Ködade CLI — a terminal workspace for agent CLIs",
    long_about = None,
)]
pub struct Cli {
    /// Session to attach to or query.
    #[arg(
        short = 's',
        long = "session",
        global = true,
        value_name = "NAME",
        default_value = DEFAULT_SESSION,
        value_parser = session_name
    )]
    pub session: String,

    /// Attach to or query a daemon on `USER@HOST` over an SSH-forwarded socket
    /// instead of the local one (#23). Requires `kodade-cli` on the remote host.
    #[arg(long = "remote", global = true, value_name = "USER@HOST")]
    pub remote: Option<String>,

    /// Use this daemon socket directly (overrides inherited pane context).
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "remote")]
    pub socket: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Check the selected release channel or install a verified standalone update.
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long, value_parser = ["stable", "preview"])]
        channel: Option<String>,
        /// Print the persisted update channel and exit.
        #[arg(long, conflicts_with_all = ["check", "install_to"])]
        show_channel: bool,
        #[arg(long, conflicts_with = "check")]
        install_to: Option<PathBuf>,
    },
    /// Manage local, versioned command extensions.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Run the session daemon (started automatically when attaching).
    Daemon {
        /// Session name; defaults to the global --session value.
        #[arg(value_parser = session_name)]
        session: Option<String>,
    },
    /// Diagnose configuration, tools, and daemon health without starting a session.
    #[command(visible_alias = "status")]
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// List workspaces, tabs, panes, and their states.
    Ls {
        /// Print the layout snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect and control agent panes.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Send text to a pane, followed by a newline.
    Send {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        #[arg(value_name = "TEXT", allow_hyphen_values = true)]
        text: String,
        /// Send the text without a trailing newline.
        #[arg(long)]
        no_newline: bool,
    },
    /// Create a workspace with an optional root directory; prints its id.
    New {
        /// Workspace name. Selects the workspace if one already has this name.
        #[arg(short = 'w', long = "workspace", value_name = "NAME")]
        workspace: String,
        /// Root directory new panes in the workspace start in.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Run a command in a new pane through the login shell; prints the pane id.
    Run {
        /// Workspace name or id to run in (defaults to the active workspace).
        #[arg(short = 'w', long = "workspace", value_name = "NAME")]
        workspace: Option<String>,
        /// Tab name or id to run in (defaults to a new tab).
        #[arg(short = 't', long = "tab", value_name = "TAB")]
        tab: Option<String>,
        /// Pane title (defaults to the command's basename).
        #[arg(long = "name", value_name = "NAME")]
        name: Option<String>,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Split the focused (or given) pane into a new pane; prints the pane id.
    Split {
        /// Split downward instead of to the right.
        #[arg(long)]
        down: bool,
        /// Pane to split (defaults to the focused pane).
        #[arg(short = 'p', long = "pane", value_name = "PANE", value_parser = pane_id)]
        pane: Option<PaneId>,
        /// Optional command to run in the new pane, after `--`.
        #[arg(last = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Open a new tab in a workspace; prints the new pane id.
    NewTab {
        /// Workspace name or id (defaults to the active workspace).
        #[arg(short = 'w', long = "workspace", value_name = "NAME")]
        workspace: Option<String>,
        /// Pane title for the new tab.
        #[arg(long = "name", value_name = "NAME")]
        name: Option<String>,
    },
    /// Manage git-worktree workspaces (#22).
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Stop the session and its daemon.
    KillSession,
    /// Inspect the configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Print the key bindings, generated from the current config.
    Keys {
        /// Print the bindings as JSON instead of aligned text.
        #[arg(long)]
        json: bool,
    },
    /// Print or install agent CLI integrations.
    Integrate {
        #[command(subcommand)]
        target: IntegrateCommand,
    },
    /// Inspect and control panes.
    Pane {
        #[command(subcommand)]
        command: PaneCommand,
    },
    /// Inspect and control tabs (TAB is a name or an id).
    Tab {
        #[command(subcommand)]
        command: TabCommand,
    },
    /// Inspect and control workspaces (WS is a name or an id).
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Inspect and control sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Manage saved SSH machine profiles stored on this client.
    Machine {
        #[command(subcommand)]
        command: MachineCommand,
    },
    /// Export or apply a session layout (the persistence JSON).
    Layout {
        #[command(subcommand)]
        command: LayoutCommand,
    },
    /// Stream session events until interrupted.
    Events {
        /// Print each event as a JSON object instead of a text line.
        #[arg(long)]
        json: bool,
    },
    /// Print a shell completion script.
    Completion {
        #[arg(value_name = "SHELL")]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum PaneCommand {
    /// Paste a PNG path without submitting. With no PATH, use the host clipboard.
    PasteImage {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Print a pane's text. Defaults to the visible screen; `--scrollback`
    /// includes the full history and `--lines N` keeps only the last N lines.
    Read {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        /// Keep only the last N lines.
        #[arg(long, value_name = "N")]
        lines: Option<usize>,
        /// Include the full scrollback, not just the visible screen.
        #[arg(long)]
        scrollback: bool,
    },
    /// List the panes of the active tab.
    Ls {
        /// Print the pane snapshots as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Send key names (`Enter`, `C-c`, `M-x`, `F5`) or literal text to a pane.
    ///
    /// A capitalized word that is not a known key name is an error; pass
    /// `--literal` to send such text verbatim.
    SendKeys {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        /// Send every argument as text, joined by spaces (no key names).
        #[arg(short = 'l', long)]
        literal: bool,
        #[arg(value_name = "KEYS", required = true, allow_hyphen_values = true)]
        keys: Vec<String>,
    },
    /// Close a pane.
    Kill {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
    },
    /// Focus a pane, activating its tab and workspace.
    Focus {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
    },
    /// Toggle zoom on a pane's tab, with that pane focused.
    Zoom {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
    },
    /// Swap a pane with its neighbour in a direction.
    Swap {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        #[arg(value_name = "DIRECTION")]
        direction: DirectionArg,
    },
    /// Move a pane into another tab.
    Move {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        /// Destination tab name or id.
        #[arg(short = 't', long = "tab", value_name = "TAB")]
        tab: String,
    },
    /// Resize a pane by N cells in a direction.
    Resize {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        #[arg(value_name = "DIRECTION")]
        direction: DirectionArg,
        #[arg(value_name = "N", allow_hyphen_values = true)]
        cells: i16,
    },
    /// Wait until a pane's visible screen contains TEXT.
    ///
    /// The match is a plain substring unless `--regex` is set.
    WaitOutput {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        #[arg(long = "match", value_name = "TEXT", allow_hyphen_values = true)]
        text: String,
        /// Match TEXT as a regular expression instead of a literal substring.
        #[arg(long)]
        regex: bool,
        /// Search the full scrollback instead of only the visible screen.
        #[arg(long)]
        scrollback: bool,
        /// Give up after S seconds and exit 2.
        #[arg(long, value_name = "S")]
        timeout: Option<u64>,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum TabCommand {
    /// List the tabs of the active workspace.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Open a new tab; prints its pane id.
    New {
        /// Workspace name or id (defaults to the active workspace).
        #[arg(short = 'w', long = "workspace", value_name = "NAME")]
        workspace: Option<String>,
        /// Pane title for the new tab.
        #[arg(long = "name", value_name = "NAME")]
        name: Option<String>,
    },
    /// Close a tab and its panes.
    Close {
        #[arg(value_name = "TAB")]
        tab: String,
    },
    /// Rename a tab.
    Rename {
        #[arg(value_name = "TAB")]
        tab: String,
        #[arg(value_name = "NAME", allow_hyphen_values = true)]
        name: String,
    },
    /// Activate a tab.
    Select {
        #[arg(value_name = "TAB")]
        tab: String,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum WorkspaceCommand {
    /// List workspaces.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Create a workspace with an optional root directory; prints its id.
    New {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
        /// Environment inherited by panes created in this workspace (KEY=VALUE).
        #[arg(long = "env", value_name = "KEY=VALUE", value_parser = env_assignment)]
        env: Vec<(String, String)>,
    },
    /// Close a workspace and its tabs.
    Close {
        #[arg(value_name = "WS")]
        workspace: String,
    },
    /// Rename a workspace.
    Rename {
        #[arg(value_name = "WS")]
        workspace: String,
        #[arg(value_name = "NAME", allow_hyphen_values = true)]
        name: String,
    },
    /// Activate a workspace.
    Select {
        #[arg(value_name = "WS")]
        workspace: String,
    },
    /// Set a workspace's sidebar color, or `off` to clear it (#19).
    Color {
        #[arg(value_name = "WS")]
        workspace: String,
        /// A `#rrggbb` hex color, or `off`.
        #[arg(value_name = "HEX|off")]
        color: String,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum SessionCommand {
    /// Print the daemon socket path for the session. `--remote` prints the
    /// remote host's path (used to set up the forwarded socket).
    Path,
    /// List every session socket in the runtime directory and probe it.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Stop a session (defaults to the current one).
    Kill {
        #[arg(value_name = "NAME", value_parser = session_name)]
        name: Option<String>,
    },
    /// Rename the current session; its socket and state file move with it.
    Rename {
        #[arg(value_name = "NAME", value_parser = session_name)]
        name: String,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum MachineCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Add {
        #[arg(value_name = "SSH_TARGET")]
        target: String,
        #[arg(long)]
        label: String,
        #[arg(long = "remote-session")]
        session: Option<String>,
        /// Download and install a verified standalone binary when the host needs one.
        #[arg(long)]
        install: bool,
    },
    /// Check a saved host and optionally install a verified standalone binary.
    Prepare {
        #[arg(value_name = "PROFILE")]
        id: String,
        #[arg(long)]
        install: bool,
    },
    Rename {
        id: String,
        #[arg(long)]
        label: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Remove {
        id: String,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum LayoutCommand {
    /// Write the session layout as JSON to FILE (default: stdout).
    Export {
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Rebuild the session layout from a file written by `layout export`.
    Apply {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

/// Direction words accepted by `pane swap` / `pane resize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DirectionArg {
    Up,
    Down,
    Left,
    Right,
}

impl From<DirectionArg> for Direction {
    fn from(value: DirectionArg) -> Self {
        match value {
            DirectionArg::Up => Direction::Up,
            DirectionArg::Down => Direction::Down,
            DirectionArg::Left => Direction::Left,
            DirectionArg::Right => Direction::Right,
        }
    }
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum AgentCommand {
    /// Print the versioned local automation guide without connecting to a session.
    Guide,
    /// List recognized agents and their states.
    Ls {
        /// Print the matching pane snapshots as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start a command in a new pane. Never types into an existing shell.
    Start {
        /// Workspace name or id (defaults to the active workspace).
        #[arg(short = 'w', long = "workspace", value_name = "NAME")]
        workspace: Option<String>,
        /// Open in this tab (or a new tab when omitted).
        #[arg(short = 't', long = "tab", value_name = "TAB")]
        tab: Option<String>,
        /// Pane title (defaults to the command's basename).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Print the new pane snapshot as JSON.
        #[arg(long)]
        json: bool,
        /// Command and arguments, after `--`.
        #[arg(last = true, required = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Read an agent pane by numeric id, unique agent name, pane title, or `current`.
    Read {
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(long, value_name = "N")]
        lines: Option<usize>,
        #[arg(long)]
        scrollback: bool,
        #[arg(long)]
        json: bool,
    },
    /// Send terminal key names or literal text to an agent target.
    SendKeys {
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(short = 'l', long)]
        literal: bool,
        #[arg(value_name = "KEYS", required = true, allow_hyphen_values = true)]
        keys: Vec<String>,
    },
    /// Submit a sanitized, bracketed prompt to an unblocked recognized agent.
    Prompt {
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(value_name = "TEXT", allow_hyphen_values = true)]
        text: String,
        /// Wait for fresh output or a working state, then a settled state.
        #[arg(long, conflicts_with = "until")]
        wait: bool,
        /// Wait for fresh activity, then this exact state.
        #[arg(long, value_name = "STATE", value_parser = agent_state, conflicts_with = "wait")]
        until: Option<AgentStateKind>,
        /// Give up after S seconds and exit 2 (only valid with --wait/--until).
        #[arg(long, value_name = "S")]
        timeout: Option<u64>,
        /// Print the final pane snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Focus an agent target without attaching the TUI.
    Focus {
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Focus a pane and attach the TUI to its session.
    Attach {
        #[arg(value_name = "TARGET")]
        target: String,
    },
    /// Rename a pane.
    Rename {
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(value_name = "NAME", allow_hyphen_values = true)]
        name: String,
    },
    /// Print a pane's agent state and the reason for it.
    Explain {
        #[arg(value_name = "TARGET")]
        target: String,
        /// Print the pane snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Refresh agent-detection manifests from the repo (opt-in network call).
    UpdateManifests,
    /// Inspect the active manifest cache, or atomically reload local overrides.
    Manifests {
        /// Re-read bundled and user override manifests without restarting.
        #[arg(long)]
        reload: bool,
        /// Print JSON instead of an aligned table.
        #[arg(long)]
        json: bool,
    },
    /// Validate a manifest file against the daemon schema without installing it.
    ValidateManifest {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Wait until a pane reaches an agent state; exits 2 on timeout.
    Wait {
        #[arg(value_name = "TARGET")]
        target: String,
        /// One of: blocked, working, done, idle, unknown.
        #[arg(long, value_name = "STATE", value_parser = agent_state)]
        state: AgentStateKind,
        /// Give up after S seconds and exit 2.
        #[arg(long, value_name = "S")]
        timeout: Option<u64>,
    },
    /// Report an agent state to the daemon (used by agent hooks).
    Report {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        /// One of: blocked, working, done, idle, unknown.
        #[arg(value_name = "STATE", value_parser = agent_state)]
        state: AgentStateKind,
        /// Name recorded as the source of the report.
        #[arg(long, value_name = "NAME", default_value = "cli")]
        source: String,
        /// Canonical agent name associated with a native conversation id.
        #[arg(long, value_name = "AGENT")]
        native_agent: Option<String>,
        /// Agent-native conversation id reported by an integration hook.
        #[arg(long, value_name = "ID", conflicts_with = "native_session_path")]
        native_session_id: Option<String>,
        /// Agent-native session path reported by an integration hook.
        #[arg(long, value_name = "PATH", conflicts_with = "native_session_id")]
        native_session_path: Option<PathBuf>,
        /// Read one bounded JSON hook payload from a pipe and extract this
        /// integration's documented native session field.
        #[arg(long)]
        hook_json: bool,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum WorktreeCommand {
    /// Add a worktree workspace for BRANCH, rooted in the given workspace's repo.
    Add {
        /// Branch to check out (created from `--from` when it does not exist).
        #[arg(value_name = "BRANCH")]
        branch: String,
        /// Base ref for a new branch (defaults to the repo's current HEAD).
        #[arg(long = "from", value_name = "REF")]
        from: Option<String>,
        /// Alias for --from, naming the starting ref explicitly.
        #[arg(long, value_name = "REF", conflicts_with = "from")]
        base: Option<String>,
        /// Explicit directory for the new checkout.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        /// Workspace whose root repo to branch from (defaults to the active one).
        #[arg(short = 'w', long = "workspace", value_name = "NAME")]
        workspace: Option<String>,
    },
    /// Open an existing linked worktree without creating or copying it.
    Open {
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Workspace whose root repo owns the worktree (defaults to active).
        #[arg(short = 'w', long = "workspace", value_name = "NAME")]
        workspace: Option<String>,
    },
    /// Remove a worktree workspace by workspace name/id or branch name.
    Remove {
        /// Workspace name, workspace id, or branch of the worktree to remove.
        #[arg(value_name = "WS|BRANCH")]
        target: String,
        /// Close the workspace but leave the worktree directory on disk.
        #[arg(long)]
        keep: bool,
    },
    /// List worktree workspaces, showing their root and parent workspace.
    List {
        /// Print the matching workspaces as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum ConfigCommand {
    /// Write a documented starter configuration when no file exists.
    Init,
    /// Print the path of the config file.
    Path,
    /// Print the effective configuration as TOML.
    Show,
    /// Check the config file, exiting non-zero when it has problems.
    Validate,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum IntegrateCommand {
    /// List available integrations and whether their config directory exists.
    List,
    /// Claude Code hooks that report agent state to the daemon.
    ClaudeCode {
        /// Merge the hooks into ~/.claude/settings.json instead of printing them.
        #[arg(long)]
        write: bool,
        /// Remove only Ködade-managed hooks from the settings file.
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Gemini CLI hooks (Claude-compatible) that report agent state.
    GeminiCli {
        /// Merge the hooks into ~/.gemini/settings.json instead of printing them.
        #[arg(long)]
        write: bool,
        /// Remove only Ködade-managed hooks from the settings file.
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Codex lifecycle hooks that report agent state without replacing `notify`.
    Codex {
        /// Merge the entries into ~/.codex/hooks.json instead of printing them.
        #[arg(long)]
        write: bool,
        /// Retained for compatibility; Codex hooks never replace `notify`.
        #[arg(long)]
        force: bool,
        /// Remove only Ködade-managed hooks from ~/.codex/hooks.json.
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// GitHub Copilot CLI lifecycle hooks.
    Copilot {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Cursor lifecycle hooks.
    Cursor {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Factory Droid lifecycle hooks.
    Droid {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Kimi Code lifecycle hooks.
    Kimi {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Qwen Code lifecycle hooks.
    Qwen {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// OpenCode local plugin (official API documented; fixture-tested here).
    OpenCode {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Pi global extension (verified against installed Pi 0.85.1 docs).
    Pi {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Oh My Pi global extension.
    Omp {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Kilo global lifecycle plugin.
    Kilo {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
    /// Hermes global lifecycle plugin.
    Hermes {
        #[arg(long)]
        write: bool,
        #[arg(long, conflicts_with = "write")]
        remove: bool,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum PluginCommand {
    /// List installed extensions and whether they are enabled.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Register a local extension directory without copying it.
    Link { path: PathBuf },
    /// Clone a GitHub OWNER/REPO[/SUBDIR] extension into Ködade's managed directory.
    Install { source: String },
    /// Stop loading an extension while retaining its files and registry entry.
    Disable { id: String },
    /// Re-enable a registered extension.
    Enable { id: String },
    /// Remove a local link from the registry; its source directory remains untouched.
    Unlink { id: String },
    /// Remove a managed extension and its registry entry. Linked directories are protected.
    Uninstall { id: String },
    /// Run an extension action in the current workspace/pane context.
    Run { id: String, action: String },
    /// Open a named extension pane in a new Ködade tab.
    Pane { id: String, pane: String },
}

// Pane ids are plain integers on the wire; keep the error message script-friendly.
pub fn session_name(value: &str) -> Result<String, String> {
    kodade_cli_daemon::validate_session_name(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

fn pane_id(value: &str) -> Result<PaneId, String> {
    value
        .parse()
        .map(PaneId)
        .map_err(|_| format!("invalid pane id '{value}'"))
}

// Reuses the shared state parser so the CLI and the daemon agree on names.
fn agent_state(value: &str) -> Result<AgentStateKind, String> {
    crate::commands::parse_state(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn rejects_invalid_session_names_before_any_socket_access() {
        for name in [
            "../outside",
            "a/b",
            "a\\b",
            "line\nbreak",
            ".",
            "..",
            "",
            &"x".repeat(65),
        ] {
            assert!(
                Cli::try_parse_from(["kodade-cli", "-s", name, "session", "path"]).is_err(),
                "{name:?}"
            );
            assert!(
                Cli::try_parse_from(["kodade-cli", "session", "rename", name]).is_err(),
                "{name:?}"
            );
            assert!(
                Cli::try_parse_from(["kodade-cli", "daemon", name]).is_err(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn parses_plugin_lifecycle_commands() {
        assert!(matches!(
            parse(&["kodade-cli", "plugin", "link", "./demo"]).command,
            Some(Command::Plugin {
                command: PluginCommand::Link { .. }
            })
        ));
        assert!(matches!(
            parse(&["kodade-cli", "plugin", "run", "demo", "hello"]).command,
            Some(Command::Plugin { command: PluginCommand::Run { id, action } }) if id == "demo" && action == "hello"
        ));
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("arguments parse")
    }

    #[test]
    fn clap_definitions_are_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn update_modes_do_not_mix_check_show_or_explicit_install() {
        assert!(matches!(
            parse(&["kodade-cli", "update", "--channel", "preview", "--check"]).command,
            Some(Command::Update { check: true, .. })
        ));
        assert!(Cli::try_parse_from([
            "kodade-cli",
            "update",
            "--check",
            "--install-to",
            "/tmp/kodade-cli"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "kodade-cli",
            "update",
            "--show-channel",
            "--install-to",
            "/tmp/kodade-cli"
        ])
        .is_err());
    }

    #[test]
    fn parses_session_commands_and_report_states() {
        let cli = parse(&[
            "kodade-cli",
            "-s",
            "work",
            "agent",
            "report",
            "7",
            "working",
            "--source",
            "hook",
        ]);
        assert_eq!(cli.session, "work");
        assert_eq!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Report {
                    pane: PaneId(7),
                    state: AgentStateKind::Working,
                    source: "hook".into(),
                    native_agent: None,
                    native_session_id: None,
                    native_session_path: None,
                    hook_json: false,
                }
            })
        );
    }

    #[test]
    fn report_accepts_the_trailing_session_flag_used_by_hooks() {
        // The Claude Code hooks emit `agent report $PANE idle -s "$SESSION"`.
        let cli = parse(&["kodade-cli", "agent", "report", "7", "idle", "-s", "work"]);
        assert_eq!(cli.session, "work");
        assert_eq!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Report {
                    pane: PaneId(7),
                    state: AgentStateKind::Idle,
                    source: "cli".into(),
                    native_agent: None,
                    native_session_id: None,
                    native_session_path: None,
                    hook_json: false,
                }
            })
        );
    }

    #[test]
    fn report_accepts_native_conversation_identity() {
        let cli = parse(&[
            "kodade-cli",
            "agent",
            "report",
            "7",
            "working",
            "--source",
            "kodade:codex",
            "--native-agent",
            "codex",
            "--native-session-id",
            "thread-7",
        ]);
        assert_eq!(
            cli.command,
            Some(Command::Agent {
                command: AgentCommand::Report {
                    pane: PaneId(7),
                    state: AgentStateKind::Working,
                    source: "kodade:codex".into(),
                    native_agent: Some("codex".into()),
                    native_session_id: Some("thread-7".into()),
                    native_session_path: None,
                    hook_json: false,
                }
            })
        );
    }

    #[test]
    fn parses_send_attach_and_integrate() {
        assert_eq!(
            parse(&["kodade-cli", "send", "7", "hello", "--no-newline"]).command,
            Some(Command::Send {
                pane: PaneId(7),
                text: "hello".into(),
                no_newline: true,
            })
        );
        let attach = parse(&["kodade-cli"]);
        assert_eq!(attach.session, DEFAULT_SESSION);
        assert_eq!(attach.command, None);
        assert_eq!(
            parse(&["kodade-cli", "integrate", "claude-code", "--write"]).command,
            Some(Command::Integrate {
                target: IntegrateCommand::ClaudeCode {
                    write: true,
                    remove: false
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "daemon", "work"]).command,
            Some(Command::Daemon {
                session: Some("work".into())
            })
        );
    }

    #[test]
    fn text_and_name_accept_leading_hyphens() {
        // Agent CLIs are driven with values like `-y` or `--continue`, and the
        // hand-rolled parser accepted them; `--no-newline` still wins on either side.
        assert_eq!(
            parse(&["kodade-cli", "send", "1", "-y"]).command,
            Some(Command::Send {
                pane: PaneId(1),
                text: "-y".into(),
                no_newline: false,
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "send", "1", "--no-newline", "hi"]).command,
            Some(Command::Send {
                pane: PaneId(1),
                text: "hi".into(),
                no_newline: true,
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "send", "1", "hi", "--no-newline"]).command,
            Some(Command::Send {
                pane: PaneId(1),
                text: "hi".into(),
                no_newline: true,
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "agent", "rename", "1", "--foo"]).command,
            Some(Command::Agent {
                command: AgentCommand::Rename {
                    target: "1".into(),
                    name: "--foo".into(),
                }
            })
        );
        // `--` still forces the next value through verbatim.
        assert_eq!(
            parse(&["kodade-cli", "send", "1", "--", "--no-newline"]).command,
            Some(Command::Send {
                pane: PaneId(1),
                text: "--no-newline".into(),
                no_newline: false,
            })
        );
    }

    #[test]
    fn parses_pane_read() {
        assert_eq!(
            parse(&[
                "kodade-cli",
                "pane",
                "read",
                "7",
                "--lines",
                "5",
                "--scrollback"
            ])
            .command,
            Some(Command::Pane {
                command: PaneCommand::Read {
                    pane: PaneId(7),
                    lines: Some(5),
                    scrollback: true,
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "read", "2"]).command,
            Some(Command::Pane {
                command: PaneCommand::Read {
                    pane: PaneId(2),
                    lines: None,
                    scrollback: false,
                }
            })
        );
    }

    #[test]
    fn parses_remote_flag_and_session_path() {
        let cli = parse(&["kodade-cli", "--remote", "user@host", "-s", "work", "ls"]);
        assert_eq!(cli.remote.as_deref(), Some("user@host"));
        assert_eq!(cli.session, "work");
        assert_eq!(
            parse(&["kodade-cli", "session", "path"]).command,
            Some(Command::Session {
                command: SessionCommand::Path
            })
        );
        // The global flag also parses after the subcommand.
        assert_eq!(
            parse(&["kodade-cli", "session", "path", "--remote", "host"]).remote,
            Some("host".into())
        );
    }

    #[test]
    fn parses_worktree_subcommands() {
        assert_eq!(
            parse(&["kodade-cli", "worktree", "add", "feat-a", "--from", "main"]).command,
            Some(Command::Worktree {
                command: WorktreeCommand::Add {
                    branch: "feat-a".into(),
                    from: Some("main".into()),
                    base: None,
                    path: None,
                    workspace: None,
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "worktree", "remove", "feat-a", "--keep"]).command,
            Some(Command::Worktree {
                command: WorktreeCommand::Remove {
                    target: "feat-a".into(),
                    keep: true,
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "worktree", "list", "--json"]).command,
            Some(Command::Worktree {
                command: WorktreeCommand::List { json: true }
            })
        );
    }

    #[test]
    fn parses_config_subcommands() {
        assert_eq!(
            parse(&["kodade-cli", "config", "path"]).command,
            Some(Command::Config {
                command: ConfigCommand::Path
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "config", "validate"]).command,
            Some(Command::Config {
                command: ConfigCommand::Validate
            })
        );
        assert!(Cli::try_parse_from(["kodade-cli", "config", "nope"]).is_err());
    }

    #[test]
    fn rejects_unknown_report_states_and_integrations() {
        assert!(Cli::try_parse_from(["kodade-cli", "agent", "report", "7", "busy"]).is_err());
        assert!(Cli::try_parse_from(["kodade-cli", "integrate", "nope"]).is_err());
    }

    #[test]
    fn parses_integrate_and_manifest_subcommands() {
        assert_eq!(
            parse(&["kodade-cli", "integrate", "list"]).command,
            Some(Command::Integrate {
                target: IntegrateCommand::List
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "integrate", "codex", "--write", "--force"]).command,
            Some(Command::Integrate {
                target: IntegrateCommand::Codex {
                    write: true,
                    force: true,
                    remove: false,
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "integrate", "gemini-cli", "--write"]).command,
            Some(Command::Integrate {
                target: IntegrateCommand::GeminiCli {
                    write: true,
                    remove: false
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "agent", "update-manifests"]).command,
            Some(Command::Agent {
                command: AgentCommand::UpdateManifests
            })
        );
    }
    #[test]
    fn parses_every_pane_verb() {
        assert_eq!(
            parse(&["kodade-cli", "pane", "ls", "--json"]).command,
            Some(Command::Pane {
                command: PaneCommand::Ls { json: true }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "send-keys", "3", "codex", "Enter"]).command,
            Some(Command::Pane {
                command: PaneCommand::SendKeys {
                    pane: PaneId(3),
                    literal: false,
                    keys: vec!["codex".into(), "Enter".into()],
                }
            })
        );
        assert_eq!(
            parse(&[
                "kodade-cli",
                "pane",
                "send-keys",
                "3",
                "--literal",
                "Hello there"
            ])
            .command,
            Some(Command::Pane {
                command: PaneCommand::SendKeys {
                    pane: PaneId(3),
                    literal: true,
                    keys: vec!["Hello there".into()],
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "kill", "3"]).command,
            Some(Command::Pane {
                command: PaneCommand::Kill { pane: PaneId(3) }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "focus", "3"]).command,
            Some(Command::Pane {
                command: PaneCommand::Focus { pane: PaneId(3) }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "zoom", "3"]).command,
            Some(Command::Pane {
                command: PaneCommand::Zoom { pane: PaneId(3) }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "swap", "3", "left"]).command,
            Some(Command::Pane {
                command: PaneCommand::Swap {
                    pane: PaneId(3),
                    direction: DirectionArg::Left,
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "move", "3", "--tab", "agents"]).command,
            Some(Command::Pane {
                command: PaneCommand::Move {
                    pane: PaneId(3),
                    tab: "agents".into(),
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "pane", "resize", "3", "up", "-2"]).command,
            Some(Command::Pane {
                command: PaneCommand::Resize {
                    pane: PaneId(3),
                    direction: DirectionArg::Up,
                    cells: -2,
                }
            })
        );
        assert_eq!(
            parse(&[
                "kodade-cli",
                "pane",
                "wait-output",
                "3",
                "--match",
                "done",
                "--timeout",
                "5"
            ])
            .command,
            Some(Command::Pane {
                command: PaneCommand::WaitOutput {
                    pane: PaneId(3),
                    text: "done".into(),
                    regex: false,
                    scrollback: false,
                    timeout: Some(5),
                }
            })
        );
        // `--match` is a substring, so a regex-looking value is still literal.
        assert!(Cli::try_parse_from(["kodade-cli", "pane", "swap", "3", "sideways"]).is_err());
    }

    #[test]
    fn parses_agent_automation_commands() {
        assert_eq!(
            parse(&[
                "kodade-cli",
                "agent",
                "start",
                "--name",
                "reviewer",
                "--",
                "codex",
                "--full-auto"
            ])
            .command,
            Some(Command::Agent {
                command: AgentCommand::Start {
                    workspace: None,
                    tab: None,
                    name: Some("reviewer".into()),
                    json: false,
                    command: vec!["codex".into(), "--full-auto".into()],
                }
            })
        );
        assert_eq!(
            parse(&[
                "kodade-cli",
                "agent",
                "prompt",
                "reviewer",
                "review this",
                "--until",
                "done",
                "--timeout",
                "10",
                "--json"
            ])
            .command,
            Some(Command::Agent {
                command: AgentCommand::Prompt {
                    target: "reviewer".into(),
                    text: "review this".into(),
                    wait: false,
                    until: Some(AgentStateKind::Done),
                    timeout: Some(10),
                    json: true,
                }
            })
        );
        assert_eq!(
            parse(&[
                "kodade-cli",
                "agent",
                "read",
                "current",
                "--scrollback",
                "--json"
            ])
            .command,
            Some(Command::Agent {
                command: AgentCommand::Read {
                    target: "current".into(),
                    lines: None,
                    scrollback: true,
                    json: true,
                }
            })
        );
    }

    #[test]
    fn parses_tab_and_workspace_verbs() {
        assert_eq!(
            parse(&["kodade-cli", "tab", "ls"]).command,
            Some(Command::Tab {
                command: TabCommand::Ls { json: false }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "tab", "new", "-w", "repo", "--name", "agents"]).command,
            Some(Command::Tab {
                command: TabCommand::New {
                    workspace: Some("repo".into()),
                    name: Some("agents".into()),
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "tab", "rename", "2", "agents"]).command,
            Some(Command::Tab {
                command: TabCommand::Rename {
                    tab: "2".into(),
                    name: "agents".into(),
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "tab", "close", "agents"]).command,
            Some(Command::Tab {
                command: TabCommand::Close {
                    tab: "agents".into()
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "tab", "select", "agents"]).command,
            Some(Command::Tab {
                command: TabCommand::Select {
                    tab: "agents".into()
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "workspace", "new", "repo", "/tmp/repo"]).command,
            Some(Command::Workspace {
                command: WorkspaceCommand::New {
                    name: "repo".into(),
                    path: Some(PathBuf::from("/tmp/repo")),
                    env: vec![],
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "workspace", "ls", "--json"]).command,
            Some(Command::Workspace {
                command: WorkspaceCommand::Ls { json: true }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "workspace", "rename", "1", "repo"]).command,
            Some(Command::Workspace {
                command: WorkspaceCommand::Rename {
                    workspace: "1".into(),
                    name: "repo".into(),
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "workspace", "close", "repo"]).command,
            Some(Command::Workspace {
                command: WorkspaceCommand::Close {
                    workspace: "repo".into()
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "workspace", "color", "repo", "#e7a33b"]).command,
            Some(Command::Workspace {
                command: WorkspaceCommand::Color {
                    workspace: "repo".into(),
                    color: "#e7a33b".into(),
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "workspace", "color", "repo", "off"]).command,
            Some(Command::Workspace {
                command: WorkspaceCommand::Color {
                    workspace: "repo".into(),
                    color: "off".into(),
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "workspace", "select", "repo"]).command,
            Some(Command::Workspace {
                command: WorkspaceCommand::Select {
                    workspace: "repo".into()
                }
            })
        );
    }

    #[test]
    fn parses_session_layout_event_and_completion_verbs() {
        assert_eq!(
            parse(&["kodade-cli", "session", "ls", "--json"]).command,
            Some(Command::Session {
                command: SessionCommand::Ls { json: true }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "session", "kill", "work"]).command,
            Some(Command::Session {
                command: SessionCommand::Kill {
                    name: Some("work".into())
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "session", "rename", "work"]).command,
            Some(Command::Session {
                command: SessionCommand::Rename {
                    name: "work".into()
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "layout", "export"]).command,
            Some(Command::Layout {
                command: LayoutCommand::Export { file: None }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "layout", "apply", "layout.json"]).command,
            Some(Command::Layout {
                command: LayoutCommand::Apply {
                    file: PathBuf::from("layout.json")
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "events", "--json"]).command,
            Some(Command::Events { json: true })
        );
        assert_eq!(
            parse(&["kodade-cli", "completion", "zsh"]).command,
            Some(Command::Completion {
                shell: clap_complete::Shell::Zsh
            })
        );
        assert!(Cli::try_parse_from(["kodade-cli", "completion", "csh"]).is_err());
    }

    #[test]
    fn parses_agent_wait() {
        assert_eq!(
            parse(&[
                "kodade-cli",
                "agent",
                "wait",
                "3",
                "--state",
                "blocked",
                "--timeout",
                "10"
            ])
            .command,
            Some(Command::Agent {
                command: AgentCommand::Wait {
                    target: "3".into(),
                    state: AgentStateKind::Blocked,
                    timeout: Some(10),
                }
            })
        );
        assert!(
            Cli::try_parse_from(["kodade-cli", "agent", "wait", "3", "--state", "busy"]).is_err()
        );
    }

    #[test]
    fn parses_machine_catalog_commands() {
        assert_eq!(
            parse(&[
                "kodade-cli",
                "machine",
                "add",
                "buildbox",
                "--label",
                "Build",
                "--remote-session",
                "agents"
            ])
            .command,
            Some(Command::Machine {
                command: MachineCommand::Add {
                    target: "buildbox".into(),
                    label: "Build".into(),
                    session: Some("agents".into()),
                    install: false,
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "machine", "disable", "m-1"]).command,
            Some(Command::Machine {
                command: MachineCommand::Disable { id: "m-1".into() }
            })
        );
    }
}
