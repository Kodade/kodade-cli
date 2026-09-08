//! Independent client-side endpoint state for local and saved SSH machines.
//! Workers own transports; this model is deliberately transport-free so a bad
//! machine cannot block rendering, input, or reconnect scheduling elsewhere.

use anyhow::{anyhow, Context, Result};
use kodade_cli_proto::{
    decode, encode, ClientMessage, LayoutSnapshot, ServerMessage, PROTOCOL_VERSION,
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::{app, machines::MachineProfile, remote};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EndpointId {
    Local,
    Machine(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Online,
    Offline { retry: u8 },
    Attention(String),
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub label: String,
    pub status: Status,
    pub cached: Option<LayoutSnapshot>,
    retry_at: Instant,
}

#[derive(Debug)]
pub struct Manager {
    endpoints: BTreeMap<EndpointId, Endpoint>,
}

#[derive(Debug, Clone)]
pub struct SidebarMachine {
    pub id: EndpointId,
    pub label: String,
    pub status: String,
}

/// Bounded input fan-out.  The UI never writes to a socket directly: a worker
/// owns each socket and dropping/failing one worker only affects that endpoint.
#[derive(Debug)]
pub struct Router {
    selected: EndpointId,
    senders: BTreeMap<EndpointId, mpsc::Sender<ClientMessage>>,
    online: std::collections::BTreeSet<EndpointId>,
}

impl Router {
    pub fn new(selected: EndpointId) -> Self {
        Self {
            selected,
            senders: BTreeMap::new(),
            online: std::collections::BTreeSet::new(),
        }
    }

    pub fn register(&mut self, id: EndpointId, sender: mpsc::Sender<ClientMessage>) {
        self.senders.insert(id, sender);
    }

    /// Dropping the sender tells an idle or reconnecting worker to exit; queued
    /// input is discarded so disabling a machine cannot replay stale keystrokes.
    pub fn unregister(&mut self, id: &EndpointId) {
        self.senders.remove(id);
        self.online.remove(id);
        if &self.selected == id {
            self.selected = EndpointId::Local;
        }
    }

    pub fn mark_online(&mut self, id: EndpointId) {
        self.online.insert(id);
    }

    pub fn mark_offline(&mut self, id: &EndpointId) {
        self.online.remove(id);
    }

    pub fn select(&mut self, id: EndpointId) {
        self.selected = id;
    }
    pub fn selected(&self) -> &EndpointId {
        &self.selected
    }

    pub fn ids(&self) -> Vec<EndpointId> {
        self.senders.keys().cloned().collect()
    }

    pub fn send(&self, message: ClientMessage) -> Result<()> {
        if !self.online.contains(&self.selected) {
            return Ok(());
        }
        match self
            .senders
            .get(&self.selected)
            .ok_or_else(|| anyhow!("selected endpoint is offline"))?
            .try_send(message)
        {
            Ok(()) => Ok(()),
            // Input is deliberately lossy at this boundary. It is safer to
            // show endpoint state than replay a key into a later SSH session.
            Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => {
                Ok(())
            }
        }
    }
}

/// Start a saved-machine worker. It reconnects independently with a bounded
/// SSH setup and never holds the terminal/event-loop task hostage.
pub fn spawn_machine(
    profile: MachineProfile,
    session: String,
    cols: u16,
    rows: u16,
    updates: mpsc::Sender<(EndpointId, app::Update)>,
    commands: mpsc::Receiver<ClientMessage>,
) {
    tokio::spawn(async move {
        let id = EndpointId::Machine(profile.id.clone());
        let mut commands = commands;
        let mut retry = 0u8;
        let mut cols = cols;
        let mut rows = rows;
        let mut remote_session = profile.session.clone().unwrap_or(session);
        loop {
            let result = connect_machine(
                &profile,
                &remote_session,
                &mut cols,
                &mut rows,
                &updates,
                &mut commands,
            )
            .await;
            if commands.is_closed() {
                return;
            }
            if let Ok(Some(renamed)) = result {
                remote_session = renamed;
                retry = 0;
                continue;
            }
            let detail = result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "connection closed".into());
            let _ = updates
                .send((id.clone(), app::Update::EndpointFailed { reason: detail }))
                .await;
            let delay = Duration::from_secs(1u64 << retry.min(6));
            retry = retry.saturating_add(1).min(6);
            let sleep = tokio::time::sleep(delay);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    message = commands.recv() => if message.is_none() { return; },
                }
            }
        }
    });
}

async fn connect_machine(
    profile: &MachineProfile,
    session: &str,
    cols: &mut u16,
    rows: &mut u16,
    updates: &mpsc::Sender<(EndpointId, app::Update)>,
    commands: &mut mpsc::Receiver<ClientMessage>,
) -> Result<Option<String>> {
    let id = EndpointId::Machine(profile.id.clone());
    let (socket, _tunnel) = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(12), remote::connect_endpoint(&profile.target, session)) => {
            result.context("SSH endpoint setup timed out")??
        }
        command = commands.recv() => match command {
            None => return Err(anyhow!("endpoint command channel closed")),
            // The router only admits input for online endpoints. If a stale
            // message races setup, discard it rather than replaying it later.
            Some(_) => return Err(anyhow!("endpoint input discarded during setup")),
        },
    };
    let stream = tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(&socket))
        .await
        .context("connect forwarded endpoint socket timed out")?
        .context("connect forwarded endpoint socket")?;
    let (reader, mut writer) = stream.into_split();
    write_endpoint(
        &mut writer,
        &ClientMessage::Hello {
            cols: *cols,
            rows: *rows,
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let mut lines = BufReader::new(reader).lines();
    let connected_session = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow!("endpoint closed during handshake"))?;
            match decode::<ServerMessage>(line.as_bytes())? {
                ServerMessage::Welcome { session, version } if version == PROTOCOL_VERSION => {
                    break Ok(session)
                }
                ServerMessage::Welcome { version, .. } => {
                    return Err(anyhow!("protocol version mismatch: {version}"))
                }
                ServerMessage::Error { message } => return Err(anyhow!(message)),
                _ => {}
            }
        }
    })
    .await
    .context("endpoint handshake timed out")??;
    updates
        .send((
            id.clone(),
            app::Update::EndpointConnected {
                session: connected_session,
                socket: socket.clone(),
            },
        ))
        .await
        .map_err(|_| anyhow!("UI closed"))?;
    write_endpoint(&mut writer, &ClientMessage::Subscribe).await?;
    loop {
        tokio::select! {
            command = commands.recv() => {
                let command = command.ok_or_else(|| anyhow!("endpoint command channel closed"))?;
                if let ClientMessage::Resize { cols: next_cols, rows: next_rows } = command {
                    *cols = next_cols;
                    *rows = next_rows;
                    write_endpoint(&mut writer, &ClientMessage::Resize { cols: next_cols, rows: next_rows }).await?;
                    continue;
                }
                write_endpoint(&mut writer, &command).await?;
            }
            line = lines.next_line() => {
                let line = line?.ok_or_else(|| anyhow!("endpoint closed"))?;
                let update = match decode(line.as_bytes()) {
                    Ok(ServerMessage::Layout(layout)) => Some(app::Update::Layout(layout)),
                    Ok(ServerMessage::Welcome { session, .. }) => Some(app::Update::Session(session)),
                    Ok(ServerMessage::Notification(notification)) | Ok(ServerMessage::Event(kodade_cli_proto::Event::Notification(notification))) => Some(app::Update::Notification(notification)),
                    Ok(ServerMessage::Event(kodade_cli_proto::Event::SessionRenamed { name, socket })) => {
                        updates.send((id.clone(), app::Update::SessionRenamed { name: name.clone(), socket })).await.map_err(|_| anyhow!("UI closed"))?;
                        return Ok(Some(name));
                    }
                    Ok(ServerMessage::Error { message }) => return Err(anyhow!(message)),
                    Ok(ServerMessage::Shutdown) => return Err(anyhow!("endpoint shut down")),
                    _ => None,
                };
                if let Some(update) = update { updates.send((id.clone(), update)).await.map_err(|_| anyhow!("UI closed"))?; }
            }
        }
    }
}

async fn write_endpoint(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    message: &ClientMessage,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), writer.write_all(&encode(message)?))
        .await
        .context("endpoint write timed out")?
        .context("endpoint write")
}

impl Manager {
    pub fn new(profiles: &[MachineProfile], now: Instant) -> Self {
        let mut endpoints = BTreeMap::new();
        endpoints.insert(
            EndpointId::Local,
            Endpoint {
                label: "Local".into(),
                status: Status::Connecting,
                cached: None,
                retry_at: now,
            },
        );
        for profile in profiles {
            let id = EndpointId::Machine(profile.id.clone());
            endpoints.insert(
                id.clone(),
                Endpoint {
                    label: profile.label.clone(),
                    status: if profile.enabled {
                        Status::Connecting
                    } else {
                        Status::Disabled
                    },
                    cached: None,
                    retry_at: now,
                },
            );
        }
        Self { endpoints }
    }
    pub fn endpoint(&self, id: &EndpointId) -> Option<&Endpoint> {
        self.endpoints.get(id)
    }

    pub fn sidebar_machines(&self) -> Vec<SidebarMachine> {
        self.endpoints
            .iter()
            .map(|(id, endpoint)| SidebarMachine {
                id: id.clone(),
                label: endpoint.label.clone(),
                status: match &endpoint.status {
                    Status::Connecting => "connecting".into(),
                    Status::Online => "online".into(),
                    Status::Offline { retry } => format!("offline · retry {}", retry + 1),
                    Status::Attention(reason) => format!("attention · {reason}"),
                    Status::Disabled => "disabled".into(),
                },
            })
            .collect()
    }

    /// Reconcile profile metadata without throwing away live snapshots or an
    /// endpoint's current online/offline state.
    pub fn reconcile(&mut self, profiles: &[MachineProfile], now: Instant) {
        self.endpoints.retain(|id, _| {
            matches!(id, EndpointId::Local)
                || profiles
                    .iter()
                    .any(|profile| EndpointId::Machine(profile.id.clone()) == *id)
        });
        for profile in profiles {
            let id = EndpointId::Machine(profile.id.clone());
            match self.endpoints.get_mut(&id) {
                Some(endpoint) => {
                    endpoint.label = profile.label.clone();
                    if !profile.enabled {
                        endpoint.status = Status::Disabled;
                    }
                }
                None => {
                    self.endpoints.insert(
                        id,
                        Endpoint {
                            label: profile.label.clone(),
                            status: if profile.enabled {
                                Status::Connecting
                            } else {
                                Status::Disabled
                            },
                            cached: None,
                            retry_at: now,
                        },
                    );
                }
            }
        }
    }
    pub fn update(&mut self, id: &EndpointId, layout: LayoutSnapshot) {
        if let Some(e) = self.endpoints.get_mut(id) {
            e.cached = Some(layout);
            e.status = Status::Online;
        }
    }
    pub fn failed(&mut self, id: &EndpointId, now: Instant, reason: String) {
        if let Some(e) = self.endpoints.get_mut(id) {
            let retry = match e.status {
                Status::Offline { retry } => retry.saturating_add(1).min(6),
                _ => 0,
            };
            e.retry_at = now + Duration::from_secs(1u64 << retry);
            e.status = Status::Offline { retry };
            if reason.contains("password") || reason.contains("host key") {
                e.status = Status::Attention(reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::MachineProfile;
    fn profile() -> MachineProfile {
        MachineProfile {
            id: "m1".into(),
            label: "Build".into(),
            target: "build".into(),
            session: None,
            enabled: true,
        }
    }
    #[test]
    fn failures_keep_cached_state_and_backoff_independently() {
        let now = Instant::now();
        let id = EndpointId::Machine("m1".into());
        let mut m = Manager::new(&[profile()], now);
        m.failed(&id, now, "network down".into());
        assert!(matches!(
            m.endpoint(&id).unwrap().status,
            Status::Offline { retry: 0 }
        ));
        m.failed(&id, now + Duration::from_secs(1), "network down".into());
        assert!(matches!(
            m.endpoint(&id).unwrap().status,
            Status::Offline { retry: 1 }
        ));
    }
    #[test]
    fn disabled_endpoint_stays_disabled() {
        let now = Instant::now();
        let id = EndpointId::Machine("m1".into());
        let m = Manager::new(
            &[MachineProfile {
                enabled: false,
                ..profile()
            }],
            now,
        );
        assert!(matches!(m.endpoint(&id).unwrap().status, Status::Disabled));
    }

    #[tokio::test]
    async fn selected_router_never_leaks_input_to_another_machine() {
        let machine = EndpointId::Machine("m1".into());
        let (local_tx, mut local_rx) = mpsc::channel(1);
        let (machine_tx, mut machine_rx) = mpsc::channel(1);
        let mut router = Router::new(EndpointId::Local);
        router.register(EndpointId::Local, local_tx);
        router.register(machine.clone(), machine_tx);
        router.mark_online(EndpointId::Local);
        router.mark_online(machine.clone());
        router
            .send(ClientMessage::Input {
                bytes: b"local".to_vec(),
            })
            .unwrap();
        router.select(machine);
        router
            .send(ClientMessage::Input {
                bytes: b"remote".to_vec(),
            })
            .unwrap();
        assert!(
            matches!(local_rx.recv().await, Some(ClientMessage::Input { bytes }) if bytes == b"local")
        );
        assert!(
            matches!(machine_rx.recv().await, Some(ClientMessage::Input { bytes }) if bytes == b"remote")
        );
    }

    #[test]
    fn removing_selected_machine_returns_input_to_local() {
        let machine = EndpointId::Machine("m1".into());
        let (local_tx, _local_rx) = mpsc::channel(1);
        let (machine_tx, _machine_rx) = mpsc::channel(1);
        let mut router = Router::new(EndpointId::Local);
        router.register(EndpointId::Local, local_tx);
        router.register(machine.clone(), machine_tx);
        router.select(machine.clone());
        router.unregister(&machine);
        assert_eq!(router.selected(), &EndpointId::Local);
    }
}
