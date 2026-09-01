mod commands;
mod input;
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
use std::{env, process::Stdio, time::Duration};
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
    let args: Vec<_> = env::args().skip(1).collect();
    match commands::parse(&args)? {
        commands::Command::Attach { session } => attach(&session).await,
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
            attach(&session).await
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
    }
}
async fn attach(session: &str) -> Result<()> {
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
    tui(stream).await
}
async fn tui(stream: UnixStream) -> Result<()> {
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
            cols: pane_cols(cols, true),
            rows,
        })?)
        .await?;
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = loop_tui(&mut term, &mut writer, &mut rx).await;
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    term.show_cursor()?;
    result
}
async fn loop_tui(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    rx: &mut mpsc::Receiver<LayoutSnapshot>,
) -> Result<()> {
    let mut layout = None;
    let mut prefix = false;
    let mut rename = false;
    let mut name = String::new();
    let mut drag = None;
    let mut sidebar = true;
    loop {
        while let Ok(next) = rx.try_recv() {
            layout = Some(next);
        }
        term.draw(|f| {
            if let Some(layout) = &layout {
                render::render(f, layout, sidebar, prefix, rename, &name)
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
                    writer
                        .write_all(&encode(&ClientMessage::RenamePane {
                            name: std::mem::take(&mut name),
                        })?)
                        .await?;
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
            Event::Key(k) if prefix => {
                prefix = false;
                if k.code == KeyCode::Char('b') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    writer
                        .write_all(&encode(&ClientMessage::Input { bytes: vec![2] })?)
                        .await?
                } else if k.code == KeyCode::Char('b') {
                    sidebar = !sidebar;
                    let size = term.size()?;
                    writer
                        .write_all(&encode(&ClientMessage::Resize {
                            cols: pane_cols(size.width, sidebar),
                            rows: size.height,
                        })?)
                        .await?
                } else if k.code == KeyCode::Char('r') {
                    rename = true
                } else if k.code == KeyCode::Char('d') {
                    return Ok(());
                } else if k.code == KeyCode::Char('w') {
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
                } else if let Some(m) = command(k) {
                    writer.write_all(&encode(&m)?).await?
                }
            }
            Event::Key(k) if is_prefix(k) => prefix = true,
            Event::Key(k) => {
                if let Some(bytes) = bytes(k) {
                    writer
                        .write_all(&encode(&ClientMessage::Input { bytes })?)
                        .await?
                }
            }
            Event::Mouse(mouse) => {
                let Some(current) = &layout else { continue };
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
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

fn pane_cols(cols: u16, sidebar: bool) -> u16 {
    cols.saturating_sub(render::sidebar_width(sidebar)).max(1)
}
fn is_prefix(k: KeyEvent) -> bool {
    k.code == KeyCode::Char('b') && k.modifiers.contains(KeyModifiers::CONTROL)
}
fn command(k: KeyEvent) -> Option<ClientMessage> {
    Some(match k.code {
        KeyCode::Char('%') => ClientMessage::SplitRight,
        KeyCode::Char('\"') => ClientMessage::SplitDown,
        KeyCode::Char('c') => ClientMessage::NewTab,
        KeyCode::Char('n') => ClientMessage::NextTab,
        KeyCode::Char('p') => ClientMessage::PrevTab,
        KeyCode::Char('x') => ClientMessage::ClosePane,
        KeyCode::Char('z') => ClientMessage::ZoomPane,
        KeyCode::Char('H') => ClientMessage::ResizePane {
            direction: Direction::Left,
            cells: 2,
        },
        KeyCode::Char('J') => ClientMessage::ResizePane {
            direction: Direction::Down,
            cells: 2,
        },
        KeyCode::Char('K') => ClientMessage::ResizePane {
            direction: Direction::Up,
            cells: 2,
        },
        KeyCode::Char('L') => ClientMessage::ResizePane {
            direction: Direction::Right,
            cells: 2,
        },
        KeyCode::Char('W') => ClientMessage::NewWorkspace {
            name: "workspace".into(),
        },
        KeyCode::Up | KeyCode::Char('k') => ClientMessage::FocusPane {
            direction: Direction::Up,
        },
        KeyCode::Down | KeyCode::Char('j') => ClientMessage::FocusPane {
            direction: Direction::Down,
        },
        KeyCode::Left | KeyCode::Char('h') => ClientMessage::FocusPane {
            direction: Direction::Left,
        },
        KeyCode::Right | KeyCode::Char('l') => ClientMessage::FocusPane {
            direction: Direction::Right,
        },
        _ => return None,
    })
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
