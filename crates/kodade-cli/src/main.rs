mod app;
mod cli;
mod commands;
mod config;
mod help;
mod input;
mod mode;
mod notify;
mod overlay;
mod paste;
mod picker;
mod remote;
mod render;
mod selection;
mod settings;
mod state;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kodade_cli_proto::{decode, encode, ClientMessage, ServerMessage, SplitAxis, PROTOCOL_VERSION};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{env, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Cli::parse();
    let session = args.session.clone();
    let remote = args.remote.clone();

    // Commands that never open a session socket run locally; `session` verbs
    // pass through to the host when `--remote` is set.
    let needs_socket = matches!(
        args.command,
        None | Some(
            cli::Command::Ls { .. }
                | cli::Command::Agent { .. }
                | cli::Command::Pane { .. }
                | cli::Command::Send { .. }
                | cli::Command::New { .. }
                | cli::Command::Run { .. }
                | cli::Command::Split { .. }
                | cli::Command::NewTab { .. }
                | cli::Command::KillSession
        )
    );
    // `--remote` sets up the SSH forward once; `_tunnel` must outlive every
    // request below so the forward stays open (dropping it removes the socket).
    let (socket, _tunnel) = if needs_socket {
        remote::resolve_socket(&args).await?
    } else {
        (kodade_cli_daemon::socket_path(&session), None)
    };
    let command = args.command;

    // The config is only loaded where it is used, so `config validate` does not
    // print its warnings twice.
    match command {
        // No subcommand attaches the TUI to the session.
        None => attach(&socket, &session, &config::Config::load()).await,
        Some(cli::Command::Daemon { session: name }) => {
            kodade_cli_daemon::run(name.unwrap_or(session)).await
        }
        Some(cli::Command::Session { command }) => {
            session_command(remote.as_deref(), &session, command).await
        }
        Some(cli::Command::Ls { json }) => {
            let layout =
                commands::layout(commands::request(&socket, commands::layout_query()).await?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&layout)?);
            } else {
                println!("{}", commands::format_ls(&layout));
                // Mark a session that was rebuilt from a state file and not yet attached (#9).
                if layout.restored {
                    println!("(restored)");
                }
            }
            Ok(())
        }
        Some(cli::Command::Agent { command }) => {
            agent(&socket, &session, &config::Config::load(), command).await
        }
        Some(cli::Command::Pane { command }) => pane(&socket, command).await,
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
                commands::request(&socket, ClientMessage::SendToPane { id: pane, bytes }).await?,
            )?;
            Ok(())
        }
        Some(cli::Command::New { workspace, path }) => {
            let layout =
                commands::layout(commands::request(&socket, commands::layout_query()).await?)?;
            // Selecting an existing name is idempotent; otherwise create it.
            if let Ok(id) = commands::resolve_workspace(&layout, &workspace) {
                commands::request(&socket, ClientMessage::SelectWorkspace { id }).await?;
                println!("{}", id.0);
            } else {
                let reply = commands::layout(
                    commands::request(
                        &socket,
                        ClientMessage::NewWorkspace {
                            name: workspace,
                            root: path,
                        },
                    )
                    .await?,
                )?;
                println!("{}", reply.active_workspace.0);
            }
            Ok(())
        }
        Some(cli::Command::Run {
            workspace,
            tab,
            name,
            command,
        }) => {
            let (ws, tab) = resolve_target(&socket, workspace, tab).await?;
            let reply = commands::layout(
                commands::request(
                    &socket,
                    ClientMessage::NewPane {
                        workspace: ws,
                        tab,
                        split: None,
                        command: Some(command),
                        name,
                    },
                )
                .await?,
            )?;
            println!("{}", commands::focused_pane(&reply)?.0);
            Ok(())
        }
        Some(cli::Command::Split {
            down,
            pane,
            command,
        }) => {
            if let Some(pane) = pane {
                commands::request(&socket, ClientMessage::FocusPaneId { id: pane }).await?;
            }
            let axis = if down {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            };
            let reply = commands::layout(
                commands::request(
                    &socket,
                    ClientMessage::NewPane {
                        workspace: None,
                        tab: None,
                        split: Some(axis),
                        command: (!command.is_empty()).then_some(command),
                        name: None,
                    },
                )
                .await?,
            )?;
            println!("{}", commands::focused_pane(&reply)?.0);
            Ok(())
        }
        Some(cli::Command::NewTab { workspace, name }) => {
            let (ws, _) = resolve_target(&socket, workspace, None).await?;
            let reply = commands::layout(
                commands::request(
                    &socket,
                    ClientMessage::NewPane {
                        workspace: ws,
                        tab: None,
                        split: None,
                        command: None,
                        name,
                    },
                )
                .await?,
            )?;
            println!("{}", commands::focused_pane(&reply)?.0);
            Ok(())
        }
        Some(cli::Command::KillSession) => {
            match commands::request(&socket, ClientMessage::KillSession).await? {
                ServerMessage::Shutdown => Ok(()),
                message => commands::layout(message).map(|_| ()),
            }
        }
        Some(cli::Command::Config { command }) => {
            config_command(command);
            Ok(())
        }
        Some(cli::Command::Keys { json }) => {
            let config = config::Config::load();
            if json {
                println!("{}", help::keys_json(&config));
            } else {
                print!("{}", help::keys_text(&config));
            }
            Ok(())
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

/// Resolve optional `-w`/`-t` names to ids, fetching one layout snapshot only
/// when a name is actually given.
async fn resolve_target(
    socket: &Path,
    workspace: Option<String>,
    tab: Option<String>,
) -> Result<(
    Option<kodade_cli_proto::WorkspaceId>,
    Option<kodade_cli_proto::TabId>,
)> {
    if workspace.is_none() && tab.is_none() {
        return Ok((None, None));
    }
    let layout = commands::layout(commands::request(socket, commands::layout_query()).await?)?;
    let ws = workspace
        .as_deref()
        .map(|name| commands::resolve_workspace(&layout, name))
        .transpose()?;
    let tab = tab
        .as_deref()
        .map(|name| commands::resolve_tab(&layout, ws, name))
        .transpose()?;
    Ok((ws, tab))
}

/// `config` subcommands: locate, print, or check the config file.
fn config_command(command: cli::ConfigCommand) {
    match command {
        cli::ConfigCommand::Path => println!("{}", config::config_path().display()),
        cli::ConfigCommand::Show => match config::Config::load_checked() {
            Ok(config) => print!("{}", config.to_toml()),
            Err(error) => {
                eprintln!("kodade-cli: {error}");
                std::process::exit(1);
            }
        },
        cli::ConfigCommand::Validate => {
            let path = config::config_path();
            // No file at all is a normal state: the defaults apply.
            if !path.exists() {
                println!("{}: not found (defaults in use)", path.display());
                return;
            }
            match config::Config::load_checked() {
                Ok(config) if config.warnings.is_empty() => println!("{}: ok", path.display()),
                Ok(config) => {
                    for warning in &config.warnings {
                        println!("{}: {warning}", path.display());
                    }
                    std::process::exit(1);
                }
                Err(error) => {
                    println!("{error}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// `session` subcommands. Locally these inspect this host's daemon; with
/// `--remote` they run on the host over SSH (#23).
async fn session_command(
    remote: Option<&str>,
    session: &str,
    command: cli::SessionCommand,
) -> Result<()> {
    if let Some(host) = remote {
        return remote::run_session(host, session, &command).await;
    }
    match command {
        cli::SessionCommand::Path => {
            println!("{}", kodade_cli_daemon::socket_path(session).display());
            Ok(())
        }
    }
}

/// `agent` subcommands: read pane state or report it back to the daemon.
async fn agent(
    socket: &Path,
    session: &str,
    config: &config::Config,
    command: cli::AgentCommand,
) -> Result<()> {
    match command {
        cli::AgentCommand::Ls { json } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
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
                commands::request(socket, ClientMessage::FocusPaneId { id: pane }).await?,
            )?;
            attach(socket, session, config).await
        }
        cli::AgentCommand::Rename { pane, name } => {
            commands::layout(
                commands::request(socket, ClientMessage::RenamePaneId { id: pane, name }).await?,
            )?;
            Ok(())
        }
        cli::AgentCommand::Explain { pane, json } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
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
                    socket,
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

/// `pane` subcommands: read a pane's text for scripting.
async fn pane(socket: &Path, command: cli::PaneCommand) -> Result<()> {
    match command {
        cli::PaneCommand::Read {
            pane,
            lines,
            scrollback,
        } => {
            let reply = commands::request(
                socket,
                ClientMessage::ReadPane {
                    id: pane,
                    scrollback,
                    lines,
                },
            )
            .await?;
            match reply {
                ServerMessage::PaneText { text, .. } => {
                    println!("{text}");
                    Ok(())
                }
                other => anyhow::bail!("unexpected reply: {other:?}"),
            }
        }
    }
}

/// Connects to the session daemon at `socket`, starting a local one in the
/// background when the socket is the local path and nothing answers. A remote
/// (forwarded) socket is never auto-started here — `remote::resolve_socket`
/// already ensured the remote daemon is up.
async fn attach(socket: &Path, session: &str, config: &config::Config) -> Result<()> {
    // Only spawn a daemon for this host's own socket; a `--remote` tunnel socket
    // differs from the local path and must not trigger a local daemon.
    let can_spawn = socket == kodade_cli_daemon::socket_path(session).as_path();
    let stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(e)
            if can_spawn
                && matches!(
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
                if let Ok(s) = UnixStream::connect(socket).await {
                    break s;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Err(e) => return Err(e.into()),
    };
    tui(stream, config, session, socket).await
}

/// Sets up the terminal, hands the socket to `App`, and always restores it.
async fn tui(
    stream: UnixStream,
    config: &config::Config,
    session: &str,
    socket: &Path,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut state = app::App::new(config, session, socket.to_path_buf());
    let (cols, rows) = crossterm::terminal::size()?;
    // Collapse the sidebar before the first Hello so a narrow launch starts with
    // the right pane width (#19).
    state.apply_auto_hide(cols);
    // Send a versioned Hello, then verify the daemon speaks our protocol before
    // touching the terminal so a mismatch prints cleanly and exits 1 (#23).
    writer
        .write_all(&encode(&ClientMessage::Hello {
            cols: state.pane_cols(cols),
            rows,
            version: PROTOCOL_VERSION,
        })?)
        .await?;
    handshake(&mut lines, &mut state).await?;
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let update = match decode(line.as_bytes()) {
                Ok(ServerMessage::Layout(layout)) => app::Update::Layout(layout),
                Ok(ServerMessage::Welcome { session, .. }) => app::Update::Session(session),
                Ok(ServerMessage::Notification(notification)) => {
                    app::Update::Notification(notification)
                }
                _ => continue,
            };
            if tx.send(update).await.is_err() {
                break;
            }
        }
    });
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Bracketed paste lets the client tell a paste from typing (#21).
    execute!(stdout, EnableBracketedPaste)?;
    if config.mouse {
        execute!(stdout, crossterm::event::EnableMouseCapture)?;
    }
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = state.run(&mut term, &mut writer, &mut rx).await;
    disable_raw_mode()?;
    execute!(term.backend_mut(), DisableBracketedPaste)?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    if config.mouse {
        execute!(term.backend_mut(), crossterm::event::DisableMouseCapture)?;
    }
    term.show_cursor()?;
    result
}

/// Read the daemon's opening `Welcome` and verify its protocol version before
/// the terminal is put into raw mode. A mismatch (or an `Error`, which is what
/// the daemon sends when it rejects our `Hello`) prints a message and exits 1
/// so the user never sees a half-drawn screen (#23).
async fn handshake(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    state: &mut app::App,
) -> Result<()> {
    loop {
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection during handshake"))?;
        match decode::<ServerMessage>(line.as_bytes()) {
            Ok(ServerMessage::Welcome { session, version }) => {
                if version != PROTOCOL_VERSION {
                    eprintln!(
                        "protocol version mismatch: client {PROTOCOL_VERSION}, daemon {version} — upgrade kodade-cli on both ends"
                    );
                    std::process::exit(1);
                }
                state.handle_session(session);
                return Ok(());
            }
            Ok(ServerMessage::Error { message }) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
            // Ignore anything before the Welcome (there should be nothing).
            _ => continue,
        }
    }
}
