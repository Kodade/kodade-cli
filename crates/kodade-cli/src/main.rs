mod commands;
mod config;
mod input;
mod mode;
mod render;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kodade_cli_proto::{decode, encode, ClientMessage, Direction, LayoutSnapshot, ServerMessage};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{env, io::Write, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};
const SCROLL_STEP: i16 = 3;

struct DragState {
    direction: Direction,
    vertical: bool,
    last: u16,
}
#[tokio::main]
async fn main() -> Result<()> {
    let config = config::Config::load();
    let args: Vec<_> = env::args().skip(1).collect();
    match commands::parse(&args)? {
        commands::Command::Attach { session } => attach(&session, &config).await,
        commands::Command::Daemon { session } => kodade_cli_daemon::run(session).await,
        commands::Command::Ls {
            session,
            agents_only,
        } => {
            let layout =
                commands::layout(commands::request(&session, commands::layout_query()).await?)?;
            println!(
                "{}",
                if agents_only {
                    commands::format_agents(&layout)
                } else {
                    commands::format_ls(&layout)
                }
            );
            Ok(())
        }
        commands::Command::AgentAttach { session, pane } => {
            commands::layout(
                commands::request(&session, ClientMessage::FocusPaneId { id: pane }).await?,
            )?;
            attach(&session, &config).await
        }
        commands::Command::Rename {
            session,
            pane,
            name,
        } => {
            commands::layout(
                commands::request(&session, ClientMessage::RenamePaneId { id: pane, name }).await?,
            )?;
            Ok(())
        }
        commands::Command::Explain { session, pane } => {
            let layout =
                commands::layout(commands::request(&session, commands::layout_query()).await?)?;
            let pane = commands::find_pane(&layout, pane)?;
            println!(
                "{}  {}",
                commands::state_name(pane.state),
                pane.state_reason
            );
            Ok(())
        }
        commands::Command::Report {
            session,
            pane,
            state,
            source,
        } => {
            commands::layout(
                commands::request(
                    &session,
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
        commands::Command::Send {
            session,
            pane,
            bytes,
        } => {
            commands::layout(
                commands::request(&session, ClientMessage::SendToPane { id: pane, bytes }).await?,
            )?;
            Ok(())
        }
        commands::Command::KillSession { session } => {
            match commands::request(&session, ClientMessage::KillSession).await? {
                ServerMessage::Shutdown => Ok(()),
                message => commands::layout(message).map(|_| ()),
            }
        }
        commands::Command::Integrate { write } => commands::integrate_claude_code(write),
    }
}
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
    tui(stream, config).await
}
async fn tui(stream: UnixStream, config: &config::Config) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(ServerMessage::Layout(layout)) = decode(line.as_bytes()) {
                if tx.send(layout).await.is_err() {
                    break;
                }
            }
        }
    });
    let (cols, rows) = crossterm::terminal::size()?;
    writer
        .write_all(&encode(&ClientMessage::Hello {
            cols: pane_cols(cols, config.sidebar),
            rows,
        })?)
        .await?;
    let theme = config.resolve_theme();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    if config.mouse {
        execute!(stdout, EnableMouseCapture)?;
    }
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = loop_tui(&mut term, &mut writer, &mut rx, config, &theme).await;
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    if config.mouse {
        execute!(term.backend_mut(), DisableMouseCapture)?;
    }
    term.show_cursor()?;
    result
}
async fn loop_tui(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    rx: &mut mpsc::Receiver<LayoutSnapshot>,
    config: &config::Config,
    theme: &config::Theme,
) -> Result<()> {
    let mut layout = None;
    let mut prefix = false;
    let mut rename = false;
    let mut name = String::new();
    let mut rename_target = None;
    let mut drag = None;
    let mut sidebar = config.sidebar;
    let mut navigate = None;
    let mut copy: Option<mode::CopyMode> = None;
    let mut menu: Option<mode::Menu> = None;
    let mut note: Option<String> = None;
    loop {
        while let Ok(next) = rx.try_recv() {
            if let Some(copy_mode) = &mut copy {
                if let Some(pane) = next.panes.iter().find(|pane| pane.id == copy_mode.pane) {
                    copy_mode.refresh(pane.screen.clone());
                }
            }
            layout = Some(next);
        }
        term.draw(|f| {
            if let Some(layout) = &layout {
                render::render(
                    f,
                    layout,
                    &render::Ui {
                        sidebar,
                        prefix,
                        rename,
                        name: &name,
                        navigate,
                        copy: copy.as_ref(),
                        menu: menu.as_ref(),
                        note: note.as_deref(),
                    },
                    theme,
                )
            }
        })?;
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        match event::read()? {
            Event::Resize(cols, rows) => {
                writer
                    .write_all(&encode(&ClientMessage::Resize {
                        cols: pane_cols(cols, sidebar),
                        rows,
                    })?)
                    .await?
            }
            Event::Key(k) if rename => match k.code {
                KeyCode::Enter => {
                    let name = std::mem::take(&mut name);
                    let message = match rename_target.take() {
                        Some(mode::MenuTarget::Pane(id)) => {
                            ClientMessage::RenamePaneId { id, name }
                        }
                        Some(mode::MenuTarget::Tab(id)) => {
                            writer
                                .write_all(&encode(&ClientMessage::SelectTab { id })?)
                                .await?;
                            ClientMessage::RenameTab { name }
                        }
                        Some(mode::MenuTarget::Workspace(id)) => {
                            writer
                                .write_all(&encode(&ClientMessage::SelectWorkspace { id })?)
                                .await?;
                            ClientMessage::RenameWorkspace { name }
                        }
                        None => ClientMessage::RenamePane { name },
                    };
                    writer.write_all(&encode(&message)?).await?;
                    rename = false
                }
                KeyCode::Esc => {
                    name.clear();
                    rename = false
                }
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) => name.push(c),
                _ => {}
            },
            Event::Key(k) if let Some(copy_mode) = &mut copy => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => copy = None,
                KeyCode::Char('v') => copy_mode.anchor = Some(copy_mode.cursor),
                KeyCode::Char('y') => {
                    if let Some(anchor) = copy_mode.anchor {
                        let text = mode::selected_text(
                            &copy_mode.screen.contents,
                            anchor,
                            copy_mode.cursor,
                        );
                        let (payload, truncated) = mode::osc52(&text);
                        execute!(term.backend_mut(), crossterm::style::Print(payload))?;
                        term.backend_mut().flush()?;
                        note = Some(if truncated {
                            " copied (truncated to 100KB)".into()
                        } else {
                            " copied".into()
                        });
                        copy = None;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => copy_mode.move_by(-1, 0),
                KeyCode::Down | KeyCode::Char('j') => copy_mode.move_by(1, 0),
                KeyCode::Left | KeyCode::Char('h') => copy_mode.move_by(0, -1),
                KeyCode::Right | KeyCode::Char('l') => copy_mode.move_by(0, 1),
                KeyCode::PageUp | KeyCode::Char('u')
                    if k.modifiers.contains(KeyModifiers::CONTROL)
                        || matches!(k.code, KeyCode::PageUp) =>
                {
                    writer
                        .write_all(&encode(&ClientMessage::ScrollPane {
                            id: copy_mode.pane,
                            delta: 20,
                        })?)
                        .await?
                }
                KeyCode::PageDown | KeyCode::Char('d')
                    if k.modifiers.contains(KeyModifiers::CONTROL)
                        || matches!(k.code, KeyCode::PageDown) =>
                {
                    writer
                        .write_all(&encode(&ClientMessage::ScrollPane {
                            id: copy_mode.pane,
                            delta: -20,
                        })?)
                        .await?
                }
                _ => {}
            },
            Event::Key(k) if let Some(menu_state) = &mut menu => match k.code {
                KeyCode::Esc => menu = None,
                KeyCode::Up | KeyCode::Char('k') => menu_state.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => menu_state.move_by(1),
                KeyCode::Enter => {
                    execute_menu(writer, &mut menu, &mut rename, &mut rename_target).await?;
                }
                _ => {}
            },
            Event::Key(k) if let Some(current) = navigate => {
                let rows = render::sidebar_rows(layout.as_ref().expect("navigate has layout"));
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        navigate = None;
                        if !config.sidebar {
                            sidebar = false;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        navigate = mode::navigate(
                            &rows
                                .iter()
                                .map(|row| row.target.clone())
                                .collect::<Vec<_>>(),
                            Some(current),
                            -1,
                        )
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        navigate = mode::navigate(
                            &rows
                                .iter()
                                .map(|row| row.target.clone())
                                .collect::<Vec<_>>(),
                            Some(current),
                            1,
                        )
                    }
                    KeyCode::Enter => {
                        activate_sidebar(writer, rows[current].target.clone()).await?;
                        navigate = None;
                    }
                    _ => {}
                }
            }
            Event::Key(k) if prefix => {
                prefix = false;
                if k == config.prefix {
                    writer
                        .write_all(&encode(&ClientMessage::Input {
                            bytes: bytes(k).unwrap_or_default(),
                        })?)
                        .await?
                } else if let Some(action) = config.action(k) {
                    if matches!(action, config::Action::SidebarToggle) {
                        sidebar = !sidebar;
                        let size = term.size()?;
                        writer
                            .write_all(&encode(&ClientMessage::Resize {
                                cols: pane_cols(size.width, sidebar),
                                rows: size.height,
                            })?)
                            .await?;
                    } else if matches!(action, config::Action::Navigate) {
                        sidebar = true;
                        navigate = Some(0);
                    } else if matches!(action, config::Action::CopyMode) {
                        if let Some(current) = &layout {
                            if let Some(pane) = current.panes.iter().find(|pane| pane.focused) {
                                copy = Some(mode::CopyMode::new(pane.id, pane.screen.clone()));
                            }
                        }
                    } else if matches!(action, config::Action::Rename) {
                        rename = true;
                    } else if matches!(action, config::Action::Detach) {
                        return Ok(());
                    } else if matches!(action, config::Action::WorkspaceNext) {
                        if let Some(current) = &layout {
                            let index = current
                                .workspaces
                                .iter()
                                .position(|workspace| workspace.active)
                                .unwrap_or(0);
                            let next = (index + 1) % current.workspaces.len();
                            writer
                                .write_all(&encode(&ClientMessage::SelectWorkspace {
                                    id: current.workspaces[next].id,
                                })?)
                                .await?
                        }
                    } else if let Some(message) = action.message() {
                        writer.write_all(&encode(&message)?).await?
                    }
                }
            }
            Event::Key(k) if k == config.prefix => prefix = true,
            Event::Key(k) => {
                if let Some(bytes) = bytes(k) {
                    writer
                        .write_all(&encode(&ClientMessage::Input { bytes })?)
                        .await?
                }
            }
            Event::Mouse(mouse) if config.mouse => {
                let Some(current) = &layout else { continue };
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(menu_state) = &mut menu {
                            if let Some(selected) =
                                mode::menu_hit(menu_state, mouse.column, mouse.row)
                            {
                                menu_state.selected = selected;
                                execute_menu(writer, &mut menu, &mut rename, &mut rename_target)
                                    .await?;
                            } else {
                                menu = None;
                            }
                            continue;
                        }
                        let size = term.size()?;
                        let frame_area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                        let content_area = render::content_area(frame_area, sidebar);
                        if mouse.column < content_area.x {
                            if sidebar {
                                if let Some(row) = render::sidebar_row_at(
                                    &render::sidebar_rows(current),
                                    mouse.row,
                                ) {
                                    let message = match row.target {
                                        render::SidebarTarget::Workspace(id) => {
                                            ClientMessage::SelectWorkspace { id }
                                        }
                                        render::SidebarTarget::Tab(id) => {
                                            ClientMessage::SelectTab { id }
                                        }
                                        render::SidebarTarget::Pane(id) => {
                                            ClientMessage::FocusPaneId { id }
                                        }
                                    };
                                    writer.write_all(&encode(&message)?).await?;
                                }
                            } else {
                                sidebar = true;
                                writer
                                    .write_all(&encode(&ClientMessage::Resize {
                                        cols: pane_cols(size.width, sidebar),
                                        rows: size.height,
                                    })?)
                                    .await?;
                            }
                            continue;
                        }
                        let rects = render::pane_rects_for(current, content_area);
                        if let Some(border) = input::border_at(&rects, mouse.column, mouse.row) {
                            writer
                                .write_all(&encode(&ClientMessage::FocusPaneId {
                                    id: border.pane,
                                })?)
                                .await?;
                            drag = Some(DragState {
                                direction: border.direction,
                                vertical: border.vertical,
                                last: if border.vertical {
                                    mouse.column
                                } else {
                                    mouse.row
                                },
                            });
                        } else if mouse.row == 0 {
                            if let Some(id) = input::tab_at(
                                &render::tab_spans_for(current, content_area.x),
                                mouse.column,
                            ) {
                                writer
                                    .write_all(&encode(&ClientMessage::SelectTab { id })?)
                                    .await?;
                            }
                        } else if let Some(id) = input::pane_at(&rects, mouse.column, mouse.row) {
                            writer
                                .write_all(&encode(&ClientMessage::FocusPaneId { id })?)
                                .await?;
                        }
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        let size = term.size()?;
                        let content = render::content_area(
                            ratatui::layout::Rect::new(0, 0, size.width, size.height),
                            sidebar,
                        );
                        let target = if sidebar && mouse.column < content.x {
                            render::sidebar_row_at(&render::sidebar_rows(current), mouse.row).map(
                                |row| match row.target {
                                    render::SidebarTarget::Workspace(id) => {
                                        mode::MenuTarget::Workspace(id)
                                    }
                                    render::SidebarTarget::Tab(id) => mode::MenuTarget::Tab(id),
                                    render::SidebarTarget::Pane(id) => mode::MenuTarget::Pane(id),
                                },
                            )
                        } else {
                            input::pane_at(
                                &render::pane_rects_for(current, content),
                                mouse.column,
                                mouse.row,
                            )
                            .map(mode::MenuTarget::Pane)
                        };
                        if let Some(target) = target {
                            menu = Some(mode::Menu {
                                target,
                                x: mouse.column,
                                y: mouse.row,
                                selected: 0,
                            });
                        } else {
                            menu = None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(dragging) = &mut drag {
                            let now = if dragging.vertical {
                                mouse.column
                            } else {
                                mouse.row
                            };
                            let cells = input::drag_delta(dragging.last, now);
                            if cells != 0 {
                                writer
                                    .write_all(&encode(&ClientMessage::ResizePane {
                                        direction: dragging.direction,
                                        cells,
                                    })?)
                                    .await?;
                                dragging.last = now;
                            }
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => drag = None,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let rects = render::pane_rects_for(current, {
                            let size = term.size()?;
                            render::content_area(
                                ratatui::layout::Rect::new(0, 0, size.width, size.height),
                                sidebar,
                            )
                        });
                        if let Some(id) = input::pane_at(&rects, mouse.column, mouse.row) {
                            let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                                SCROLL_STEP
                            } else {
                                -SCROLL_STEP
                            };
                            writer
                                .write_all(&encode(&ClientMessage::ScrollPane { id, delta })?)
                                .await?;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

async fn activate_sidebar(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    target: render::SidebarTarget,
) -> Result<()> {
    let message = match target {
        render::SidebarTarget::Workspace(id) => ClientMessage::SelectWorkspace { id },
        render::SidebarTarget::Tab(id) => ClientMessage::SelectTab { id },
        render::SidebarTarget::Pane(id) => ClientMessage::FocusPaneId { id },
    };
    writer.write_all(&encode(&message)?).await?;
    Ok(())
}
async fn execute_menu(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    menu: &mut Option<mode::Menu>,
    rename: &mut bool,
    rename_target: &mut Option<mode::MenuTarget>,
) -> Result<()> {
    let menu_state = menu.take().expect("menu exists");
    if let mode::MenuTarget::Pane(id) = menu_state.target {
        writer
            .write_all(&encode(&ClientMessage::FocusPaneId { id })?)
            .await?;
    }
    match menu_state.action() {
        mode::MenuAction::Rename => {
            *rename = true;
            *rename_target = Some(menu_state.target);
        }
        mode::MenuAction::SplitRight => {
            writer
                .write_all(&encode(&ClientMessage::SplitRight)?)
                .await?
        }
        mode::MenuAction::SplitDown => {
            writer
                .write_all(&encode(&ClientMessage::SplitDown)?)
                .await?
        }
        mode::MenuAction::Zoom => writer.write_all(&encode(&ClientMessage::ZoomPane)?).await?,
        mode::MenuAction::Close => {
            let message = match menu_state.target {
                mode::MenuTarget::Pane(_) => ClientMessage::ClosePane,
                mode::MenuTarget::Tab(id) => ClientMessage::CloseTab { id },
                mode::MenuTarget::Workspace(id) => ClientMessage::CloseWorkspace { id },
            };
            writer.write_all(&encode(&message)?).await?;
        }
    };
    Ok(())
}

fn pane_cols(cols: u16, sidebar: bool) -> u16 {
    cols.saturating_sub(render::sidebar_width(sidebar)).max(1)
}
fn bytes(k: KeyEvent) -> Option<Vec<u8>> {
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut b = match k.code {
        KeyCode::Char(c) if k.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![(c.to_ascii_lowercase() as u8) & 0x1f]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![27],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            format!("\x1b[{code}~").into_bytes()
        }
        _ => return None,
    };
    if alt {
        b.insert(0, 27)
    }
    Some(b)
}
