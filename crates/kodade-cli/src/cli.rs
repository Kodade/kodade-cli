//! Command-line surface for the `kodade-cli` binary.
//!
//! clap owns parsing, help, and `--version`; `main.rs` only dispatches the
//! parsed tree. Every read command accepts `--json` so scripts can consume the
//! proto snapshots directly.

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
    /// Stop the session and its daemon.
    KillSession,
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
pub enum IntegrateCommand {
    /// Claude Code hooks that report agent state to the daemon.
    ClaudeCode {
        /// Merge the hooks into ~/.claude/settings.json instead of printing them.
        #[arg(long)]
        write: bool,
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
    fn rejects_unknown_states_and_pane_ids() {
        assert!(Cli::try_parse_from(["kodade-cli", "agent", "report", "7", "busy"]).is_err());
        assert!(Cli::try_parse_from(["kodade-cli", "agent", "explain", "x"]).is_err());
        assert!(Cli::try_parse_from(["kodade-cli", "integrate", "codex"]).is_err());
    }
}
