mod app;
mod cli;
mod commands;
mod config;
mod help;
mod input;
mod keys;
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
use kodade_cli_proto::{
    decode, encode, ClientMessage, Direction, Event, QueryKind, ServerMessage, SplitAxis,
    PROTOCOL_VERSION,
};
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
                | cli::Command::Tab { .. }
                | cli::Command::Workspace { .. }
                | cli::Command::Layout { .. }
                | cli::Command::Events { .. }
                | cli::Command::Send { .. }
                | cli::Command::New { .. }
                | cli::Command::Run { .. }
                | cli::Command::Split { .. }
                | cli::Command::NewTab { .. }
                | cli::Command::Worktree { .. }
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
        Some(cli::Command::Worktree { command }) => worktree(&socket, command).await,
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
        // `new` is the alias of `workspace new`.
        Some(cli::Command::New { workspace, path }) => {
            new_workspace(&socket, workspace, path).await
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
        Some(cli::Command::Tab { command }) => tab(&socket, command).await,
        Some(cli::Command::Workspace { command }) => workspace(&socket, command).await,
        Some(cli::Command::Layout { command }) => layout_command(&socket, command).await,
        Some(cli::Command::Events { json }) => commands::stream_events(&socket, json).await,
        Some(cli::Command::Completion { shell }) => {
            let mut command = <cli::Cli as clap::CommandFactory>::command();
            clap_complete::generate(
                shell,
                &mut command,
                "kodade-cli",
                &mut std::io::stdout().lock(),
            );
            Ok(())
        }
    }
}

/// `pane` subcommands. Pane-targeted actions the daemon only applies to the
/// focused pane are prefixed with a `FocusPaneId`, which is also what the
/// equivalent key binding would do.
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
        cli::PaneCommand::Ls { json } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&layout.panes)?);
            } else {
                println!("{}", commands::format_panes(&layout));
            }
            Ok(())
        }
        cli::PaneCommand::SendKeys {
            pane,
            keys,
            literal,
        } => {
            let bytes = if literal {
                keys::literal(&keys)
            } else {
                keys::parse_all(&keys)?
            };
            commands::layout(
                commands::request(socket, ClientMessage::SendToPane { id: pane, bytes }).await?,
            )?;
            Ok(())
        }
        cli::PaneCommand::Kill { pane } => focus_then(socket, pane, ClientMessage::ClosePane).await,
        cli::PaneCommand::Focus { pane } => {
            commands::layout(
                commands::request(socket, ClientMessage::FocusPaneId { id: pane }).await?,
            )?;
            Ok(())
        }
        cli::PaneCommand::Zoom { pane } => focus_then(socket, pane, ClientMessage::ZoomPane).await,
        cli::PaneCommand::Swap { pane, direction } => {
            focus_then(
                socket,
                pane,
                ClientMessage::SwapPane {
                    direction: Direction::from(direction),
                },
            )
            .await
        }
        cli::PaneCommand::Resize {
            pane,
            direction,
            cells,
        } => {
            focus_then(
                socket,
                pane,
                ClientMessage::ResizePane {
                    direction: Direction::from(direction),
                    cells,
                },
            )
            .await
        }
        cli::PaneCommand::Move { pane, tab } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            let tab = commands::resolve_tab_anywhere(&layout, &tab)?;
            commands::layout(
                commands::request(socket, ClientMessage::MovePaneToTab { pane, tab }).await?,
            )?;
            Ok(())
        }
        cli::PaneCommand::WaitOutput {
            pane,
            text,
            timeout,
        } => {
            let reached = commands::poll_pane(socket, pane, timeout, |snapshot| {
                snapshot.screen.contents.contains(&text)
            })
            .await?;
            if !reached {
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

/// Focus a pane, then run a message the daemon applies to the focused pane.
async fn focus_then(
    socket: &Path,
    pane: kodade_cli_proto::PaneId,
    message: ClientMessage,
) -> Result<()> {
    commands::layout(commands::request(socket, ClientMessage::FocusPaneId { id: pane }).await?)?;
    commands::layout(commands::request(socket, message).await?)?;
    Ok(())
}

/// `tab` subcommands; TAB is a name or an id in the active workspace.
async fn tab(socket: &Path, command: cli::TabCommand) -> Result<()> {
    match command {
        cli::TabCommand::Ls { json } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&layout.tabs)?);
            } else {
                println!("{}", commands::format_tabs(&layout.tabs));
            }
            Ok(())
        }
        cli::TabCommand::New { workspace, name } => {
            let (ws, _) = resolve_target(socket, workspace, None).await?;
            let reply = commands::layout(
                commands::request(
                    socket,
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
        cli::TabCommand::Close { tab } => {
            let id = resolve_tab_name(socket, &tab).await?;
            commands::layout(commands::request(socket, ClientMessage::CloseTab { id }).await?)?;
            Ok(())
        }
        cli::TabCommand::Rename { tab, name } => {
            let id = resolve_tab_name(socket, &tab).await?;
            commands::layout(
                commands::request(socket, ClientMessage::RenameTabId { id, name }).await?,
            )?;
            Ok(())
        }
        cli::TabCommand::Select { tab } => {
            let id = resolve_tab_name(socket, &tab).await?;
            commands::layout(commands::request(socket, ClientMessage::SelectTab { id }).await?)?;
            Ok(())
        }
    }
}

/// `workspace` subcommands; WS is a name or an id.
async fn workspace(socket: &Path, command: cli::WorkspaceCommand) -> Result<()> {
    match command {
        cli::WorkspaceCommand::Ls { json } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&layout.workspaces)?);
            } else {
                println!("{}", commands::format_workspaces(&layout.workspaces));
            }
            Ok(())
        }
        // Same idempotent create-or-select as the top-level `new` alias.
        cli::WorkspaceCommand::New { name, path } => new_workspace(socket, name, path).await,
        cli::WorkspaceCommand::Close { workspace } => {
            let id = resolve_workspace_name(socket, &workspace).await?;
            commands::layout(
                commands::request(socket, ClientMessage::CloseWorkspace { id }).await?,
            )?;
            Ok(())
        }
        cli::WorkspaceCommand::Rename { workspace, name } => {
            let id = resolve_workspace_name(socket, &workspace).await?;
            commands::layout(
                commands::request(socket, ClientMessage::RenameWorkspaceId { id, name }).await?,
            )?;
            Ok(())
        }
        cli::WorkspaceCommand::Color { workspace, color } => {
            let id = resolve_workspace_name(socket, &workspace).await?;
            // `off` clears the override; the daemon validates the hex form.
            let color = (color != "off").then_some(color);
            commands::layout(
                commands::request(socket, ClientMessage::SetWorkspaceColor { id, color }).await?,
            )?;
            Ok(())
        }
        cli::WorkspaceCommand::Select { workspace } => {
            let id = resolve_workspace_name(socket, &workspace).await?;
            commands::layout(
                commands::request(socket, ClientMessage::SelectWorkspace { id }).await?,
            )?;
            Ok(())
        }
    }
}

/// `session` subcommands. Locally `ls` probes every socket in the runtime
/// directory; with `--remote` every verb runs on the host over SSH (#23).
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
        cli::SessionCommand::Ls { json } => {
            let entries = commands::session_entries().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if !entries.is_empty() {
                println!("{}", commands::format_sessions(&entries));
            }
            Ok(())
        }
        cli::SessionCommand::Kill { name } => {
            let target =
                kodade_cli_daemon::socket_path(&name.unwrap_or_else(|| session.to_owned()));
            match commands::request(&target, ClientMessage::KillSession).await? {
                ServerMessage::Shutdown => Ok(()),
                message => commands::layout(message).map(|_| ()),
            }
        }
        cli::SessionCommand::Rename { name } => {
            let socket = kodade_cli_daemon::socket_path(session);
            commands::layout(
                commands::request(&socket, ClientMessage::RenameSession { name }).await?,
            )?;
            Ok(())
        }
    }
}

/// `layout export|apply` over the persistence JSON.
async fn layout_command(socket: &Path, command: cli::LayoutCommand) -> Result<()> {
    match command {
        cli::LayoutCommand::Export { file } => {
            let exported = commands::session_file(
                commands::request(socket, ClientMessage::Query(QueryKind::Session)).await?,
            )?;
            let json = format!("{}\n", serde_json::to_string_pretty(&exported)?);
            match file {
                Some(path) => std::fs::write(&path, json)
                    .with_context(|| format!("write {}", path.display()))?,
                None => print!("{json}"),
            }
            Ok(())
        }
        cli::LayoutCommand::Apply { file } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            let parsed = serde_json::from_str(&text).context("parse the layout file")?;
            commands::layout(commands::request(socket, ClientMessage::ApplyLayout(parsed)).await?)?;
            Ok(())
        }
    }
}

/// Create a workspace, or select it when the name already exists.
async fn new_workspace(
    socket: &Path,
    name: String,
    path: Option<std::path::PathBuf>,
) -> Result<()> {
    let layout = commands::layout(commands::request(socket, commands::layout_query()).await?)?;
    if let Ok(id) = commands::resolve_workspace(&layout, &name) {
        commands::request(socket, ClientMessage::SelectWorkspace { id }).await?;
        println!("{}", id.0);
    } else {
        let reply = commands::layout(
            commands::request(socket, ClientMessage::NewWorkspace { name, root: path }).await?,
        )?;
        println!("{}", reply.active_workspace.0);
    }
    Ok(())
}

/// Resolve a tab name or id against a fresh snapshot.
async fn resolve_tab_name(socket: &Path, needle: &str) -> Result<kodade_cli_proto::TabId> {
    let layout = commands::layout(commands::request(socket, commands::layout_query()).await?)?;
    commands::resolve_tab(&layout, None, needle)
}

/// Resolve a workspace name or id against a fresh snapshot.
async fn resolve_workspace_name(
    socket: &Path,
    needle: &str,
) -> Result<kodade_cli_proto::WorkspaceId> {
    let layout = commands::layout(commands::request(socket, commands::layout_query()).await?)?;
    commands::resolve_workspace(&layout, needle)
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
        cli::AgentCommand::Wait {
            pane,
            state,
            timeout,
        } => {
            let reached =
                commands::poll_pane(socket, pane, timeout, |snapshot| snapshot.state == state)
                    .await?;
            if !reached {
                std::process::exit(2);
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

/// `worktree` subcommands: add, remove, and list git-worktree workspaces (#22).
async fn worktree(socket: &Path, command: cli::WorktreeCommand) -> Result<()> {
    match command {
        cli::WorktreeCommand::Add {
            branch,
            from,
            workspace,
        } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            // The repo to branch is the target workspace's root (default: active).
            let ws = match workspace.as_deref() {
                Some(name) => commands::resolve_workspace(&layout, name)?,
                None => layout.active_workspace,
            };
            let repo_root = layout
                .workspaces
                .iter()
                .find(|item| item.id == ws)
                .and_then(|item| item.root.clone())
                .ok_or_else(|| anyhow!("workspace has no root directory to branch from"))?;
            let reply = commands::layout(
                commands::request(
                    socket,
                    ClientMessage::NewWorktreeWorkspace {
                        repo_root,
                        branch,
                        from,
                    },
                )
                .await?,
            )?;
            println!("{}", reply.active_workspace.0);
            Ok(())
        }
        cli::WorktreeCommand::Remove { target, keep } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            let id = commands::resolve_worktree(&layout, &target)?;
            commands::request(socket, ClientMessage::RemoveWorktreeWorkspace { id, keep }).await?;
            println!("{}", id.0);
            Ok(())
        }
        cli::WorktreeCommand::List { json } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&commands::worktree_workspaces(&layout))?
                );
            } else {
                println!("{}", commands::format_worktrees(&layout));
            }
            Ok(())
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
    // Subscribe so the TUI learns about session-level changes (a rename moves
    // the socket under it). Subscribed connections receive notifications as
    // `Event::Notification` instead of `ServerMessage::Notification`.
    writer
        .write_all(&encode(&ClientMessage::Subscribe)?)
        .await?;
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let update = match decode(line.as_bytes()) {
                Ok(ServerMessage::Layout(layout)) => app::Update::Layout(layout),
                Ok(ServerMessage::Welcome { session, .. }) => app::Update::Session(session),
                Ok(ServerMessage::Notification(notification)) => {
                    app::Update::Notification(notification)
                }
                Ok(ServerMessage::Event(Event::Notification(notification))) => {
                    app::Update::Notification(notification)
                }
                Ok(ServerMessage::Event(Event::SessionRenamed { name, socket })) => {
                    app::Update::SessionRenamed { name, socket }
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
