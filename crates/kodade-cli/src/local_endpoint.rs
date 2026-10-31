//! An attached local transport, including explicit live-upgrade reconnects.

use anyhow::{anyhow, Context, Result};
use kodade_cli_proto::{decode, encode, ClientMessage, Event, ServerMessage, PROTOCOL_VERSION};
use std::{collections::VecDeque, path::PathBuf, time::Duration};
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

impl Viewport {
    pub(crate) fn apply(&mut self, message: &ClientMessage) {
        match message {
            ClientMessage::Resize { cols, rows } => {
                self.cols = *cols;
                self.rows = *rows;
            }
            ClientMessage::SetCompactView { enabled } => self.compact = *enabled,
            ClientMessage::SetTerminalColors { colors } => self.colors = colors.clone(),
            _ => {}
        }
    }
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
        let mut pending = VecDeque::new();
        let result: Result<()> = async {
            loop {
                tokio::select! {
                    message = commands.recv() => {
                        let Some(message) = message else { return Ok(()); };
                        viewport.apply(&message);
                        tokio::time::timeout(Duration::from_secs(5), connection.writer.write_all(&encode(&message)?))
                            .await.context("local write timed out")??;
                    }
                    message = async {
                        if let Some(message) = pending.pop_front() {
                            return Ok(message);
                        }
                        let line = connection.lines.next_line().await?.context("local endpoint disconnected")?;
                        decode::<ServerMessage>(line.as_bytes())
                    } => {
                        let message = message?;
                        let update = match message {
                            ServerMessage::Upgrading => {
                                updates.send(Update::EndpointFailed { reason: "daemon upgrading; reconnecting".into() }).await?;
                                // Requests queued against the retired connection must never be
                                // replayed into a newly selected pane. Retain viewport state.
                                while let Ok(message) = commands.try_recv() {
                                    viewport.apply(&message);
                                }
                                let (replacement, session, restored) = reconnect(&socket, &viewport, &view).await?;
                                pending = restored;
                                connection = replacement;
                                // The UI may have queued input before it processed the offline
                                // notice. Discard it after reconnect too, retaining viewport state.
                                while let Ok(message) = commands.try_recv() {
                                    viewport.apply(&message);
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
                    reason: format!("local endpoint disconnected: {error:#}"),
                })
                .await;
        }
    });
}

async fn reconnect(
    socket: &std::path::Path,
    viewport: &Viewport,
    view: &crate::reconnect_view::View,
) -> Result<(Connection, String, VecDeque<ServerMessage>)> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let stream = loop {
                match UnixStream::connect(socket).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            };
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let hello = writer
                .write_all(&encode(&ClientMessage::Hello {
                    cols: viewport.cols,
                    rows: viewport.rows,
                    version: PROTOCOL_VERSION,
                })?)
                .await;
            let session = loop {
                let line = lines
                    .next_line()
                    .await?
                    .context("replacement closed during handshake")?;
                match decode::<ServerMessage>(line.as_bytes())? {
                    ServerMessage::Welcome { session, version } if version == PROTOCOL_VERSION => {
                        break Some(session)
                    }
                    ServerMessage::Upgrading => break None,
                    // A subsequent upgrade can pause dispatch before Hello is
                    // processed. Only its explicit commit notice permits reconnect.
                    ServerMessage::Error { message } if message == "daemon upgrade in progress" => {
                        continue
                    }
                    ServerMessage::Error { message } => return Err(anyhow!(message)),
                    _ => {
                        return Err(anyhow!(
                            "replacement daemon rejected the protocol handshake"
                        ))
                    }
                }
            };
            let Some(session) = session else {
                continue;
            };
            hello?;
            let mut restore = vec![
                ClientMessage::SetCompactView {
                    enabled: viewport.compact,
                },
                ClientMessage::SetTerminalColors {
                    colors: viewport.colors.clone(),
                },
            ];
            restore.extend(view.restore());
            restore.push(ClientMessage::Subscribe);
            // This ordered reply acknowledges every preceding restore command.
            restore.push(ClientMessage::Query(kodade_cli_proto::QueryKind::Version));
            let mut bytes = Vec::new();
            for message in restore {
                bytes.extend(encode(&message)?);
            }
            let written = writer.write_all(&bytes).await;
            // Even if a retiring peer closed its write side, consume its queued
            // lifecycle notice. An arbitrary EOF or write error is never a retry.
            if let Some(pending) = read_restored_view(&mut lines).await? {
                written?;
                return Ok((Connection { lines, writer }, session, pending));
            }
            // Another committed upgrade interrupted restoration. Keep the original
            // acknowledged view and the original overall deadline; replay no input.
        }
    })
    .await
    .context("replacement daemon reconnect timed out after 10s")?
}

async fn read_restored_view(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
) -> Result<Option<VecDeque<ServerMessage>>> {
    let mut pending = VecDeque::new();
    let mut layout = None;
    let mut paused = false;
    loop {
        let line = lines
            .next_line()
            .await?
            .context("replacement closed restoring view")?;
        match decode::<ServerMessage>(line.as_bytes())? {
            ServerMessage::Version { version } if version == PROTOCOL_VERSION && !paused => {
                pending.push_back(ServerMessage::Layout(
                    layout.context("replacement omitted restored layout")?,
                ));
                return Ok(Some(pending));
            }
            ServerMessage::Layout(snapshot) => layout = Some(snapshot),
            ServerMessage::Error { message } if message == "daemon upgrade in progress" => {
                // Version replies still work while dispatch is paused, so they
                // cannot acknowledge commands rejected during that pause.
                paused = true;
            }
            ServerMessage::Error { message } => return Err(anyhow!(message)),
            ServerMessage::Upgrading => return Ok(None),
            ServerMessage::Shutdown => return Err(anyhow!("replacement shut down restoring view")),
            message => pending.push_back(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{
        AgentStateKind, LayoutSnapshot, LayoutTree, PaneId, PaneSnapshot, Screen, TabId,
        WorkspaceId,
    };

    fn layout(focused: u64) -> LayoutSnapshot {
        LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(2),
            workspaces: vec![],
            tabs: vec![],
            tree: LayoutTree::Leaf {
                pane: PaneId(focused),
            },
            panes: vec![PaneSnapshot {
                id: PaneId(focused),
                title: "recorder".into(),
                focused: true,
                scroll_offset: 0,
                screen: Screen::default(),
                agent: None,
                agent_generation: 0,
                activity_revision: 0,
                state: AgentStateKind::Idle,
                state_reason: String::new(),
                state_age_secs: 0,
                cwd: None,
            }],
            zoomed: false,
            restored: false,
        }
    }

    #[tokio::test]
    async fn reconnect_hides_intermediate_focus_until_restore_acknowledged() {
        let path = std::env::temp_dir().join(format!(
            "kr-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Use /tmp on macOS too, where the default temporary directory is long.
        let path = PathBuf::from("/tmp").join(path.file_name().unwrap());
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let mut focused = 1;
                loop {
                    let message: ClientMessage =
                        decode(lines.next_line().await.unwrap().unwrap().as_bytes()).unwrap();
                    match message {
                        ClientMessage::Hello { .. } => {
                            writer
                                .write_all(
                                    &encode(&ServerMessage::Welcome {
                                        session: "test".into(),
                                        version: PROTOCOL_VERSION,
                                    })
                                    .unwrap(),
                                )
                                .await
                                .unwrap();
                        }
                        ClientMessage::FocusPaneId { id } => focused = id.0,
                        ClientMessage::Query(kodade_cli_proto::QueryKind::Version) => {
                            // Both panes can be visible even in snapshots with the wrong focus.
                            // Only this ordered reply proves all restoration commands ran.
                            writer
                                .write_all(
                                    &encode(&ServerMessage::Version {
                                        version: PROTOCOL_VERSION,
                                    })
                                    .unwrap(),
                                )
                                .await
                                .unwrap();
                            break;
                        }
                        _ => {}
                    }
                    writer
                        .write_all(&encode(&ServerMessage::Layout(layout(focused))).unwrap())
                        .await
                        .unwrap();
                }
            }
        });
        let mut view = crate::reconnect_view::View::default();
        view.observe(&layout(9));
        let viewport = Viewport {
            cols: 120,
            rows: 30,
            compact: false,
            colors: None,
        };
        for _ in 0..2 {
            let (_connection, _, pending) = reconnect(&path, &viewport, &view).await.unwrap();
            let layouts: Vec<_> = pending
                .into_iter()
                .filter_map(|message| match message {
                    ServerMessage::Layout(layout) => Some(layout),
                    _ => None,
                })
                .collect();
            assert_eq!(
                layouts.len(),
                1,
                "never publish the default or intermediate view"
            );
            assert_eq!(layouts[0].panes[0].id, PaneId(9));
            view.observe(&layouts[0]);
            assert!(matches!(
                view.restore().last(),
                Some(ClientMessage::FocusPaneId { id: PaneId(9) })
            ));
        }
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }
    fn test_listener() -> (PathBuf, tokio::net::UnixListener) {
        let path = PathBuf::from(format!(
            "/tmp/ku-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        (path, listener)
    }

    #[tokio::test]
    async fn overlapping_upgrades_during_hello_and_restore_keep_acknowledged_focus() {
        let (path, listener) = test_listener();
        let server = tokio::spawn(async move {
            for generation in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let hello: ClientMessage =
                    decode(lines.next_line().await.unwrap().unwrap().as_bytes()).unwrap();
                assert!(matches!(hello, ClientMessage::Hello { .. }));
                if generation == 0 {
                    for message in [
                        ServerMessage::Error {
                            message: "daemon upgrade in progress".into(),
                        },
                        ServerMessage::Upgrading,
                    ] {
                        writer.write_all(&encode(&message).unwrap()).await.unwrap();
                    }
                    continue;
                }
                writer
                    .write_all(
                        &encode(&ServerMessage::Welcome {
                            session: "test".into(),
                            version: PROTOCOL_VERSION,
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                let mut focus = None;
                loop {
                    let message: ClientMessage =
                        decode(lines.next_line().await.unwrap().unwrap().as_bytes()).unwrap();
                    match message {
                        ClientMessage::FocusPaneId { id } => focus = Some(id),
                        ClientMessage::Input { .. } | ClientMessage::SendToPane { .. } => {
                            panic!("replayed user input")
                        }
                        ClientMessage::Query(kodade_cli_proto::QueryKind::Version) => break,
                        _ => {}
                    }
                }
                assert_eq!(
                    focus,
                    Some(PaneId(9)),
                    "retain the original acknowledged focus in every generation"
                );
                let messages = if generation == 1 {
                    vec![
                        ServerMessage::Layout(layout(1)),
                        ServerMessage::Error {
                            message: "daemon upgrade in progress".into(),
                        },
                        // A read-only probe can succeed although every restore
                        // mutation was rejected. It must not publish this view.
                        ServerMessage::Version {
                            version: PROTOCOL_VERSION,
                        },
                        ServerMessage::Upgrading,
                    ]
                } else {
                    vec![
                        ServerMessage::Layout(layout(9)),
                        ServerMessage::Version {
                            version: PROTOCOL_VERSION,
                        },
                    ]
                };
                for message in messages {
                    writer.write_all(&encode(&message).unwrap()).await.unwrap();
                }
            }
        });
        let mut view = crate::reconnect_view::View::default();
        view.observe(&layout(9));
        let viewport = Viewport {
            cols: 120,
            rows: 30,
            compact: false,
            colors: None,
        };
        let (_connection, _, pending) = reconnect(&path, &viewport, &view).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.front(), Some(&ServerMessage::Layout(layout(9))));
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn reconnect_does_not_retry_eof_shutdown_or_unrelated_errors() {
        for during_restore in [false, true] {
            for messages in [
                vec![],
                vec![ServerMessage::Shutdown],
                vec![ServerMessage::Error {
                    message: "no such pane".into(),
                }],
                vec![
                    ServerMessage::Error {
                        message: "daemon upgrade in progress".into(),
                    },
                    ServerMessage::Version {
                        version: PROTOCOL_VERSION,
                    },
                ],
            ] {
                let (path, listener) = test_listener();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    lines.next_line().await.unwrap().unwrap();
                    if during_restore {
                        writer
                            .write_all(
                                &encode(&ServerMessage::Welcome {
                                    session: "test".into(),
                                    version: PROTOCOL_VERSION,
                                })
                                .unwrap(),
                            )
                            .await
                            .unwrap();
                        loop {
                            let message: ClientMessage =
                                decode(lines.next_line().await.unwrap().unwrap().as_bytes())
                                    .unwrap();
                            if matches!(
                                message,
                                ClientMessage::Query(kodade_cli_proto::QueryKind::Version)
                            ) {
                                break;
                            }
                        }
                    }
                    for message in messages {
                        writer.write_all(&encode(&message).unwrap()).await.unwrap();
                    }
                });
                let viewport = Viewport {
                    cols: 120,
                    rows: 30,
                    compact: false,
                    colors: None,
                };
                let view = crate::reconnect_view::View::default();
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    reconnect(&path, &viewport, &view),
                )
                .await;
                assert!(result
                    .expect("must fail promptly without another connect attempt")
                    .is_err());
                server.await.unwrap();
                std::fs::remove_file(path).unwrap();
            }
        }
    }
}
