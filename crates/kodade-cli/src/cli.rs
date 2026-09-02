//! Command-line surface for the `kodade-cli` binary.
//!
//! clap owns parsing, help, and `--version`; `main.rs` only dispatches the
//! parsed tree. Every read command accepts `--json` so scripts can consume the
//! proto snapshots directly.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use kodade_cli_proto::{AgentStateKind, PaneId};

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
        default_value = DEFAULT_SESSION
    )]
    pub session: String,

    /// Attach to or query a daemon on `USER@HOST` over an SSH-forwarded socket
    /// instead of the local one (#23). Requires `kodade-cli` on the remote host.
    #[arg(long = "remote", global = true, value_name = "USER@HOST")]
    pub remote: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Run the session daemon (started automatically when attaching).
    Daemon {
        /// Session name; defaults to the global --session value.
        session: Option<String>,
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
    /// Work with a pane's contents. #16 extends this group with more verbs.
    Pane {
        #[command(subcommand)]
        command: PaneCommand,
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
    /// Inspect sessions. #16 extends this group with `ls`, `kill`, and `rename`.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
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
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum AgentCommand {
    /// List recognized agents and their states.
    Ls {
        /// Print the matching pane snapshots as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Focus a pane and attach the TUI to its session.
    Attach {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
    },
    /// Rename a pane.
    Rename {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        #[arg(value_name = "NAME", allow_hyphen_values = true)]
        name: String,
    },
    /// Print a pane's agent state and the reason for it.
    Explain {
        #[arg(value_name = "PANE", value_parser = pane_id)]
        pane: PaneId,
        /// Print the pane snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Refresh agent-detection manifests from the repo (opt-in network call).
    UpdateManifests,
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
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum SessionCommand {
    /// Print the daemon socket path for the session. `--remote` prints the
    /// remote host's path (used to set up the forwarded socket).
    Path,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum PaneCommand {
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
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum ConfigCommand {
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
    },
    /// Gemini CLI hooks (Claude-compatible) that report agent state.
    GeminiCli {
        /// Merge the hooks into ~/.gemini/settings.json instead of printing them.
        #[arg(long)]
        write: bool,
    },
    /// Codex `notify` entry that reports agent state.
    Codex {
        /// Merge the entry into ~/.codex/config.toml instead of printing it.
        #[arg(long)]
        write: bool,
        /// Replace an existing `notify` entry instead of refusing.
        #[arg(long)]
        force: bool,
    },
}

// Pane ids are plain integers on the wire; keep the error message script-friendly.
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

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("arguments parse")
    }

    #[test]
    fn clap_definitions_are_valid() {
        Cli::command().debug_assert();
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
                target: IntegrateCommand::ClaudeCode { write: true }
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
                    pane: PaneId(1),
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
    fn rejects_unknown_states_and_pane_ids() {
        assert!(Cli::try_parse_from(["kodade-cli", "agent", "report", "7", "busy"]).is_err());
        assert!(Cli::try_parse_from(["kodade-cli", "agent", "explain", "x"]).is_err());
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
                    force: true
                }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "integrate", "gemini-cli", "--write"]).command,
            Some(Command::Integrate {
                target: IntegrateCommand::GeminiCli { write: true }
            })
        );
        assert_eq!(
            parse(&["kodade-cli", "agent", "update-manifests"]).command,
            Some(Command::Agent {
                command: AgentCommand::UpdateManifests
            })
        );
    }
}
