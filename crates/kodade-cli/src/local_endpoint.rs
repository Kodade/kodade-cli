//! An attached local transport, including explicit live-upgrade reconnects.

use anyhow::{anyhow, Context, Result};
use kodade_cli_proto::{decode, encode, ClientMessage, Event, ServerMessage, PROTOCOL_VERSION};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixStream,
    },
    sync::mpsc,
};

use crate::{app::Update, endpoints::Updates};

pub struct Connection {
    pub lines: Lines<BufReader<OwnedReadHalf>>,
    pub writer: OwnedWriteHalf,
}

pub struct Viewport {
    pub cols: u16,
    pub rows: u16,
    pub compact: bool,
    pub colors: Option<kodade_cli_proto::TerminalColors>,
}

pub fn spawn(
    mut connection: Connection,
    mut socket: PathBuf,
    mut viewport: Viewport,
    updates: Updates,
    mut commands: mpsc::Receiver<ClientMessage>,
) {
    tokio::spawn(async move {
        let mut view = crate::reconnect_view::View::default();
        let result: Result<()> = async {
            loop {
                tokio::select! {
                    message = commands.recv() => {
                        let Some(message) = message else { return Ok(()); };
                        match message {
                            ClientMessage::Resize { cols, rows } => { viewport.cols = cols; viewport.rows = rows; }
                            ClientMessage::SetCompactView { enabled } => viewport.compact = enabled,
                            _ => {}
                        }
                        tokio::time::timeout(Duration::from_secs(5), connection.writer.write_all(&encode(&message)?))
                            .await.context("local write timed out")??;
                    }
                    line = connection.lines.next_line() => {
                        let line = line?.context("local endpoint disconnected")?;
                        let message: ServerMessage = decode(line.as_bytes())?;
                        let update = match message {
                            ServerMessage::Upgrading => {
                                updates.send(Update::EndpointFailed { reason: "daemon upgrading; reconnecting".into() }).await?;
                                // Requests queued against the retired connection must never be
                                // replayed into a newly selected pane. Retain only viewport state.
                                while let Ok(message) = commands.try_recv() {
                                    match message {
                                        ClientMessage::Resize { cols, rows } => { viewport.cols = cols; viewport.rows = rows; }
                                        ClientMessage::SetCompactView { enabled } => viewport.compact = enabled,
                                        _ => {}
                                    }
                                }
                                let (replacement, session) = reconnect(&socket, &viewport, &view).await?;
                                connection = replacement;
                                // The UI may have queued input before it processed the offline
                                // notice. Discard it after reconnect too, retaining resize only.
                                while let Ok(message) = commands.try_recv() {
                                    match message {
                                        ClientMessage::Resize { cols, rows } => { viewport.cols = cols; viewport.rows = rows; }
                                        ClientMessage::SetCompactView { enabled } => viewport.compact = enabled,
                                        _ => {}
                                    }
                                }
                                connection.writer.write_all(&encode(&ClientMessage::Resize { cols: viewport.cols, rows: viewport.rows })?).await?;
                                connection.writer.write_all(&encode(&ClientMessage::SetCompactView { enabled: viewport.compact })?).await?;
                                connection.writer.write_all(&encode(&ClientMessage::SetTerminalColors { colors: viewport.colors.clone() })?).await?;
                                Update::EndpointConnected { session, socket: socket.clone() }
                            }
                            ServerMessage::Layout(layout) => { view.observe(&layout); Update::Layout(layout) },
                            ServerMessage::Clipboard { pane, text } => Update::Clipboard { pane, text },
                            ServerMessage::Welcome { session, .. } => Update::Session(session),
                            ServerMessage::Notification(notification)
                            | ServerMessage::Event(Event::Notification(notification)) => Update::Notification(notification),
                            ServerMessage::Event(Event::SessionRenamed { name, socket: renamed }) => {
                                socket = renamed.clone();
                                Update::SessionRenamed { name, socket: renamed }
                            }
                            ServerMessage::Error { message } => Update::RequestError(message),
                            ServerMessage::Shutdown => {
                                updates.send(Update::EndpointFailed { reason: "local endpoint shut down".into() }).await?;
                                return Ok(());
                            }
                            _ => continue,
                        };
                        updates.send(update).await?;
                    }
                }
            }
        }.await;
        if let Err(error) = result {
            let _ = updates
                .send(Update::RequestError(format!("connection ended: {error:#}")))
                .await;
            let _ = updates
                .send(Update::EndpointFailed {
                    reason: "local endpoint disconnected".into(),
                })
                .await;
        }
    });
}

async fn reconnect(
    socket: &std::path::Path,
    viewport: &Viewport,
    view: &crate::reconnect_view::View,
) -> Result<(Connection, String)> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let stream = loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(&encode(&ClientMessage::Hello {
                cols: viewport.cols,
                rows: viewport.rows,
                version: PROTOCOL_VERSION,
            })?)
            .await?;
        let line = lines
            .next_line()
            .await?
            .context("replacement closed during handshake")?;
        let session = match decode::<ServerMessage>(line.as_bytes())? {
            ServerMessage::Welcome { session, version } if version == PROTOCOL_VERSION => session,
            _ => {
                return Err(anyhow!(
                    "replacement daemon rejected the protocol handshake"
                ))
            }
        };
        writer
            .write_all(&encode(&ClientMessage::SetCompactView {
                enabled: viewport.compact,
            })?)
            .await?;
        writer
            .write_all(&encode(&ClientMessage::SetTerminalColors {
                colors: viewport.colors.clone(),
            })?)
            .await?;
        for message in view.restore() {
            writer.write_all(&encode(&message)?).await?;
        }
        writer
            .write_all(&encode(&ClientMessage::Subscribe)?)
            .await?;
        Ok((Connection { lines, writer }, session))
    })
    .await
    .context("replacement daemon reconnect timed out after 10s")?
}
