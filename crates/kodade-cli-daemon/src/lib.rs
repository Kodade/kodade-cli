//! Persistent PTY host and session model for Ködade CLI.

mod agent;
mod layout;
mod manifest;

use std::{
    collections::HashMap,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{
    decode, encode, AgentInfo, AgentStateKind, ClientMessage, Direction, LayoutSnapshot,
    LayoutTree, PaneId, PaneSnapshot, QueryKind, Screen, ServerMessage, SidebarTabInfo, SplitAxis,
    TabId, TabInfo, WorkspaceId, WorkspaceInfo,
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

struct Session {
    state: Mutex<SessionState>,
    panes: Mutex<HashMap<PaneId, Arc<Pane>>>,
    updates: broadcast::Sender<()>,
    shutdown: broadcast::Sender<()>,
    size: Mutex<(u16, u16)>,
    manifests: Vec<manifest::Manifest>,
}
struct SessionState {
    workspaces: Vec<Workspace>,
    active_workspace: WorkspaceId,
    next_id: u64,
}
struct Workspace {
    id: WorkspaceId,
    name: String,
    tabs: Vec<Tab>,
    active_tab: TabId,
}
struct Tab {
    id: TabId,
    name: String,
    tree: LayoutTree,
    focused: PaneId,
    zoomed: bool,
}
struct Pane {
    title: Mutex<String>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    scroll_offset: Mutex<usize>,
    last_output: Arc<Mutex<Instant>>,
    hook: Mutex<Option<ReportedHook>>,
    spawn_process: String,
    process: Mutex<ProcessEvidence>,
}

#[derive(Clone)]
struct ReportedHook {
    state: AgentStateKind,
    source: String,
    reported_at: Instant,
}

struct ProcessEvidence {
    name: Option<String>,
    checked_at: Instant,
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
    let mut shutdown = session.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                drop(listener);
                let _ = fs::remove_file(&socket);
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let session = Arc::clone(&session);
                let session_name = session_name.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_client(stream, session, session_name).await {
                        eprintln!("Ködade CLI client disconnected: {error:#}");
                    }
                });
            }
        }
    }
}

async fn remove_stale_socket(socket: &Path) -> Result<()> {
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
        let (updates, _) = broadcast::channel(64);
        let (shutdown, _) = broadcast::channel(16);
        let session = Self {
            state: Mutex::new(SessionState {
                workspaces: Vec::new(),
                active_workspace: WorkspaceId(1),
                next_id: 1,
            }),
            panes: Mutex::new(HashMap::new()),
            updates,
            shutdown,
            size: Mutex::new((cols, rows)),
            manifests: manifest::load()?,
        };
        let pane = session.new_pane("shell")?;
        let tab = Tab {
            id: session.tab_id(),
            name: "shell".into(),
            tree: LayoutTree::Leaf { pane },
            focused: pane,
            zoomed: false,
        };
        session
            .state
            .lock()
            .expect("state lock poisoned")
            .workspaces
            .push(Workspace {
                id: WorkspaceId(1),
                name: "default".into(),
                active_tab: tab.id,
                tabs: vec![tab],
            });
        Ok(session)
    }

    fn next_id(&self) -> u64 {
        let mut state = self.state.lock().expect("state lock poisoned");
        state.next_id += 1;
        state.next_id
    }
    fn pane_id(&self) -> PaneId {
        PaneId(self.next_id())
    }
    fn tab_id(&self) -> TabId {
        TabId(self.next_id())
    }
    fn workspace_id(&self) -> WorkspaceId {
        WorkspaceId(self.next_id())
    }

    fn new_pane(&self, title: &str) -> Result<PaneId> {
        let id = self.pane_id();
        let (cols, rows) = *self.size.lock().expect("size lock poisoned");
        let pane = Arc::new(Pane::spawn(title, cols, rows, self.updates.clone())?);
        self.panes
            .lock()
            .expect("pane lock poisoned")
            .insert(id, pane);
        Ok(id)
    }

    fn active_tab_mut(state: &mut SessionState) -> &mut Tab {
        let workspace = state
            .workspaces
            .iter_mut()
            .find(|item| item.id == state.active_workspace)
            .expect("active workspace exists");
        workspace
            .tabs
            .iter_mut()
            .find(|item| item.id == workspace.active_tab)
            .expect("active tab exists")
    }
    fn active_tab(state: &SessionState) -> &Tab {
        let workspace = state
            .workspaces
            .iter()
            .find(|item| item.id == state.active_workspace)
            .expect("active workspace exists");
        workspace
            .tabs
            .iter()
            .find(|item| item.id == workspace.active_tab)
            .expect("active tab exists")
    }
    fn notify(&self) {
        let _ = self.updates.send(());
    }

    fn snapshot(&self) -> Result<LayoutSnapshot> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?;
        let workspace = state
            .workspaces
            .iter()
            .find(|item| item.id == state.active_workspace)
            .expect("active workspace exists");
        let tab = Self::active_tab(&state);
        let tree = if tab.zoomed {
            LayoutTree::Leaf { pane: tab.focused }
        } else {
            tab.tree.clone()
        };
        let mut ids = Vec::new();
        layout::leaves(&tree, &mut ids);
        let panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        let now = Instant::now();
        let detections: HashMap<_, _> = panes
            .iter()
            .map(|(id, pane)| (*id, pane.detect(&self.manifests, now)))
            .collect();
        let snapshots = ids
            .into_iter()
            .filter_map(|id| {
                panes.get(&id).map(|pane| PaneSnapshot {
                    id,
                    title: pane.title.lock().expect("title lock poisoned").clone(),
                    focused: id == tab.focused,
                    scroll_offset: pane.scroll_offset(),
                    screen: pane.snapshot(),
                    agent: detections[&id].agent.clone(),
                    state: detections[&id].state,
                    state_reason: detections[&id].reason.clone(),
                })
            })
            .collect();
        Ok(LayoutSnapshot {
            active_workspace: workspace.id,
            active_tab: tab.id,
            workspaces: state
                .workspaces
                .iter()
                .map(|item| {
                    let tabs = item
                        .tabs
                        .iter()
                        .map(|tab| sidebar_tab_info(tab, &panes, &detections))
                        .collect::<Vec<_>>();
                    WorkspaceInfo {
                        id: item.id,
                        name: item.name.clone(),
                        active: item.id == workspace.id,
                        state: agent::rollup(tabs.iter().map(|tab| tab.state)),
                        tabs,
                    }
                })
                .collect(),
            tabs: workspace
                .tabs
                .iter()
                .map(|item| TabInfo {
                    id: item.id,
                    name: item.name.clone(),
                    active: item.id == tab.id,
                    state: tab_state(item, &detections),
                })
                .collect(),
            tree,
            panes: snapshots,
            zoomed: tab.zoomed,
        })
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        *self
            .size
            .lock()
            .map_err(|_| anyhow!("size lock poisoned"))? = (cols, rows);
        let snapshot = self.snapshot()?;
        let mut sizes = Vec::new();
        pane_sizes(
            &snapshot.tree,
            cols.max(1),
            rows.saturating_sub(2).max(1),
            &mut sizes,
        );
        let panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        for (id, width, height) in sizes {
            if let Some(pane) = panes.get(&id) {
                pane.resize(width.max(1), height.max(1))?;
            }
        }
        self.notify();
        Ok(())
    }

    fn resize_current(&self) -> Result<()> {
        let (cols, rows) = *self
            .size
            .lock()
            .map_err(|_| anyhow!("size lock poisoned"))?;
        self.resize(cols, rows)
    }

    fn handle(&self, message: ClientMessage) -> Result<()> {
        match message {
            ClientMessage::Query(QueryKind::Layout) => {}
            ClientMessage::Hello { cols, rows } | ClientMessage::Resize { cols, rows } => {
                self.resize(cols, rows)?
            }
            ClientMessage::Input { bytes } => {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let focused = Self::active_tab(&state).focused;
                drop(state);
                if let Some(pane) = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?
                    .get(&focused)
                {
                    pane.reset_scrollback();
                    pane.write(&bytes)?;
                }
                self.notify();
            }
            ClientMessage::SplitRight | ClientMessage::SplitDown => {
                let axis = if matches!(message, ClientMessage::SplitRight) {
                    SplitAxis::Horizontal
                } else {
                    SplitAxis::Vertical
                };
                let pane = self.new_pane("shell")?;
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let tab = Self::active_tab_mut(&mut state);
                let focused = tab.focused;
                layout::split(&mut tab.tree, focused, axis, pane);
                tab.focused = pane;
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::ClosePane => self.close_pane()?,
            ClientMessage::FocusPane { direction } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let tab = Self::active_tab_mut(&mut state);
                if !tab.zoomed {
                    if let Some(pane) = layout::focus_neighbor(&tab.tree, tab.focused, direction) {
                        tab.focused = pane;
                    }
                }
                self.notify();
            }
            ClientMessage::FocusPaneId { id } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let changed = focus_pane_id(&mut state, id);
                drop(state);
                if changed {
                    self.resize_current()?;
                } else {
                    self.notify();
                }
            }
            ClientMessage::SendToPane { id, bytes } => {
                let panes = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?;
                let pane = panes
                    .get(&id)
                    .ok_or_else(|| anyhow!("pane {} not found", id.0))?;
                pane.reset_scrollback();
                pane.write(&bytes)?;
                self.notify();
            }
            ClientMessage::RenamePaneId { id, name } => {
                let panes = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?;
                let pane = panes
                    .get(&id)
                    .ok_or_else(|| anyhow!("pane {} not found", id.0))?;
                *pane.title.lock().expect("title lock poisoned") = name;
                self.notify();
            }
            ClientMessage::KillSession => {
                let _ = self.shutdown.send(());
            }
            ClientMessage::NewTab => {
                let pane = self.new_pane("shell")?;
                let id = self.tab_id();
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let active = state.active_workspace;
                let workspace = state
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == active)
                    .expect("active workspace exists");
                workspace.tabs.push(Tab {
                    id,
                    name: format!("tab {}", workspace.tabs.len() + 1),
                    tree: LayoutTree::Leaf { pane },
                    focused: pane,
                    zoomed: false,
                });
                workspace.active_tab = id;
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::NextTab | ClientMessage::PrevTab => {
                let next = matches!(message, ClientMessage::NextTab);
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let active = state.active_workspace;
                let workspace = state
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == active)
                    .expect("active workspace exists");
                let index = workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.id == workspace.active_tab)
                    .unwrap_or(0);
                let index = if next {
                    (index + 1) % workspace.tabs.len()
                } else {
                    (index + workspace.tabs.len() - 1) % workspace.tabs.len()
                };
                workspace.active_tab = workspace.tabs[index].id;
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::SelectTab { id } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let active = state.active_workspace;
                let workspace = state
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == active)
                    .expect("active workspace exists");
                if workspace.tabs.iter().any(|tab| tab.id == id) {
                    workspace.active_tab = id;
                }
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::NewWorkspace { name } => {
                let pane = self.new_pane("shell")?;
                let tab_id = self.tab_id();
                let id = self.workspace_id();
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                state.workspaces.push(Workspace {
                    id,
                    name,
                    active_tab: tab_id,
                    tabs: vec![Tab {
                        id: tab_id,
                        name: "shell".into(),
                        tree: LayoutTree::Leaf { pane },
                        focused: pane,
                        zoomed: false,
                    }],
                });
                state.active_workspace = id;
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::SelectWorkspace { id } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                if state.workspaces.iter().any(|workspace| workspace.id == id) {
                    state.active_workspace = id;
                }
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::RenamePane { name } => {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let focused = Self::active_tab(&state).focused;
                drop(state);
                if let Some(pane) = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?
                    .get(&focused)
                {
                    *pane.title.lock().expect("title lock poisoned") = name;
                }
                self.notify();
            }
            ClientMessage::RenameTab { name } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                Self::active_tab_mut(&mut state).name = name;
                self.notify();
            }
            ClientMessage::RenameWorkspace { name } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let active = state.active_workspace;
                state
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == active)
                    .expect("active workspace exists")
                    .name = name;
                self.notify();
            }
            ClientMessage::ResizePane { direction, cells } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let span = match direction {
                    Direction::Left | Direction::Right => {
                        self.size.lock().expect("size lock poisoned").0
                    }
                    Direction::Up | Direction::Down => {
                        self.size.lock().expect("size lock poisoned").1
                    }
                };
                let tab = Self::active_tab_mut(&mut state);
                layout::resize(
                    &mut tab.tree,
                    tab.focused,
                    direction,
                    cells as f32 / span.max(1) as f32,
                );
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::ScrollPane { id, delta } => {
                if let Some(pane) = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?
                    .get(&id)
                {
                    pane.scroll(delta);
                }
                self.notify();
            }
            ClientMessage::ZoomPane => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let tab = Self::active_tab_mut(&mut state);
                tab.zoomed = !tab.zoomed;
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::AgentState {
                pane,
                state,
                source,
            } => {
                let panes = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?;
                let pane = panes
                    .get(&pane)
                    .ok_or_else(|| anyhow!("pane {} not found", pane.0))?;
                *pane.hook.lock().expect("hook lock poisoned") = Some(ReportedHook {
                    state,
                    source,
                    reported_at: Instant::now(),
                });
                self.notify();
            }
        }
        Ok(())
    }

    fn close_pane(&self) -> Result<()> {
        let needs_fresh = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let workspace = state
                .workspaces
                .iter()
                .find(|item| item.id == state.active_workspace)
                .expect("active workspace exists");
            let tab = workspace
                .tabs
                .iter()
                .find(|tab| tab.id == workspace.active_tab)
                .expect("active tab exists");
            workspace.tabs.len() == 1 && matches!(tab.tree, LayoutTree::Leaf { .. })
        };
        // Spawn before taking the state lock: id allocation also reads session state.
        let fresh_pane = needs_fresh.then(|| self.new_pane("shell")).transpose()?;
        let removed;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let active = state.active_workspace;
            let workspace = state
                .workspaces
                .iter_mut()
                .find(|item| item.id == active)
                .expect("active workspace exists");
            let tab_index = workspace
                .tabs
                .iter()
                .position(|tab| tab.id == workspace.active_tab)
                .expect("active tab exists");
            let tab_count = workspace.tabs.len();
            let tab = &mut workspace.tabs[tab_index];
            let focused = tab.focused;
            match layout::close(tab.tree.clone(), focused) {
                Some(tree) => {
                    tab.tree = tree;
                    let mut leaves = Vec::new();
                    layout::leaves(&tab.tree, &mut leaves);
                    tab.focused = leaves[0];
                    removed = Some(focused);
                }
                None if tab_count > 1 => {
                    workspace.tabs.remove(tab_index);
                    workspace.active_tab = workspace.tabs[tab_index.saturating_sub(1)].id;
                    removed = Some(focused);
                }
                None => {
                    // A workspace is never left without its final working tab.
                    let pane = fresh_pane.expect("last tab received a replacement pane");
                    tab.tree = LayoutTree::Leaf { pane };
                    tab.focused = pane;
                    tab.zoomed = false;
                    removed = Some(focused);
                }
            }
        }
        if let Some(id) = removed {
            self.panes
                .lock()
                .map_err(|_| anyhow!("pane lock poisoned"))?
                .remove(&id);
        }
        self.resize_current()
    }
}

fn sidebar_tab_info(
    tab: &Tab,
    panes: &HashMap<PaneId, Arc<Pane>>,
    detections: &HashMap<PaneId, agent::Detection>,
) -> SidebarTabInfo {
    let mut pane_ids = Vec::new();
    layout::leaves(&tab.tree, &mut pane_ids);
    SidebarTabInfo {
        id: tab.id,
        name: tab.name.clone(),
        state: tab_state(tab, detections),
        agents: pane_ids
            .into_iter()
            .filter_map(|pane| {
                let pane_ref = panes.get(&pane)?;
                let detection = detections.get(&pane)?;
                Some(AgentInfo {
                    pane,
                    name: detection.agent.clone().unwrap_or_else(|| {
                        pane_ref.title.lock().expect("title lock poisoned").clone()
                    }),
                    state: detection.state,
                })
            })
            .collect(),
    }
}

/// Focus a pane wherever it lives, activating its tab and workspace first.
fn focus_pane_id(state: &mut SessionState, pane: PaneId) -> bool {
    let location = state
        .workspaces
        .iter()
        .enumerate()
        .find_map(|(workspace_index, workspace)| {
            workspace
                .tabs
                .iter()
                .position(|tab| layout::contains(&tab.tree, pane))
                .map(|tab_index| (workspace_index, tab_index))
        });
    let Some((workspace_index, tab_index)) = location else {
        return false;
    };
    let workspace_id = state.workspaces[workspace_index].id;
    {
        let workspace = &mut state.workspaces[workspace_index];
        workspace.active_tab = workspace.tabs[tab_index].id;
        workspace.tabs[tab_index].focused = pane;
    }
    state.active_workspace = workspace_id;
    true
}

impl Pane {
    fn spawn(title: &str, cols: u16, rows: u16, updates: broadcast::Sender<()>) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        let spawn_process = Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sh")
            .to_owned();
        let mut command = CommandBuilder::new(&shell);
        command.arg("-l");
        pair.slave
            .spawn_command(command)
            .context("spawn login shell in PTY")?;
        let writer = pair.master.take_writer()?;
        let reader = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let last_output = Arc::new(Mutex::new(Instant::now()));
        read_pty(
            reader,
            Arc::clone(&parser),
            Arc::clone(&last_output),
            updates,
        );
        Ok(Self {
            title: Mutex::new(title.into()),
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            parser,
            scroll_offset: Mutex::new(0),
            last_output,
            hook: Mutex::new(None),
            spawn_process,
            process: Mutex::new(ProcessEvidence {
                name: None,
                checked_at: Instant::now() - Duration::from_secs(2),
            }),
        })
    }
    fn snapshot(&self) -> Screen {
        let mut offset = self
            .scroll_offset
            .lock()
            .expect("scroll offset lock poisoned");
        let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
        parser.set_scrollback(*offset);
        *offset = parser.screen().scrollback();
        snapshot(&parser)
    }
    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .lock()
            .map_err(|_| anyhow!("PTY master lock poisoned"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        self.parser
            .lock()
            .map_err(|_| anyhow!("PTY parser lock poisoned"))?
            .set_size(rows, cols);
        Ok(())
    }
    fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow!("PTY writer lock poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }
    fn scroll(&self, delta: i16) {
        let mut offset = self
            .scroll_offset
            .lock()
            .expect("scroll offset lock poisoned");
        let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
        *offset = scroll_offset_after_delta(*offset, delta, usize::MAX);
        parser.set_scrollback(*offset);
        *offset = parser.screen().scrollback();
    }
    fn reset_scrollback(&self) {
        let mut offset = self
            .scroll_offset
            .lock()
            .expect("scroll offset lock poisoned");
        *offset = 0;
        self.parser
            .lock()
            .expect("PTY parser lock poisoned")
            .set_scrollback(0);
    }
    fn scroll_offset(&self) -> usize {
        *self
            .scroll_offset
            .lock()
            .expect("scroll offset lock poisoned")
    }

    fn detect(&self, manifests: &[manifest::Manifest], now: Instant) -> agent::Detection {
        let (screen, title) = {
            let parser = self.parser.lock().expect("PTY parser lock poisoned");
            (
                parser.screen().contents(),
                parser.screen().title().to_owned(),
            )
        };
        // portable-pty obtains the foreground process-group leader from the PTY itself.
        // `ps` turns that portable pid into a basename without sysctl; unavailable leaders fall
        // back to the login-shell process captured at spawn, with OSC terminal title as evidence.
        let process = self.process_name(now);
        let hook = self
            .hook
            .lock()
            .expect("hook lock poisoned")
            .clone()
            .map(|hook| agent::HookState {
                state: hook.state,
                source: hook.source,
                age: now.saturating_duration_since(hook.reported_at),
            });
        let output_age =
            now.saturating_duration_since(*self.last_output.lock().expect("output lock poisoned"));
        agent::detect(
            manifests,
            process.as_deref().or(Some(&self.spawn_process)),
            &title,
            &screen,
            output_age,
            hook,
        )
    }

    fn process_name(&self, now: Instant) -> Option<String> {
        let mut process = self.process.lock().expect("process lock poisoned");
        if now.saturating_duration_since(process.checked_at) >= Duration::from_secs(2) {
            process.name = self
                .master
                .lock()
                .expect("PTY master lock poisoned")
                .process_group_leader()
                .and_then(|pid| {
                    let output = Command::new("ps")
                        .args(["-p", &pid.to_string(), "-o", "comm="])
                        .output()
                        .ok()?;
                    let path = String::from_utf8(output.stdout).ok()?;
                    Path::new(path.trim())
                        .file_name()?
                        .to_str()
                        .map(str::to_owned)
                });
            process.checked_at = now;
        }
        process.name.clone()
    }
}

fn tab_state(tab: &Tab, detections: &HashMap<PaneId, agent::Detection>) -> AgentStateKind {
    let mut panes = Vec::new();
    layout::leaves(&tab.tree, &mut panes);
    agent::rollup(
        panes
            .into_iter()
            .filter_map(|id| detections.get(&id).map(|item| item.state)),
    )
}

fn scroll_offset_after_delta(offset: usize, delta: i16, available: usize) -> usize {
    let next = if delta.is_negative() {
        offset.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        offset.saturating_add(delta as usize)
    };
    next.min(available)
}

/// PTY reading blocks, so parser ownership stays in Tokio's blocking pool.
fn read_pty(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    last_output: Arc<Mutex<Instant>>,
    updates: broadcast::Sender<()>,
) {
    tokio::task::spawn_blocking(move || {
        let mut bytes = [0_u8; 4096];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            parser
                .lock()
                .expect("PTY parser lock poisoned")
                .process(&bytes[..count]);
            *last_output.lock().expect("output lock poisoned") = Instant::now();
            let _ = updates.send(());
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
fn pane_sizes(tree: &LayoutTree, width: u16, height: u16, output: &mut Vec<(PaneId, u16, u16)>) {
    match tree {
        // The client draws a one-cell border on every side of each pane.
        LayoutTree::Leaf { pane } => output.push((
            *pane,
            width.saturating_sub(2).max(1),
            height.saturating_sub(2).max(1),
        )),
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => match axis {
            SplitAxis::Horizontal => {
                let first_width =
                    ((width as f32 * ratio) as u16).clamp(1, width.saturating_sub(1).max(1));
                pane_sizes(first, first_width, height, output);
                pane_sizes(second, width.saturating_sub(first_width), height, output);
            }
            SplitAxis::Vertical => {
                let first_height =
                    ((height as f32 * ratio) as u16).clamp(1, height.saturating_sub(1).max(1));
                pane_sizes(first, width, first_height, output);
                pane_sizes(second, width, height.saturating_sub(first_height), output);
            }
        },
    }
}

async fn serve_client(stream: UnixStream, session: Arc<Session>, name: String) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader).lines();
    let mut updates = session.updates.subscribe();
    let mut shutdown = session.shutdown.subscribe();
    let mut process_timer = tokio::time::interval(Duration::from_secs(2));
    process_timer.tick().await;
    let mut last_snapshot = Instant::now() - Duration::from_millis(16);
    let mut initialized = false;
    loop {
        tokio::select! {
            line = reader.next_line() => {
                let Some(line) = line? else { return Ok(()); };
                let message = decode::<ClientMessage>(line.as_bytes())?;
                let hello = matches!(message, ClientMessage::Hello { .. });
                let kill = matches!(message, ClientMessage::KillSession);
                match session.handle(message) {
                    Ok(()) if hello => {
                        initialized = true;
                        write_server(&mut writer, &ServerMessage::Welcome { session: name.clone() }).await?;
                        write_server(&mut writer, &ServerMessage::Layout(session.snapshot()?)).await?;
                    }
                    Ok(()) if kill => {
                        write_server(&mut writer, &ServerMessage::Shutdown).await?;
                        return Ok(());
                    }
                    Ok(()) => write_server(&mut writer, &ServerMessage::Layout(session.snapshot()?)).await?,
                    Err(error) => {
                        write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                        return Ok(());
                    }
                }
            }
            update = updates.recv() => match update {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !initialized { continue; }
                    // PTY output can be bursty; one newest snapshot per frame is enough.
                    while updates.try_recv().is_ok() {}
                    let remaining = Duration::from_millis(16).saturating_sub(last_snapshot.elapsed());
                    if !remaining.is_zero() { tokio::time::sleep(remaining).await; }
                    write_server(&mut writer, &ServerMessage::Layout(session.snapshot()?)).await?;
                    last_snapshot = Instant::now();
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = process_timer.tick() => {
                if initialized {
                    write_server(&mut writer, &ServerMessage::Layout(session.snapshot()?)).await?;
                    last_snapshot = Instant::now();
                }
            }
            _ = shutdown.recv() => {
                write_server(&mut writer, &ServerMessage::Shutdown).await?;
                return Ok(());
            },
        }
    }
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
    }
    #[test]
    fn pane_sizes_exclude_client_borders() {
        let mut sizes = Vec::new();
        pane_sizes(&LayoutTree::Leaf { pane: PaneId(1) }, 80, 24, &mut sizes);
        assert_eq!(sizes, vec![(PaneId(1), 78, 22)]);
    }
    #[test]
    fn scroll_offset_clamps_to_available_history() {
        assert_eq!(scroll_offset_after_delta(1, 99, 2), 2);
        assert_eq!(scroll_offset_after_delta(1, -99, 2), 0);
    }
    #[test]
    fn focus_pane_id_activates_its_workspace_and_tab() {
        let mut state = SessionState {
            active_workspace: WorkspaceId(1),
            next_id: 6,
            workspaces: vec![
                Workspace {
                    id: WorkspaceId(1),
                    name: "one".into(),
                    active_tab: TabId(2),
                    tabs: vec![Tab {
                        id: TabId(2),
                        name: "shell".into(),
                        tree: LayoutTree::Leaf { pane: PaneId(3) },
                        focused: PaneId(3),
                        zoomed: false,
                    }],
                },
                Workspace {
                    id: WorkspaceId(4),
                    name: "two".into(),
                    active_tab: TabId(5),
                    tabs: vec![Tab {
                        id: TabId(5),
                        name: "agents".into(),
                        tree: LayoutTree::Leaf { pane: PaneId(6) },
                        focused: PaneId(6),
                        zoomed: false,
                    }],
                },
            ],
        };
        assert!(focus_pane_id(&mut state, PaneId(6)));
        assert_eq!(state.active_workspace, WorkspaceId(4));
        assert_eq!(state.workspaces[1].active_tab, TabId(5));
        assert_eq!(state.workspaces[1].tabs[0].focused, PaneId(6));
        assert!(!focus_pane_id(&mut state, PaneId(99)));
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
            .expect("remove stale socket");
        assert!(!socket.exists());
        fs::remove_dir(&directory).expect("remove test directory");
    }
}
