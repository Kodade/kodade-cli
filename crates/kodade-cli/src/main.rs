mod app;
mod atomic_file;
mod attention;
mod automation;
mod cli;
mod commands;
mod config;
mod connection;
mod doctor;
mod endpoints;
mod graphics;
mod help;
mod image_paste;
mod input;
mod integrations;
mod keys;
mod machines;
mod mode;
mod notify;
mod overlay;
mod palette;
mod paste;
mod picker;
mod plugins;
mod remote;
mod render;
mod selection;
mod settings;
mod state;
mod terminal;
mod update;

use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, FromArgMatches};
use kodade_cli_proto::{
    decode, encode, ClientMessage, Direction, Event, QueryKind, ServerMessage, SplitAxis,
    PROTOCOL_VERSION,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

#[tokio::main]
async fn main() -> Result<()> {
    let matches = cli::Cli::command().get_matches();
    let explicit_session =
        matches.value_source("session") == Some(clap::parser::ValueSource::CommandLine);
    let mut args = cli::Cli::from_arg_matches(&matches)?;
    connection::inherited_context(
        &mut args,
        explicit_session,
        std::env::var("KODADE_SESSION").ok(),
        std::env::var_os("KODADE_SOCKET").map(Into::into),
    )?;
    let session = args.session.clone();
    let remote = args.remote.clone();
    if remote.is_some() && matches!(args.command, Some(cli::Command::Update { .. })) {
        bail!("update is local-only; run kodade-cli update on the remote host directly");
    }

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
                | cli::Command::Plugin {
                    command: cli::PluginCommand::Run { .. } | cli::PluginCommand::Pane { .. }
                }
                | cli::Command::KillSession
        )
    ) && !matches!(
        args.command,
        Some(cli::Command::Agent {
            command: cli::AgentCommand::UpdateManifests
                | cli::AgentCommand::ValidateManifest { .. }
        })
    );
    if remote.is_some()
        && matches!(
            args.command,
            Some(cli::Command::Agent {
                command: cli::AgentCommand::UpdateManifests
            })
        )
    {
        bail!("agent update-manifests is local-only; run it on the remote host directly");
    }
    // `--remote` sets up the SSH forward once; `_tunnel` must outlive every
    // request below so the forward stays open (dropping it removes the socket).
    let (socket, _tunnel) = if needs_socket {
        remote::resolve_socket(&args).await?
    } else {
        (
            args.socket
                .clone()
                .unwrap_or_else(|| kodade_cli_daemon::socket_path(&session)),
            None,
        )
    };
    // Only creation operations start a missing local session. Read-only and
    // destructive commands never create a new session as a side effect.
    let creates = matches!(
        args.command,
        Some(
            cli::Command::Agent {
                command: cli::AgentCommand::Start { .. }
            } | cli::Command::New { .. }
                | cli::Command::Run { .. }
                | cli::Command::NewTab { .. }
                | cli::Command::Workspace {
                    command: cli::WorkspaceCommand::New { .. }
                }
                | cli::Command::Tab {
                    command: cli::TabCommand::New { .. }
                }
        )
    );
    if creates && args.remote.is_none() && args.socket.is_none() {
        drop(connection::connect(&socket, &session, true).await?);
    }
    let command = args.command;

    // The config is only loaded where it is used, so `config validate` does not
    // print its warnings twice.
    match command {
        Some(cli::Command::Update {
            check,
            channel,
            show_channel,
            install_to,
        }) => {
            if let Some(channel) = channel.as_deref() {
                update::save_channel(channel)?;
            }
            let channel = channel.unwrap_or(update::saved_channel()?);
            if show_channel {
                println!("{channel}");
                return Ok(());
            }
            let metadata = String::from_utf8(update::fetch(update::metadata_url(&channel))?)?;
            let release = update::select_release(&channel, &metadata)?;
            if check && install_to.is_none() {
                println!(
                    "channel: {channel}\ninstalled: {}\navailable: {}",
                    env!("CARGO_PKG_VERSION"),
                    release.tag_name
                );
                Ok(())
            } else if install_to.is_none()
                && !update::release_is_newer(&release.tag_name, env!("CARGO_PKG_VERSION"))
            {
                println!(
                    "Ködade CLI {} is already current on the {channel} channel.",
                    env!("CARGO_PKG_VERSION")
                );
                Ok(())
            } else {
                let explicit_destination = install_to.is_some();
                let destination = install_to.unwrap_or(
                    std::env::current_exe().context("find installed kodade-cli executable")?,
                );
                if !explicit_destination {
                    if let Some(command) = update::package_upgrade_command(&destination) {
                        println!("This Ködade installation is package-managed. Upgrade it with:\n  {command}");
                        return Ok(());
                    }
                }
                let version = release.tag_name.trim_start_matches('v');
                let asset = update::platform_asset(version)?;
                let sums = String::from_utf8(update::fetch(update::release_asset_url(
                    &release,
                    "SHA256SUMS",
                )?)?)?;
                let archive = update::fetch(update::release_asset_url(&release, &asset)?)?;
                update::install_archive(&archive, &update::checksum(&sums, &asset)?, &destination)?;
                println!("installed verified {version} to {}", destination.display());
                Ok(())
            }
        }
        // No subcommand attaches the TUI to the session.
        None => attach(&socket, &session, &config::Config::load(), remote.is_some()).await,
        Some(cli::Command::Doctor { json }) => {
            if let Some(host) = remote.as_deref() {
                remote::run_doctor(host, &session, json).await
            } else {
                doctor::run(&socket, &session, json).await
            }
        }
        Some(cli::Command::Plugin { command }) => {
            plugins::command(&socket, &session, remote.is_some(), command).await
        }
        Some(cli::Command::Daemon { session: name }) => {
            kodade_cli_daemon::run(name.unwrap_or(session)).await
        }
        Some(cli::Command::Session { command }) => {
            session_command(remote.as_deref(), &socket, &session, command).await
        }
        Some(cli::Command::Machine { command }) => machine(command).await,
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
            agent(
                &socket,
                &session,
                remote.as_deref(),
                &config::Config::load(),
                command,
            )
            .await
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
            new_workspace(&socket, workspace, path, Vec::new()).await
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
            cli::IntegrateCommand::List => integrations::integrate_list(),
            cli::IntegrateCommand::ClaudeCode { write, remove } => {
                if remove {
                    integrations::unintegrate_claude_code()
                } else {
                    integrations::integrate_claude_code(write)
                }
            }
            cli::IntegrateCommand::GeminiCli { write, remove } => {
                if remove {
                    integrations::unintegrate_gemini()
                } else {
                    integrations::integrate_gemini(write, false)
                }
            }
            cli::IntegrateCommand::Codex {
                write,
                force,
                remove,
            } => {
                if remove {
                    integrations::unintegrate_codex()
                } else {
                    integrations::integrate_codex(write, force)
                }
            }
            cli::IntegrateCommand::OpenCode { write, remove } => {
                if remove {
                    integrations::unintegrate_opencode()
                } else {
                    integrations::integrate_opencode(write)
                }
            }
            cli::IntegrateCommand::Pi { write, remove } => {
                if remove {
                    integrations::unintegrate_pi()
                } else {
                    integrations::integrate_pi(write)
                }
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

async fn machine(command: cli::MachineCommand) -> Result<()> {
    let mut catalog = machines::load()?;
    match command {
        cli::MachineCommand::List { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&catalog.machines)?);
            } else {
                for machine in &catalog.machines {
                    println!(
                        "{}\t{}\t{}\t{}",
                        machine.id,
                        if machine.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        machine.label,
                        machine.target
                    );
                }
            }
        }
        cli::MachineCommand::Add {
            target,
            label,
            session,
            install,
        } => {
            // Validate catalog constraints before an explicit preparation, but
            // don't save until the remote is known usable.
            let mut candidate = machines::Catalog {
                machines: catalog.machines.clone(),
            };
            candidate.add(label.clone(), target.clone(), session.clone())?;
            if install {
                remote::prepare_machine(&target, true).await?;
            }
            let profile = catalog.add(label, target, session)?;
            println!("{}", profile.id);
            machines::save(&catalog)?;
        }
        cli::MachineCommand::Prepare { id, install } => {
            let profile = catalog.get_mut(&id)?.clone();
            remote::prepare_machine(&profile.target, install).await?;
            println!("prepared {} ({})", profile.label, profile.target);
        }
        cli::MachineCommand::Rename { id, label } => {
            catalog.get_mut(&id)?.label = label;
            machines::save(&catalog)?;
        }
        cli::MachineCommand::Enable { id } => {
            catalog.get_mut(&id)?.enabled = true;
            machines::save(&catalog)?;
        }
        cli::MachineCommand::Disable { id } => {
            catalog.get_mut(&id)?.enabled = false;
            machines::save(&catalog)?;
        }
        cli::MachineCommand::Remove { id } => {
            catalog.remove(&id)?;
            machines::save(&catalog)?;
        }
    }
    Ok(())
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
        cli::PaneCommand::PasteImage { pane, path } => {
            let path = image_paste::paste(socket, pane, path.as_deref()).await?;
            println!("{}", path.display());
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
            regex,
            scrollback,
            timeout,
        } => {
            let matcher = commands::output_matcher(&text, regex)?;
            // `poll_pane` only exposes screen snapshots. Output waits need the
            // daemon's durable history when requested, so read through the
            // same pane-id seam instead.
            let reached = wait_output(socket, pane, &matcher, scrollback, timeout).await?;
            if !reached {
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

/// Wait for a literal or regex match in a pane's visible screen or durable
/// scrollback. The pane id is never re-resolved, so replacement fails safely.
async fn wait_output(
    socket: &Path,
    pane: kodade_cli_proto::PaneId,
    matcher: &commands::OutputMatcher,
    scrollback: bool,
    timeout: Option<u64>,
) -> Result<bool> {
    let deadline = timeout.map(|secs| std::time::Instant::now() + Duration::from_secs(secs));
    loop {
        if matcher.matches(&commands::read_pane(socket, pane, scrollback, None).await?) {
            return Ok(true);
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
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
        cli::WorkspaceCommand::New { name, path, env } => {
            new_workspace(socket, name, path, env).await
        }
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
    socket: &Path,
    session: &str,
    command: cli::SessionCommand,
) -> Result<()> {
    if let Some(host) = remote {
        return remote::run_session(host, session, &command).await;
    }
    match command {
        cli::SessionCommand::Path => {
            println!("{}", socket.display());
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
            let target = name
                .map(|name| kodade_cli_daemon::socket_path(&name))
                .unwrap_or_else(|| socket.to_path_buf());
            match commands::request(&target, ClientMessage::KillSession).await? {
                ServerMessage::Shutdown => Ok(()),
                message => commands::layout(message).map(|_| ()),
            }
        }
        cli::SessionCommand::Rename { name } => {
            commands::layout(
                commands::request(socket, ClientMessage::RenameSession { name }).await?,
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
    env: Vec<(String, String)>,
) -> Result<()> {
    let layout = commands::layout(commands::request(socket, commands::layout_query()).await?)?;
    if let Ok(id) = commands::resolve_workspace(&layout, &name) {
        if !env.is_empty() {
            bail!("workspace '{name}' already exists; --env only applies when creating a workspace")
        }
        commands::request(socket, ClientMessage::SelectWorkspace { id }).await?;
        println!("{}", id.0);
    } else {
        let reply = commands::layout(
            commands::request(
                socket,
                ClientMessage::NewWorkspace {
                    name,
                    root: path,
                    env: env.into_iter().collect(),
                },
            )
            .await?,
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
        cli::ConfigCommand::Init => {
            if let Err(error) = init_config() {
                eprintln!("kodade-cli: {error:#}");
                std::process::exit(1);
            }
        }
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
    remote: Option<&str>,
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
        cli::AgentCommand::Start {
            workspace,
            tab,
            name,
            json,
            command,
        } => {
            let (workspace, tab) = resolve_target(socket, workspace, tab).await?;
            let pane = automation::start(socket, workspace, tab, name, command).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pane)?);
            } else {
                println!("{}", pane.id.0);
            }
            Ok(())
        }
        cli::AgentCommand::Read {
            target,
            lines,
            scrollback,
            json,
        } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            let (pane, text) = automation::read(socket, &target, scrollback, lines).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "pane": pane, "text": text })
                    )?
                );
            } else {
                println!("{text}");
            }
            Ok(())
        }
        cli::AgentCommand::SendKeys {
            target,
            keys,
            literal,
        } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            automation::send_keys(socket, &target, &keys, literal).await?;
            Ok(())
        }
        cli::AgentCommand::Prompt {
            target,
            text,
            wait,
            until,
            timeout,
            json,
        } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            match automation::prompt(socket, &target, &text, wait, until, timeout).await? {
                automation::PromptOutcome::Sent(pane)
                | automation::PromptOutcome::Settled(pane) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&pane)?);
                    }
                    Ok(())
                }
                automation::PromptOutcome::TimedOut => {
                    std::process::exit(2);
                }
            }
        }
        cli::AgentCommand::Focus { target, json } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            let pane = automation::focus(socket, &target).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pane)?);
            }
            Ok(())
        }
        cli::AgentCommand::Attach { target } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            automation::focus(socket, &target).await?;
            attach(socket, session, config, remote.is_some()).await
        }
        cli::AgentCommand::Rename { target, name } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            let pane = automation::agent_target(socket, &target).await?;
            commands::layout(
                commands::request(socket, ClientMessage::RenamePaneId { id: pane.id, name })
                    .await?,
            )?;
            Ok(())
        }
        cli::AgentCommand::Explain { target, json } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            let pane = automation::agent_target(socket, &target).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pane)?);
            } else {
                println!("{}", commands::format_explain(&pane));
            }
            Ok(())
        }
        cli::AgentCommand::Wait {
            target,
            state,
            timeout,
        } => {
            let target = contextual_agent_target(&target, socket, session, remote)?;
            let pane = automation::agent_target(socket, &target).await?;
            let reached =
                commands::poll_pane(socket, pane.id, timeout, |snapshot| snapshot.state == state)
                    .await?;
            if !reached {
                std::process::exit(2);
            }
            Ok(())
        }
        cli::AgentCommand::UpdateManifests => {
            integrations::update_manifests()?;
            if socket.exists() {
                print_manifests(
                    commands::request(socket, ClientMessage::ReloadManifests).await?,
                    false,
                )
            } else {
                println!("updated manifests; reload them when the daemon is running");
                Ok(())
            }
        }
        cli::AgentCommand::Manifests { reload, json } => {
            let request = if reload {
                ClientMessage::ReloadManifests
            } else {
                ClientMessage::Query(QueryKind::Manifests)
            };
            print_manifests(commands::request(socket, request).await?, json)
        }
        cli::AgentCommand::ValidateManifest { path } => {
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let name = kodade_cli_daemon::validate_agent_manifest(&source)?;
            println!("valid manifest: {name}");
            Ok(())
        }
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

fn print_manifests(message: ServerMessage, json: bool) -> Result<()> {
    let ServerMessage::Manifests(manifests) = message else {
        return commands::layout(message).map(|_| ());
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&manifests)?);
    } else {
        for manifest in manifests {
            println!(
                "{:<16} {:<16} {:<13} {} rules",
                manifest.name, manifest.display, manifest.source, manifest.rules
            );
        }
    }
    Ok(())
}

/// `current` is only meaningful inside a pane Ködade spawned. Reading its
/// inherited identity prevents a shell command from accidentally targeting the
/// TUI's globally focused pane, and rejects a different session or remote.
fn contextual_agent_target(
    target: &str,
    socket: &Path,
    session: &str,
    remote: Option<&str>,
) -> Result<String> {
    if target != "current" {
        return Ok(target.into());
    }
    if remote.is_some() {
        bail!("`current` cannot be used with --remote; pass an explicit pane target")
    }
    let inherited_session = std::env::var("KODADE_SESSION")
        .map_err(|_| anyhow!("`current` requires KODADE_PANE inside a Ködade pane"))?;
    if inherited_session != session {
        bail!("`current` belongs to session '{inherited_session}', not '{session}'")
    }
    let inherited_socket = std::env::var_os("KODADE_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| kodade_cli_daemon::socket_path(&inherited_session));
    if inherited_socket != socket {
        bail!("`current` cannot target a different socket; pass an explicit pane target")
    }
    let pane = std::env::var("KODADE_PANE")
        .map_err(|_| anyhow!("`current` requires KODADE_PANE inside a Ködade pane"))?;
    pane.parse::<u64>()
        .map_err(|_| anyhow!("KODADE_PANE is not a valid pane id"))?;
    Ok(pane)
}

/// `worktree` subcommands: add, remove, and list git-worktree workspaces (#22).
async fn worktree(socket: &Path, command: cli::WorktreeCommand) -> Result<()> {
    match command {
        cli::WorktreeCommand::Add {
            branch,
            from,
            base,
            path,
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
            let path = path.map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    repo_root.join(path)
                }
            });
            let reply = commands::layout(
                commands::request(
                    socket,
                    ClientMessage::NewWorktreeWorkspace {
                        repo_root,
                        branch,
                        from: base.or(from),
                        path,
                    },
                )
                .await?,
            )?;
            println!("{}", reply.active_workspace.0);
            Ok(())
        }
        cli::WorktreeCommand::Open { path, workspace } => {
            let layout =
                commands::layout(commands::request(socket, commands::layout_query()).await?)?;
            let ws = workspace
                .as_deref()
                .map(|name| commands::resolve_workspace(&layout, name))
                .transpose()?
                .unwrap_or(layout.active_workspace);
            let repo_root = layout
                .workspaces
                .iter()
                .find(|item| item.id == ws)
                .and_then(|item| item.root.clone())
                .ok_or_else(|| {
                    anyhow!("workspace has no root directory to open a worktree from")
                })?;
            let path = if path.is_absolute() {
                path
            } else {
                repo_root.join(path)
            };
            let reply = commands::layout(
                commands::request(
                    socket,
                    ClientMessage::OpenWorktreeWorkspace { repo_root, path },
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
async fn attach(socket: &Path, session: &str, config: &config::Config, remote: bool) -> Result<()> {
    // Only spawn a daemon for this host's own socket; a `--remote` tunnel socket
    // differs from the local path and must not trigger a local daemon.
    let can_spawn = socket == kodade_cli_daemon::socket_path(session).as_path();
    let stream = connection::connect(socket, session, can_spawn).await?;
    let profiles = if !remote {
        machines::load()?.machines
    } else {
        Vec::new()
    };
    tui(stream, config, session, socket, remote, profiles).await
}

/// Sets up the terminal, hands the socket to `App`, and always restores it.
async fn tui(
    stream: UnixStream,
    config: &config::Config,
    session: &str,
    socket: &Path,
    remote: bool,
    profiles: Vec<machines::MachineProfile>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut state = app::App::new(config, session, socket.to_path_buf());
    state.set_remote_endpoint(remote);
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
    tokio::time::timeout(Duration::from_secs(10), handshake(&mut lines, &mut state))
        .await
        .context("daemon handshake timed out after 10s")??;
    writer
        .write_all(&encode(&ClientMessage::SetCompactView {
            enabled: state.compact_enabled(cols),
        })?)
        .await?;
    // Subscribe so the TUI learns about session-level changes (a rename moves
    // the socket under it). Subscribed connections receive notifications as
    // `Event::Notification` instead of `ServerMessage::Notification`.
    writer
        .write_all(&encode(&ClientMessage::Subscribe)?)
        .await?;
    let (tx, mut rx) = mpsc::channel(64);
    let (command_tx, mut command_rx) = mpsc::channel(64);
    let mut router = endpoints::Router::new(endpoints::EndpointId::Local);
    router.register(endpoints::EndpointId::Local, command_tx);
    router.mark_online(endpoints::EndpointId::Local);
    state.configure_machines(&profiles);
    for profile in profiles.into_iter().filter(|profile| profile.enabled) {
        let (machine_tx, machine_rx) = mpsc::channel(64);
        let id = endpoints::EndpointId::Machine(profile.id.clone());
        router.register(id.clone(), machine_tx);
        endpoints::spawn_machine(
            profile,
            session.to_string(),
            state.pane_cols(cols),
            rows,
            router.updates(id.clone(), tx.clone()),
            machine_rx,
        );
    }
    let writer_updates = router.updates(endpoints::EndpointId::Local, tx.clone());
    tokio::spawn(async move {
        while let Some(message) = command_rx.recv().await {
            let Ok(encoded) = encode(&message) else {
                break;
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(5), writer.write_all(&encoded)).await,
                Ok(Ok(()))
            ) {
                let _ = writer_updates
                    .send(app::Update::EndpointFailed {
                        reason: "local endpoint disconnected".into(),
                    })
                    .await;
                break;
            }
        }
    });
    let reader_tx = router.updates(endpoints::EndpointId::Local, tx.clone());
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
                Ok(ServerMessage::Error { message }) => app::Update::RequestError(message),
                Ok(ServerMessage::Shutdown) => app::Update::EndpointFailed {
                    reason: "local endpoint shut down".into(),
                },
                _ => continue,
            };
            if reader_tx.send(update).await.is_err() {
                break;
            }
        }
        let _ = reader_tx
            .send(app::Update::EndpointFailed {
                reason: "local endpoint disconnected".into(),
            })
            .await;
    });
    let _modes = terminal::TerminalModes::enter(config.mouse)?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    state.run(&mut term, &mut router, &mut rx, &tx).await
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
                    bail!("protocol version mismatch: client {PROTOCOL_VERSION}, daemon {version} — upgrade kodade-cli on both ends");
                }
                state.handle_session(session);
                return Ok(());
            }
            Ok(ServerMessage::Error { message }) => {
                bail!("{message}");
            }
            // Ignore anything before the Welcome (there should be nothing).
            _ => continue,
        }
    }
}

/// Create an editable starter without overwriting an existing configuration.
fn init_config() -> Result<()> {
    use std::io::Write;
    let path = config::config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| {
            format!(
                "create {}; existing configuration is preserved",
                path.display()
            )
        })?;
    file.write_all(b"# K\xc3\xb6dade CLI configuration. Unspecified settings keep their defaults.\n# Run kodade-cli keys to inspect live bindings; prefix space opens the command center.\ntheme = \"auto\"\n\n[sidebar]\nwidth = 24\n\n[notify]\nonly_when_unfocused = true\n")?;
    println!("created {}", path.display());
    Ok(())
}
