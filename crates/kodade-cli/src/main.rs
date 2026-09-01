use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kodade_cli_proto::{
    decode, encode, ClientMessage, Direction, LayoutSnapshot, LayoutTree, PaneId, ServerMessage,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction as LDir, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use std::{collections::HashMap, env, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};
const DEFAULT_SESSION: &str = "default";
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.first().is_some_and(|x| x == "daemon") {
        return kodade_cli_daemon::run(
            args.get(1)
                .cloned()
                .unwrap_or_else(|| DEFAULT_SESSION.into()),
        )
        .await;
    }
    attach(match args.as_slice() {
        [] => DEFAULT_SESSION,
        [flag, name] if flag == "-s" || flag == "--session" => name,
        _ => bail!("usage: kodade-cli [-s SESSION]"),
    })
    .await
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
        .write_all(&encode(&ClientMessage::Hello { cols, rows })?)
        .await?;
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = loop_tui(&mut term, &mut writer, &mut rx).await;
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
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
    loop {
        while let Ok(next) = rx.try_recv() {
            layout = Some(next);
        }
        term.draw(|f| {
            if let Some(layout) = &layout {
                render(f, layout, prefix, rename, &name)
            }
        })?;
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        match event::read()? {
            Event::Resize(cols, rows) => {
                writer
                    .write_all(&encode(&ClientMessage::Resize { cols, rows })?)
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
            _ => {}
        }
    }
}
fn render(f: &mut ratatui::Frame, layout: &LayoutSnapshot, prefix: bool, rename: bool, name: &str) {
    let a = Layout::default()
        .direction(LDir::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    let ws = layout
        .workspaces
        .iter()
        .find(|x| x.active)
        .map(|x| x.name.as_str())
        .unwrap_or("workspace");
    let tabs = layout
        .tabs
        .iter()
        .map(|x| {
            if x.active {
                format!("[{}]", x.name)
            } else {
                format!(" {} ", x.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    f.render_widget(
        Paragraph::new(format!(" Ködade · {ws}  {tabs}")).style(Style::default().fg(Color::Cyan)),
        a[0],
    );
    let mut rects = HashMap::new();
    rects_for(&layout.tree, a[1], &mut rects);
    for pane in &layout.panes {
        if let Some(rect) = rects.get(&pane.id) {
            f.render_widget(
                Paragraph::new(pane.screen.contents.as_str()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(pane.title.as_str())
                        .border_style(Style::default().fg(if pane.focused {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        })),
                ),
                *rect,
            );
        }
    }
    let status = if rename {
        format!(" rename pane: {name}")
    } else if prefix {
        " prefix: % \" hjkl c n p w W x z d r".into()
    } else {
        format!(" session · {ws}")
    };
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        a[2],
    );
}
fn rects_for(tree: &LayoutTree, rect: Rect, out: &mut HashMap<PaneId, Rect>) {
    match tree {
        LayoutTree::Leaf { pane } => {
            out.insert(*pane, rect);
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let dir = match axis {
                kodade_cli_proto::SplitAxis::Horizontal => LDir::Horizontal,
                kodade_cli_proto::SplitAxis::Vertical => LDir::Vertical,
            };
            let n = if dir == LDir::Horizontal {
                rect.width
            } else {
                rect.height
            };
            let a = Layout::default()
                .direction(dir)
                .constraints([
                    Constraint::Length(((n as f32 * ratio) as u16).max(1)),
                    Constraint::Min(1),
                ])
                .split(rect);
            rects_for(first, a[0], out);
            rects_for(second, a[1], out);
        }
    }
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
