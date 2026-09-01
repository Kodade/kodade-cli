//! Persistent PTY host for Ködade CLI M0.

use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{decode, encode, ClientMessage, Screen, ServerMessage};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

/// M0 keeps this hierarchy explicit so M1 can add siblings without replacing
/// the daemon's ownership model.
struct Session {
    workspace: Workspace,
}

struct Workspace {
    tab: Tab,
}

struct Tab {
    pane: Pane,
}

struct Pane {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    updates: broadcast::Sender<Screen>,
}

pub fn socket_path(session: &str) -> PathBuf {
    let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let home = dirs::home_dir();
    let uid = env::var("UID").unwrap_or_else(|_| "unknown".to_owned());
    socket_path_for(
        session,
        runtime.as_deref(),
        home.as_deref(),
        &uid,
        cfg!(target_os = "macos"),
    )
}

fn socket_path_for(
    session: &str,
    runtime: Option<&Path>,
    home: Option<&Path>,
    uid: &str,
    is_macos: bool,
) -> PathBuf {
    let directory = if let Some(runtime) = runtime {
        runtime.join("kodade-cli")
    } else if is_macos {
        PathBuf::from(format!("/tmp/kodade-cli-{uid}"))
    } else if let Some(home) = home {
        home.join(".local/state/kodade-cli")
    } else {
        PathBuf::from(format!("/tmp/kodade-cli-{uid}"))
    };
    directory.join(format!("{session}.sock"))
}

pub async fn run(session_name: String) -> Result<()> {
    validate_session_name(&session_name)?;
    let socket = socket_path(&session_name);
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).context("create Ködade CLI socket directory")?;
    }
    if socket.exists() {
        remove_stale_socket(&socket).await?;
    }
    let listener = UnixListener::bind(&socket).context("bind Ködade CLI socket")?;
    let session = Arc::new(Session::spawn(80, 24)?);

    loop {
        let (stream, _) = listener.accept().await?;
        let session = Arc::clone(&session);
        let session_name = session_name.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_client(stream, session, session_name).await {
                eprintln!("Ködade CLI client disconnected: {error:#}");
            }
        });
    }
}

/// An existing socket belongs to a live daemon only when it accepts a connection.
async fn remove_stale_socket(socket: &Path) -> Result<()> {
    // Any failed or timed-out connect means nothing live owns the path.
    if let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(socket)).await
    {
        bail!("Ködade CLI daemon already running: {}", socket.display());
    }
    fs::remove_file(socket).context("remove stale Ködade CLI socket")?;
    Ok(())
}

fn validate_session_name(session: &str) -> Result<()> {
    if session.is_empty() || session.contains('/') || session == "." || session == ".." {
        bail!("session names must be non-empty path components");
    }
    Ok(())
}

impl Session {
    fn spawn(cols: u16, rows: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        pair.slave
            .spawn_command(command)
            .context("spawn login shell in PTY")?;

        let writer = pair.master.take_writer()?;
        let reader = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let (updates, _) = broadcast::channel(64);
        read_pty(reader, Arc::clone(&parser), updates.clone());

        Ok(Self {
            workspace: Workspace {
                tab: Tab {
                    pane: Pane {
                        writer: Mutex::new(writer),
                        master: Mutex::new(pair.master),
                        parser,
                        updates,
                    },
                },
            },
        })
    }

    fn pane(&self) -> &Pane {
        &self.workspace.tab.pane
    }
}

/// The PTY reader is blocking, so it lives in Tokio's blocking pool.
fn read_pty(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    updates: broadcast::Sender<Screen>,
) {
    tokio::task::spawn_blocking(move || {
        let mut bytes = [0_u8; 4096];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            let snapshot = {
                let mut parser = parser.lock().expect("PTY parser lock poisoned");
                parser.process(&bytes[..count]);
                snapshot(&parser)
            };
            let _ = updates.send(snapshot);
        }
    });
}

fn snapshot(parser: &vt100::Parser) -> Screen {
    let (cursor_row, cursor_col) = parser.screen().cursor_position();
    Screen {
        contents: parser.screen().contents(),
        cursor_row,
        cursor_col,
    }
}

async fn serve_client(stream: UnixStream, session: Arc<Session>, name: String) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader).lines();
    let mut updates = session.pane().updates.subscribe();
    write_server(&mut writer, &ServerMessage::Welcome { session: name }).await?;
    let initial_screen = {
        let parser = session
            .pane()
            .parser
            .lock()
            .map_err(|_| anyhow!("PTY parser lock poisoned"))?;
        snapshot(&parser)
    };
    write_server(&mut writer, &ServerMessage::Screen(initial_screen)).await?;

    loop {
        tokio::select! {
            line = reader.next_line() => {
                let Some(line) = line? else { return Ok(()); };
                handle_client_message(&session, decode::<ClientMessage>(line.as_bytes())?)?;
            }
            update = updates.recv() => {
                match update {
                    Ok(screen) => write_server(&mut writer, &ServerMessage::Screen(screen)).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let screen = {
                            let parser = session.pane().parser.lock().map_err(|_| anyhow!("PTY parser lock poisoned"))?;
                            snapshot(&parser)
                        };
                        write_server(&mut writer, &ServerMessage::Screen(screen)).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

fn handle_client_message(session: &Session, message: ClientMessage) -> Result<()> {
    match message {
        ClientMessage::Hello { cols, rows } | ClientMessage::Resize { cols, rows } => {
            session
                .pane()
                .master
                .lock()
                .map_err(|_| anyhow!("PTY master lock poisoned"))?
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })?;
            session
                .pane()
                .parser
                .lock()
                .map_err(|_| anyhow!("PTY parser lock poisoned"))?
                .set_size(rows, cols);
            let screen = {
                let parser = session
                    .pane()
                    .parser
                    .lock()
                    .map_err(|_| anyhow!("PTY parser lock poisoned"))?;
                snapshot(&parser)
            };
            let _ = session.pane().updates.send(screen);
        }
        ClientMessage::Input { bytes } => {
            let mut writer = session
                .pane()
                .writer
                .lock()
                .map_err(|_| anyhow!("PTY writer lock poisoned"))?;
            writer.write_all(&bytes)?;
            writer.flush()?;
        }
    }
    Ok(())
}

async fn write_server(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    message: &ServerMessage,
) -> Result<()> {
    writer.write_all(&encode(message)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_uses_runtime_directory_when_available() {
        assert_eq!(
            socket_path_for("work", Some(Path::new("/run/user/501")), None, "501", false),
            PathBuf::from("/run/user/501/kodade-cli/work.sock")
        );
    }

    #[test]
    fn macos_fallback_uses_per_user_tmp_directory() {
        assert_eq!(
            socket_path_for(
                "default",
                None,
                Some(Path::new("/Users/keith")),
                "501",
                true
            ),
            PathBuf::from("/tmp/kodade-cli-501/default.sock")
        );
    }

    #[test]
    fn vt100_snapshot_retains_terminal_contents() {
        let mut parser = vt100::Parser::new(3, 10, 100);
        parser.process(b"hello\r\nworld");
        assert!(snapshot(&parser).contents.contains("hello"));
        assert!(snapshot(&parser).contents.contains("world"));
    }

    #[tokio::test]
    async fn stale_socket_file_is_removed_before_binding() {
        let directory =
            std::env::temp_dir().join(format!("kodade-cli-stale-socket-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("default.sock");
        fs::write(&socket, b"stale").expect("create stale socket file");

        remove_stale_socket(&socket)
            .await
            .expect("remove stale socket file");

        assert!(!socket.exists());
        fs::remove_dir(&directory).expect("remove test directory");
    }
}
