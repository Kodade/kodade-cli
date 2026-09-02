mod app;
mod cli;
mod commands;
mod config;
mod input;
mod mode;
mod render;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kodade_cli_proto::{decode, encode, ClientMessage, ServerMessage};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{env, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Cli::parse();
    let session = args.session;
    let config = config::Config::load();
    match args.command {
        // No subcommand attaches the TUI to the session.
        None => attach(&session, &config).await,
        Some(cli::Command::Daemon { session: name }) => {
            kodade_cli_daemon::run(name.unwrap_or(session)).await
        }
        Some(cli::Command::Ls { json }) => {
            let layout =
                commands::layout(commands::request(&session, commands::layout_query()).await?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&layout)?);
            } else {
                println!("{}", commands::format_ls(&layout));
            }
            Ok(())
        }
        Some(cli::Command::Agent { command }) => agent(&session, &config, command).await,
        Some(cli::Command::Send {
            pane,
            text,
            no_newline,
        }) => {
            let bytes = if no_newline {
                text.into_bytes()
            } else {
                format!("{text}\r").into_bytes()
            };
            commands::layout(
                commands::request(&session, ClientMessage::SendToPane { id: pane, bytes }).await?,
            )?;
            Ok(())
        }
        Some(cli::Command::KillSession) => {
            match commands::request(&session, ClientMessage::KillSession).await? {
                ServerMessage::Shutdown => Ok(()),
                message => commands::layout(message).map(|_| ()),
            }
        }
        Some(cli::Command::Integrate { target }) => match target {
            cli::IntegrateCommand::List => commands::integrate_list(),
            cli::IntegrateCommand::ClaudeCode { write } => commands::integrate_claude_code(write),
            cli::IntegrateCommand::GeminiCli { write } => commands::integrate_gemini(write, false),
            cli::IntegrateCommand::Codex { write, force } => {
                commands::integrate_codex(write, force)
            }
        },
    }
}

/// `agent` subcommands: read pane state or report it back to the daemon.
async fn agent(session: &str, config: &config::Config, command: cli::AgentCommand) -> Result<()> {
    match command {
        cli::AgentCommand::Ls { json } => {
            let layout =
                commands::layout(commands::request(session, commands::layout_query()).await?)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&commands::agent_panes(&layout))?
                );
            } else {
                println!("{}", commands::format_agents(&layout));
            }
            Ok(())
        }
        cli::AgentCommand::Attach { pane } => {
            commands::layout(
                commands::request(session, ClientMessage::FocusPaneId { id: pane }).await?,
            )?;
            attach(session, config).await
        }
        cli::AgentCommand::Rename { pane, name } => {
            commands::layout(
                commands::request(session, ClientMessage::RenamePaneId { id: pane, name }).await?,
            )?;
            Ok(())
        }
        cli::AgentCommand::Explain { pane, json } => {
            let layout =
                commands::layout(commands::request(session, commands::layout_query()).await?)?;
            let pane = commands::find_pane(&layout, pane)?;
            if json {
                println!("{}", serde_json::to_string_pretty(pane)?);
            } else {
                println!("{}", commands::format_explain(pane));
            }
            Ok(())
        }
        cli::AgentCommand::UpdateManifests => commands::update_manifests(),
        cli::AgentCommand::Report {
            pane,
            state,
            source,
        } => {
            commands::layout(
                commands::request(
                    session,
                    ClientMessage::AgentState {
                        pane,
                        state,
                        source,
                    },
                )
                .await?,
            )?;
            Ok(())
        }
    }
}

/// Connects to the session daemon, starting it in the background when missing.
async fn attach(session: &str, config: &config::Config) -> Result<()> {
    let path = kodade_cli_daemon::socket_path(session);
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            std::process::Command::new(env::current_exe().context("locate binary")?)
                .arg("daemon")
                .arg(session)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            loop {
                if let Ok(s) = UnixStream::connect(&path).await {
                    break s;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Err(e) => return Err(e.into()),
    };
    tui(stream, config, session).await
}

/// Sets up the terminal, hands the socket to `App`, and always restores it.
async fn tui(stream: UnixStream, config: &config::Config, session: &str) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let update = match decode(line.as_bytes()) {
                Ok(ServerMessage::Layout(layout)) => app::Update::Layout(layout),
                Ok(ServerMessage::Welcome { session }) => app::Update::Session(session),
                _ => continue,
            };
            if tx.send(update).await.is_err() {
                break;
            }
        }
    });
    let mut state = app::App::new(config, session);
    let (cols, rows) = crossterm::terminal::size()?;
    writer
        .write_all(&encode(&ClientMessage::Hello {
            cols: state.pane_cols(cols),
            rows,
        })?)
        .await?;
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    if config.mouse {
        execute!(stdout, crossterm::event::EnableMouseCapture)?;
    }
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = state.run(&mut term, &mut writer, &mut rx).await;
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    if config.mouse {
        execute!(term.backend_mut(), crossterm::event::DisableMouseCapture)?;
    }
    term.show_cursor()?;
    result
}
