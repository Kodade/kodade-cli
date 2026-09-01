use std::{env, process::Stdio, time::Duration};

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kodade_cli_proto::{decode, encode, ClientMessage, Screen, ServerMessage};
use ratatui::{backend::CrosstermBackend, widgets::Paragraph, Terminal};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

const DEFAULT_SESSION: &str = "default";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().is_some_and(|argument| argument == "daemon") {
        let session = args
            .get(1)
            .cloned()
            .unwrap_or_else(|| DEFAULT_SESSION.to_owned());
        return kodade_cli_daemon::run(session).await;
    }
    let session = parse_session(&args)?;
    attach(&session).await
}

fn parse_session(args: &[String]) -> Result<String> {
    match args {
        [] => Ok(DEFAULT_SESSION.to_owned()),
        [flag, name] if flag == "-s" || flag == "--session" => Ok(name.clone()),
        _ => bail!("usage: kodade-cli [-s SESSION]"),
    }
}

/// The visible binary starts its hidden `daemon` subcommand when no socket exists.
async fn attach(session: &str) -> Result<()> {
    let path = kodade_cli_daemon::socket_path(session);
    let stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            start_daemon(session)?;
            connect_when_ready(&path).await?
        }
        Err(error) => return Err(error).context("connect to Ködade CLI daemon"),
    };
    run_tui(stream).await
}

fn start_daemon(session: &str) -> Result<()> {
    let executable = env::current_exe().context("locate kodade-cli executable")?;
    std::process::Command::new(executable)
        .arg("daemon")
        .arg(session)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start Ködade CLI daemon")?;
    Ok(())
}

async fn connect_when_ready(path: &std::path::Path) -> Result<UnixStream> {
    for _ in 0..50 {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!("Ködade CLI daemon did not create its socket")
}

async fn run_tui(stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let (screens_tx, mut screens_rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(ServerMessage::Screen(screen)) = decode(line.as_bytes()) {
                if screens_tx.send(screen).await.is_err() {
                    break;
                }
            }
        }
    });

    let (cols, rows) = crossterm::terminal::size()?;
    writer
        .write_all(&encode(&ClientMessage::Hello { cols, rows })?)
        .await?;
    let mut terminal = setup_terminal()?;
    let result = tui_loop(&mut terminal, &mut writer, &mut screens_rx).await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout)).map_err(Into::into)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    screens: &mut mpsc::Receiver<Screen>,
) -> Result<()> {
    let mut screen = Screen::default();
    let mut prefix = false;
    loop {
        while let Ok(update) = screens.try_recv() {
            screen = update;
        }
        terminal.draw(|frame| {
            frame.render_widget(Paragraph::new(screen.contents.as_str()), frame.area())
        })?;

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if prefix && is_detach(key) => return Ok(()),
                Event::Key(key) if is_prefix(key) => prefix = true,
                Event::Key(key) => {
                    prefix = false;
                    if let Some(bytes) = key_bytes(key) {
                        writer
                            .write_all(&encode(&ClientMessage::Input { bytes })?)
                            .await?;
                    }
                }
                Event::Resize(cols, rows) => {
                    writer
                        .write_all(&encode(&ClientMessage::Resize { cols, rows })?)
                        .await?;
                }
                _ => {}
            }
        }
    }
}

fn is_prefix(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_detach(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('d') && !key.modifiers.contains(KeyModifiers::CONTROL)
}

fn key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(vec![(character.to_ascii_lowercase() as u8) & 0x1f])
        }
        KeyCode::Char(character) => Some(character.to_string().into_bytes()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}
