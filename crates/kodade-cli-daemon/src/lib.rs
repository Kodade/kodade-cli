//! Persistent PTY host and session model for Ködade CLI.

mod agent;
mod layout;
mod manifest;
mod persist;
mod proc;

use std::{
    collections::HashMap,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{
    decode, encode, AgentInfo, AgentStateKind, CellColor, ClientMessage, Direction, LayoutSnapshot,
    LayoutTree, Notification, PaneId, PaneSnapshot, QueryKind, Run, Screen, ServerMessage,
    SidebarTabInfo, SplitAxis, TabId, TabInfo, WorkspaceId, WorkspaceInfo, ATTR_BOLD, ATTR_DIM,
    ATTR_INVERSE, ATTR_ITALIC, ATTR_UNDERLINE,
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

struct Session {
    name: String,
    state: Mutex<SessionState>,
    panes: Mutex<HashMap<PaneId, Arc<Pane>>>,
    updates: broadcast::Sender<()>,
    shutdown: broadcast::Sender<()>,
    size: Mutex<(u16, u16)>,
    manifests: Vec<manifest::Manifest>,
    /// Bumped by every layout-changing mutation (via `notify`), never by PTY
    /// output. The persist task watches this so scrollback churn is not saved.
    /// Kept on `Session` (not `SessionState`) so `notify` can bump it without
    /// re-locking state — several handlers call `notify` while holding the lock.
    layout_generation: AtomicU64,
    /// True from a cold restore until the first client `Hello`; surfaced as
    /// `LayoutSnapshot.restored` so `ls` can print `(restored)`.
    restored: AtomicBool,
    /// Ring of recent agent notifications (#10), newest last, capped at 64.
    /// Each attached client drains it by `seq` after its next snapshot.
    notifications: Mutex<Vec<Notification>>,
    /// Monotonic high-water mark for `Notification.seq`; also the id a freshly
    /// attached client uses so it never replays the backlog.
    notify_seq: AtomicU64,
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
    /// Directory new panes in this workspace fall back to (PRD §5.1).
    root: Option<PathBuf>,
}
struct Tab {
    id: TabId,
    name: String,
    tree: LayoutTree,
    focused: PaneId,
    zoomed: bool,
}
/// vt100 0.16 reports the OSC window title through callbacks instead of `Screen::title`.
#[derive(Default)]
struct PtyCallbacks {
    title: String,
}
impl vt100::Callbacks for PtyCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}
type PtyParser = vt100::Parser<PtyCallbacks>;

struct Pane {
    title: Mutex<String>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<PtyParser>>,
    scroll_offset: Mutex<usize>,
    last_output: Arc<Mutex<Instant>>,
    hook: Mutex<Option<ReportedHook>>,
    spawn_process: String,
    /// The command this pane was spawned with and the directory it started in,
    /// kept so persistence can record them without inspecting the live process.
    spawn_command: Option<Vec<String>>,
    spawn_cwd: Option<PathBuf>,
    process: Mutex<ProcessEvidence>,
    // Tracks how long the current detected state has held, for sidebar age labels.
    last_state: Mutex<Option<AgentStateKind>>,
    state_since: Mutex<Instant>,
}

#[derive(Clone)]
struct ReportedHook {
    state: AgentStateKind,
    source: String,
    reported_at: Instant,
}

struct ProcessEvidence {
    name: Option<String>,
    cwd: Option<PathBuf>,
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
    // Binding succeeded, so no live daemon owns this session: safe to restore.
    let session = Arc::new(load_session(&session_name)?);
    let mut shutdown = session.shutdown.subscribe();
    // Debounced layout persistence runs alongside the accept loop.
    tokio::spawn(persist_loop(Arc::clone(&session)));
    // Flush state on SIGTERM so a stopped daemon (e.g. logout) can be restored.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                // `kill-session` is deliberate: drop the state file so it does
                // not resurrect on the next cold start.
                persist::remove_session_file(&session_name);
                drop(listener);
                let _ = fs::remove_file(&socket);
                return Ok(());
            }
            _ = sigterm.recv() => {
                session.save();
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

/// Restore this session from its state file when one is present and valid; a
/// corrupt or foreign file is moved aside and a clean session starts instead.
fn load_session(name: &str) -> Result<Session> {
    if let Some(path) = persist::session_file_path(name) {
        match persist::read_session_file(&path) {
            Ok(Some(file)) => {
                let resume_agents = persist::resume_agents_setting();
                return Session::restore(file, resume_agents);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "Ködade CLI could not read state file {}: {error:#} — starting clean",
                    path.display()
                );
                persist::quarantine(&path);
            }
        }
    }
    Session::spawn(80, 24, name.to_owned())
}

/// Watch the update stream and persist the layout, debounced, whenever a real
/// mutation (not PTY output) advances the generation counter.
async fn persist_loop(session: Arc<Session>) {
    let mut updates = session.updates.subscribe();
    let mut last_saved = session.layout_generation.load(Ordering::Relaxed);
    loop {
        match updates.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
        if session.layout_generation.load(Ordering::Relaxed) == last_saved {
            continue; // PTY output or a no-op tick: nothing new to persist.
        }
        // Coalesce a burst of layout changes into a single write.
        tokio::time::sleep(persist::DEBOUNCE).await;
        session.save();
        last_saved = session.layout_generation.load(Ordering::Relaxed);
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
    fn spawn(cols: u16, rows: u16, name: String) -> Result<Self> {
        let (updates, _) = broadcast::channel(64);
        let (shutdown, _) = broadcast::channel(16);
        let session = Self {
            name,
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
            layout_generation: AtomicU64::new(0),
            restored: AtomicBool::new(false),
            notifications: Mutex::new(Vec::new()),
            notify_seq: AtomicU64::new(0),
        };
        let pane = session.new_pane("shell", None, None)?;
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
                root: None,
            });
        Ok(session)
    }

    /// Rebuild a session from a persisted file: fresh panes spawned in each saved
    /// cwd (falling back to the workspace root, then home), ids re-allocated so
    /// they never collide with a stale file. `resume_agents` re-runs an agent's
    /// resume command in place of the raw one. Trees are validated by the caller.
    fn restore(file: persist::SessionFile, resume_agents: bool) -> Result<Self> {
        let (updates, _) = broadcast::channel(64);
        let (shutdown, _) = broadcast::channel(16);
        let session = Self {
            name: file.name.clone(),
            state: Mutex::new(SessionState {
                workspaces: Vec::new(),
                active_workspace: WorkspaceId(1),
                next_id: 0,
            }),
            panes: Mutex::new(HashMap::new()),
            updates,
            shutdown,
            size: Mutex::new((80, 24)),
            manifests: manifest::load()?,
            layout_generation: AtomicU64::new(0),
            restored: AtomicBool::new(true),
            notifications: Mutex::new(Vec::new()),
            notify_seq: AtomicU64::new(0),
        };
        let mut workspaces = Vec::new();
        let mut workspace_ids: HashMap<u64, WorkspaceId> = HashMap::new();
        for saved in &file.workspaces {
            let mut tabs = Vec::new();
            let mut tab_ids: HashMap<u64, TabId> = HashMap::new();
            for saved_tab in &saved.tabs {
                // Spawn a fresh pane per saved pane, mapping old id -> new id so
                // the tree and focus can be remapped afterwards.
                let mut pane_ids: HashMap<PaneId, PaneId> = HashMap::new();
                for saved_pane in &saved_tab.panes {
                    let cwd = restore_cwd(saved_pane.cwd.clone(), saved.root.clone());
                    let command = resume_command(saved_pane, resume_agents, &session.manifests);
                    let new_id = session.new_pane(&saved_pane.title, cwd, command)?;
                    pane_ids.insert(PaneId(saved_pane.id), new_id);
                }
                let tree = remap_tree(&saved_tab.tree, &pane_ids);
                let mut leaves = Vec::new();
                layout::leaves(&tree, &mut leaves);
                let focused = pane_ids
                    .get(&PaneId(saved_tab.focused))
                    .copied()
                    .unwrap_or_else(|| leaves[0]);
                let new_tab_id = session.tab_id();
                tab_ids.insert(saved_tab.id, new_tab_id);
                tabs.push(Tab {
                    id: new_tab_id,
                    name: saved_tab.name.clone(),
                    tree,
                    focused,
                    zoomed: saved_tab.zoomed,
                });
            }
            let new_workspace_id = session.workspace_id();
            let active_tab = tab_ids
                .get(&saved.active_tab)
                .copied()
                .unwrap_or_else(|| tabs[0].id);
            workspace_ids.insert(saved.id, new_workspace_id);
            workspaces.push(Workspace {
                id: new_workspace_id,
                name: saved.name.clone(),
                tabs,
                active_tab,
                root: saved.root.clone(),
            });
        }
        let active_workspace = workspace_ids
            .get(&file.active_workspace)
            .copied()
            .unwrap_or_else(|| workspaces[0].id);
        {
            let mut state = session.state.lock().expect("state lock poisoned");
            state.workspaces = workspaces;
            state.active_workspace = active_workspace;
        }
        Ok(session)
    }

    /// Snapshot the current layout into a serializable [`persist::SessionFile`].
    /// Reads live pane titles and cached cwds; never blocks on process lookups.
    fn build_file(&self) -> persist::SessionFile {
        let state = self.state.lock().expect("state lock poisoned");
        let panes = self.panes.lock().expect("pane lock poisoned");
        let workspaces = state
            .workspaces
            .iter()
            .map(|workspace| persist::WorkspaceFile {
                id: workspace.id.0,
                name: workspace.name.clone(),
                root: workspace.root.clone(),
                active_tab: workspace.active_tab.0,
                tabs: workspace
                    .tabs
                    .iter()
                    .map(|tab| {
                        let mut ids = Vec::new();
                        layout::leaves(&tab.tree, &mut ids);
                        let pane_files = ids
                            .into_iter()
                            .filter_map(|id| {
                                panes.get(&id).map(|pane| persist::PaneFile {
                                    id: id.0,
                                    title: pane.title.lock().expect("title lock poisoned").clone(),
                                    cwd: pane.saved_cwd(),
                                    command: pane.spawn_command.clone(),
                                })
                            })
                            .collect();
                        persist::TabFile {
                            id: tab.id.0,
                            name: tab.name.clone(),
                            zoomed: tab.zoomed,
                            focused: tab.focused.0,
                            tree: tab.tree.clone(),
                            panes: pane_files,
                        }
                    })
                    .collect(),
            })
            .collect();
        persist::SessionFile {
            version: 1,
            name: self.name.clone(),
            active_workspace: state.active_workspace.0,
            workspaces,
        }
    }

    /// Persist the current layout to this session's state file. Best-effort:
    /// errors are logged, never propagated to the client-facing loop.
    fn save(&self) {
        let Some(path) = persist::session_file_path(&self.name) else {
            return;
        };
        let file = self.build_file();
        if let Err(error) = persist::write_session_file(&path, &file) {
            eprintln!(
                "Ködade CLI could not persist session '{}': {error:#}",
                self.name
            );
        }
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

    fn new_pane(
        &self,
        title: &str,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
    ) -> Result<PaneId> {
        let id = self.pane_id();
        let (cols, rows) = *self.size.lock().expect("size lock poisoned");
        let pane = Arc::new(Pane::spawn(
            id,
            title,
            cols,
            rows,
            self.name.clone(),
            self.updates.clone(),
            cwd,
            command,
        )?);
        self.panes
            .lock()
            .expect("pane lock poisoned")
            .insert(id, pane);
        Ok(id)
    }

    /// The cwd a new pane should inherit: the focused pane's live cwd, then the
    /// workspace root. `explicit` short-circuits both. Never holds the state or
    /// pane lock while `lsof`/`ps` run.
    fn inherit_cwd(
        &self,
        workspace: WorkspaceId,
        tab: Option<TabId>,
        explicit: Option<PathBuf>,
    ) -> Option<PathBuf> {
        if explicit.is_some() {
            return explicit;
        }
        let (focused, root) = {
            let state = self.state.lock().expect("state lock poisoned");
            let workspace = state.workspaces.iter().find(|item| item.id == workspace)?;
            let tab_id = tab.unwrap_or(workspace.active_tab);
            let focused = workspace
                .tabs
                .iter()
                .find(|item| item.id == tab_id)
                .map(|item| item.focused);
            (focused, workspace.root.clone())
        };
        let live = focused.and_then(|id| {
            let pane = self
                .panes
                .lock()
                .expect("pane lock poisoned")
                .get(&id)
                .cloned();
            pane.and_then(|pane| pane.cwd(Instant::now()))
        });
        live.or(root)
    }

    /// Handle `NewPane`: spawn into a workspace (default: active), either as a
    /// new tab (`split: None`) or by splitting a tab's focused pane. The new
    /// pane and its tab/workspace become active so the reply snapshot names it.
    fn new_pane_message(
        &self,
        workspace: Option<WorkspaceId>,
        tab: Option<TabId>,
        split: Option<SplitAxis>,
        command: Option<Vec<String>>,
        name: Option<String>,
    ) -> Result<()> {
        let target = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let target = workspace.unwrap_or(state.active_workspace);
            if !state.workspaces.iter().any(|item| item.id == target) {
                bail!("workspace {} not found", target.0);
            }
            target
        };
        let cwd = self.inherit_cwd(target, tab, None);
        let title = pane_title(name.as_deref(), command.as_deref());
        let pane = self.new_pane(&title, cwd, command)?;
        // Allocate the tab id up front; `tab_id` locks state and must not be
        // called while the guard below is held.
        let new_tab_id = self.tab_id();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?;
        state.active_workspace = target;
        let workspace = state
            .workspaces
            .iter_mut()
            .find(|item| item.id == target)
            .expect("target workspace exists");
        match split {
            Some(axis) => {
                // Split the requested tab when it exists, else the active one.
                let tab_id = tab
                    .filter(|id| workspace.tabs.iter().any(|item| item.id == *id))
                    .unwrap_or(workspace.active_tab);
                let tab_ref = workspace
                    .tabs
                    .iter_mut()
                    .find(|item| item.id == tab_id)
                    .expect("resolved tab exists");
                let focused = tab_ref.focused;
                layout::split(&mut tab_ref.tree, focused, axis, pane);
                tab_ref.focused = pane;
                workspace.active_tab = tab_id;
            }
            None => {
                workspace.tabs.push(Tab {
                    id: new_tab_id,
                    name: title,
                    tree: LayoutTree::Leaf { pane },
                    focused: pane,
                    zoomed: false,
                });
                workspace.active_tab = new_tab_id;
            }
        }
        drop(state);
        self.resize_current()
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
        // Every mutation funnels through here (directly or via `resize`), so this
        // is the one place the persist generation needs to advance. PTY output
        // sends on `updates` without calling `notify`, so it never bumps it.
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        let _ = self.updates.send(());
    }

    /// Clear the restored flag once a client has attached (`Hello`).
    fn clear_restored(&self) {
        self.restored.store(false, Ordering::Relaxed);
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
        // Age is tracked once per snapshot, after detection settles on a state.
        // The same pass spots transitions into blocked/done and queues a
        // notification once (track_state's mutation makes it idempotent across
        // the concurrent snapshot calls of every attached client).
        let mut ages = HashMap::new();
        for (id, pane) in panes.iter() {
            let detection = &detections[id];
            let previous = pane.last_state();
            let age = pane.track_state(detection.state, now);
            ages.insert(*id, age);
            let agent_known = detection.agent.is_some() || detection.from_hook;
            if should_notify(previous, detection.state, agent_known) {
                if let Some((workspace, tab)) = locate_pane(&state, *id) {
                    // Prefer the manifest display; a hook-only agent falls back to
                    // its pane title so the toast still names something useful.
                    let agent = detection
                        .agent
                        .clone()
                        .unwrap_or_else(|| pane.title.lock().expect("title lock poisoned").clone());
                    self.push_notification(*id, workspace, tab, agent, detection.state);
                }
            }
        }
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
                    state_age_secs: ages[&id],
                    // `detect` already refreshed this pane's cache this tick.
                    cwd: pane.cwd(now),
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
                        .map(|tab| sidebar_tab_info(tab, &panes, &detections, &ages))
                        .collect::<Vec<_>>();
                    WorkspaceInfo {
                        id: item.id,
                        name: item.name.clone(),
                        active: item.id == workspace.id,
                        state: agent::rollup(tabs.iter().map(|tab| tab.state)),
                        root: item.root.clone(),
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
            restored: self.restored.load(Ordering::Relaxed),
        })
    }

    /// Queues a notification with the next sequence number, capping the ring at
    /// 64 so a long-lived session never grows the queue without bound.
    fn push_notification(
        &self,
        pane: PaneId,
        workspace: WorkspaceId,
        tab: TabId,
        agent: String,
        state: AgentStateKind,
    ) {
        let seq = self.notify_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut queue = self.notifications.lock().expect("notify lock poisoned");
        queue.push(Notification {
            pane,
            workspace,
            tab,
            agent,
            state,
            seq,
        });
        let overflow = queue.len().saturating_sub(64);
        if overflow > 0 {
            queue.drain(0..overflow);
        }
    }

    /// Highest sequence handed out so far. A client records this at attach time
    /// so it only ever receives notifications raised after it connected.
    fn notify_high_water(&self) -> u64 {
        self.notify_seq.load(Ordering::Relaxed)
    }

    /// Notifications newer than `after`, oldest first.
    fn notifications_since(&self, after: u64) -> Vec<Notification> {
        self.notifications
            .lock()
            .expect("notify lock poisoned")
            .iter()
            .filter(|item| item.seq > after)
            .cloned()
            .collect()
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
                let active = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?
                    .active_workspace;
                let cwd = self.inherit_cwd(active, None, None);
                let pane = self.new_pane("shell", cwd, None)?;
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
            ClientMessage::CloseTab { id } => self.close_tab(id)?,
            ClientMessage::CloseWorkspace { id } => self.close_workspace(id)?,
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
                let active = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?
                    .active_workspace;
                let cwd = self.inherit_cwd(active, None, None);
                let pane = self.new_pane("shell", cwd, None)?;
                let id = self.tab_id();
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
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
            ClientMessage::NewPane {
                workspace,
                tab,
                split,
                command,
                name,
            } => self.new_pane_message(workspace, tab, split, command, name)?,
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
            ClientMessage::SelectTabIndex { index } => {
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
                // The wire index is one-based; out-of-range positions do nothing.
                if let Some(tab) = index
                    .checked_sub(1)
                    .and_then(|index| workspace.tabs.get(index as usize))
                {
                    workspace.active_tab = tab.id;
                }
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::MoveTab { delta } => {
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
                move_tab(workspace, delta);
                drop(state);
                self.notify();
            }
            ClientMessage::SwapPane { direction } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let tab = Self::active_tab_mut(&mut state);
                // Focus follows the pane, so only the tree changes.
                if let Some(target) = layout::focus_neighbor(&tab.tree, tab.focused, direction) {
                    let focused = tab.focused;
                    layout::swap(&mut tab.tree, focused, target);
                }
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::BreakPane => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                break_pane(&mut state);
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::EqualizeLayout => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                layout::equalize(&mut Self::active_tab_mut(&mut state).tree);
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::FocusPaneCycle { forward } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let tab = Self::active_tab_mut(&mut state);
                if !tab.zoomed {
                    if let Some(pane) = layout::cycle(&tab.tree, tab.focused, forward) {
                        tab.focused = pane;
                    }
                }
                self.notify();
            }
            ClientMessage::SelectWorkspaceDelta { delta } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                let count = state.workspaces.len();
                let index = state
                    .workspaces
                    .iter()
                    .position(|item| item.id == state.active_workspace)
                    .unwrap_or(0);
                // Wrap in both directions without going negative.
                let next = (index as isize + delta as isize).rem_euclid(count as isize) as usize;
                state.active_workspace = state.workspaces[next].id;
                drop(state);
                self.resize_current()?;
            }
            ClientMessage::RenameTabId { id, name } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                if let Some(tab) = state
                    .workspaces
                    .iter_mut()
                    .flat_map(|workspace| workspace.tabs.iter_mut())
                    .find(|tab| tab.id == id)
                {
                    tab.name = name;
                }
                drop(state);
                self.notify();
            }
            ClientMessage::RenameWorkspaceId { id, name } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                if let Some(workspace) = state.workspaces.iter_mut().find(|item| item.id == id) {
                    workspace.name = name;
                }
                drop(state);
                self.notify();
            }
            ClientMessage::NewWorkspace { name, root } => {
                // A workspace root seeds its first pane's cwd; later panes inherit.
                let pane = self.new_pane("shell", root.clone(), None)?;
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
                    root,
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
        let fresh_pane = needs_fresh
            .then(|| self.new_pane("shell", None, None))
            .transpose()?;
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

    fn close_tab(&self, id: TabId) -> Result<()> {
        let needs_fresh = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            state
                .workspaces
                .iter()
                .any(|workspace| workspace.tabs.len() == 1 && workspace.tabs[0].id == id)
        };
        let fresh = needs_fresh
            .then(|| self.new_pane("shell", None, None))
            .transpose()?;
        let fresh_tab = needs_fresh.then(|| self.tab_id());
        let pane_ids = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let workspace = state
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.tabs.iter().any(|tab| tab.id == id));
            let Some(workspace) = workspace else {
                return Ok(());
            };
            let index = workspace
                .tabs
                .iter()
                .position(|tab| tab.id == id)
                .expect("tab exists");
            let tab = workspace.tabs.remove(index);
            let mut ids = Vec::new();
            layout::leaves(&tab.tree, &mut ids);
            if workspace.tabs.is_empty() {
                let pane = fresh.expect("final tab receives a replacement pane");
                let tab_id = fresh_tab.expect("final tab receives a replacement tab");
                workspace.tabs.push(Tab {
                    id: tab_id,
                    name: "shell".into(),
                    tree: LayoutTree::Leaf { pane },
                    focused: pane,
                    zoomed: false,
                });
            }
            workspace.active_tab = workspace.tabs[index.saturating_sub(1)].id;
            ids
        };
        let mut panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        for id in pane_ids {
            panes.remove(&id);
        }
        drop(panes);
        self.resize_current()
    }

    fn close_workspace(&self, id: WorkspaceId) -> Result<()> {
        let needs_fresh = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            state.workspaces.len() == 1
                && state.workspaces.iter().any(|workspace| workspace.id == id)
        };
        let fresh = needs_fresh
            .then(|| self.new_pane("shell", None, None))
            .transpose()?;
        let fresh_tab = needs_fresh.then(|| self.tab_id());
        let pane_ids = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let index = match state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == id)
            {
                Some(index) => index,
                None => return Ok(()),
            };
            if state.workspaces.len() == 1 {
                let workspace = &mut state.workspaces[index];
                let old_tabs = std::mem::take(&mut workspace.tabs);
                let mut ids = Vec::new();
                for tab in old_tabs {
                    layout::leaves(&tab.tree, &mut ids);
                }
                let pane = fresh.expect("final workspace receives a replacement pane");
                let tab_id = fresh_tab.expect("final workspace receives a replacement tab");
                workspace.tabs.push(Tab {
                    id: tab_id,
                    name: "shell".into(),
                    tree: LayoutTree::Leaf { pane },
                    focused: pane,
                    zoomed: false,
                });
                workspace.active_tab = tab_id;
                ids
            } else {
                let workspace = state.workspaces.remove(index);
                let mut ids = Vec::new();
                for tab in workspace.tabs {
                    layout::leaves(&tab.tree, &mut ids);
                }
                state.active_workspace = state.workspaces[index.saturating_sub(1)].id;
                ids
            }
        };
        let mut panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        for id in pane_ids {
            panes.remove(&id);
        }
        drop(panes);
        self.resize_current()
    }
}

fn sidebar_tab_info(
    tab: &Tab,
    panes: &HashMap<PaneId, Arc<Pane>>,
    detections: &HashMap<PaneId, agent::Detection>,
    ages: &HashMap<PaneId, u64>,
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
                    state_age_secs: ages.get(&pane).copied().unwrap_or(0),
                })
            })
            .collect(),
    }
}

/// Reorder the active tab by `delta` positions, clamped to the ends.
fn move_tab(workspace: &mut Workspace, delta: i8) -> bool {
    let Some(index) = workspace
        .tabs
        .iter()
        .position(|tab| tab.id == workspace.active_tab)
    else {
        return false;
    };
    let last = workspace.tabs.len().saturating_sub(1);
    let target = (index as isize + delta as isize).clamp(0, last as isize) as usize;
    if target == index {
        return false;
    }
    let tab = workspace.tabs.remove(index);
    workspace.tabs.insert(target, tab);
    true
}

/// Move the focused pane into a new tab of its own, keeping its PTY alive.
/// A tab holding a single pane has nothing to break out, so it is a no-op.
fn break_pane(state: &mut SessionState) -> bool {
    let active = state.active_workspace;
    let Some(workspace) = state.workspaces.iter_mut().find(|item| item.id == active) else {
        return false;
    };
    let Some(index) = workspace
        .tabs
        .iter()
        .position(|tab| tab.id == workspace.active_tab)
    else {
        return false;
    };
    let source = &mut workspace.tabs[index];
    let pane = source.focused;
    let Some(tree) = layout::close(source.tree.clone(), pane) else {
        return false;
    };
    source.tree = tree;
    source.zoomed = false;
    let mut remaining = Vec::new();
    layout::leaves(&source.tree, &mut remaining);
    source.focused = remaining[0];
    let name = source.name.clone();
    state.next_id += 1;
    let id = TabId(state.next_id);
    let workspace = state
        .workspaces
        .iter_mut()
        .find(|item| item.id == active)
        .expect("active workspace exists");
    workspace.tabs.insert(
        index + 1,
        Tab {
            id,
            name,
            tree: LayoutTree::Leaf { pane },
            focused: pane,
            zoomed: false,
        },
    );
    workspace.active_tab = id;
    true
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
    // Panes carry a lot of spawn context (size, session, cwd, command); grouping
    // it into a struct would not make the single caller clearer.
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        id: PaneId,
        title: &str,
        cols: u16,
        rows: u16,
        session: String,
        updates: broadcast::Sender<()>,
        cwd: Option<PathBuf>,
        run: Option<Vec<String>>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        // With a command, the detection fallback name is the command basename;
        // otherwise it's the login shell's.
        let spawn_process = run
            .as_ref()
            .and_then(|args| args.first())
            .and_then(|arg| proc::process_basename(arg))
            .unwrap_or_else(|| {
                Path::new(&shell)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("sh")
                    .to_owned()
            });
        let mut command = CommandBuilder::new(&shell);
        command.arg("-l");
        // Commands run through the login shell so agent CLIs keep their env and
        // credentials handling; `exec` replaces the shell with the target.
        if let Some(args) = &run {
            command.arg("-c");
            command.arg(format!("exec {}", proc::shell_command(args)));
        }
        if let Some(dir) = &cwd {
            command.cwd(dir);
        }
        command.env("KODADE_PANE", id.0.to_string());
        command.env("KODADE_SESSION", session);
        pair.slave
            .spawn_command(command)
            .context("spawn login shell in PTY")?;
        let writer = pair.master.take_writer()?;
        let reader = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(PtyParser::new_with_callbacks(
            rows,
            cols,
            10_000,
            PtyCallbacks::default(),
        )));
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
            spawn_command: run,
            spawn_cwd: cwd,
            process: Mutex::new(ProcessEvidence {
                name: None,
                cwd: None,
                checked_at: Instant::now() - Duration::from_secs(2),
            }),
            last_state: Mutex::new(None),
            state_since: Mutex::new(Instant::now()),
        })
    }
    fn snapshot(&self) -> Screen {
        let mut offset = self
            .scroll_offset
            .lock()
            .expect("scroll offset lock poisoned");
        let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
        parser.screen_mut().set_scrollback(*offset);
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
            .screen_mut()
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
        parser.screen_mut().set_scrollback(*offset);
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
            .screen_mut()
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
            (parser.screen().contents(), parser.callbacks().title.clone())
        };
        // portable-pty obtains the foreground process-group leader from the PTY itself.
        // `ps` turns that portable pid into a basename without sysctl; unavailable leaders fall
        // back to the login-shell process captured at spawn, with OSC terminal title as evidence.
        let process = self.process_name(now);
        let last_output = *self.last_output.lock().expect("output lock poisoned");
        let hook = self
            .hook
            .lock()
            .expect("hook lock poisoned")
            .clone()
            .map(|hook| agent::HookState {
                state: hook.state,
                source: hook.source,
                age: now.saturating_duration_since(hook.reported_at),
                // A `done` report is released once the pane prints anything new.
                output_since_report: last_output > hook.reported_at,
            });
        let output_age = now.saturating_duration_since(last_output);
        agent::detect(
            manifests,
            process.as_deref().or(Some(&self.spawn_process)),
            &title,
            &screen,
            output_age,
            hook,
        )
    }

    /// The last state `track_state` recorded, or `None` before the first
    /// detection. Read before `track_state` so a transition can be spotted.
    fn last_state(&self) -> Option<AgentStateKind> {
        *self.last_state.lock().expect("state lock poisoned")
    }

    /// Records a state transition and returns how many seconds the current state
    /// has held. `state_since` only resets when the detected state actually changes.
    fn track_state(&self, state: AgentStateKind, now: Instant) -> u64 {
        let mut last = self.last_state.lock().expect("state lock poisoned");
        let mut since = self.state_since.lock().expect("state_since lock poisoned");
        *since = state_since_after(*last, state, *since, now);
        *last = Some(state);
        now.saturating_duration_since(*since).as_secs()
    }

    fn process_name(&self, now: Instant) -> Option<String> {
        self.refresh_process(now);
        self.process
            .lock()
            .expect("process lock poisoned")
            .name
            .clone()
    }

    fn cwd(&self, now: Instant) -> Option<PathBuf> {
        self.refresh_process(now);
        self.process
            .lock()
            .expect("process lock poisoned")
            .cwd
            .clone()
    }

    /// Best cwd for persistence without forcing a fresh `ps`/`lsof`: the last
    /// cached live cwd, falling back to the directory the pane was spawned in.
    fn saved_cwd(&self) -> Option<PathBuf> {
        self.process
            .lock()
            .expect("process lock poisoned")
            .cwd
            .clone()
            .or_else(|| self.spawn_cwd.clone())
    }

    /// Refresh the cached foreground-process name and cwd at most once per 2 s.
    /// The pid comes from the PTY's process-group leader; one `ps` and one
    /// `lsof` (or one procfs read) per pane per tick.
    fn refresh_process(&self, now: Instant) {
        let mut process = self.process.lock().expect("process lock poisoned");
        if now.saturating_duration_since(process.checked_at) < Duration::from_secs(2) {
            return;
        }
        let pid = self
            .master
            .lock()
            .expect("PTY master lock poisoned")
            .process_group_leader();
        if let Some(pid) = pid {
            process.name = proc::command_of(pid)
                .as_deref()
                .and_then(proc::process_basename);
            process.cwd = proc::cwd_of(pid);
        } else {
            process.name = None;
            process.cwd = None;
        }
        process.checked_at = now;
    }
}

/// Directory a restored pane should start in: its saved cwd if it still exists,
/// else the workspace root, else the pane spawn's own default (home / `$SHELL`).
fn restore_cwd(saved: Option<PathBuf>, root: Option<PathBuf>) -> Option<PathBuf> {
    saved
        .filter(|path| path.is_dir())
        .or_else(|| root.filter(|path| path.is_dir()))
}

/// The command a restored pane should run. Only when `resume_agents` is on and
/// the saved command's program matches a manifest that defines a `resume` string
/// do we relaunch it — with the manifest's resume command, not the raw one.
/// Everything else restores as a plain shell (no command re-run).
fn resume_command(
    pane: &persist::PaneFile,
    resume_agents: bool,
    manifests: &[manifest::Manifest],
) -> Option<Vec<String>> {
    if !resume_agents {
        return None;
    }
    let command = pane.command.as_ref()?;
    let process = command.first().and_then(|arg| proc::process_basename(arg));
    let manifest = manifests.iter().find(|manifest| {
        manifest.resume.is_some() && manifest.identifies(process.as_deref(), "")
    })?;
    let resume = manifest.resume.as_ref()?;
    // Resume strings are simple shell words (e.g. `codex resume --last`).
    Some(resume.split_whitespace().map(str::to_owned).collect())
}

/// Rebuild a layout tree with re-allocated pane ids. The caller validated that
/// every leaf has a mapping, so a missing id would be a bug, not bad input.
fn remap_tree(tree: &LayoutTree, ids: &HashMap<PaneId, PaneId>) -> LayoutTree {
    match tree {
        LayoutTree::Leaf { pane } => LayoutTree::Leaf {
            pane: *ids.get(pane).expect("validated leaf has a remapped id"),
        },
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => LayoutTree::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(remap_tree(first, ids)),
            second: Box::new(remap_tree(second, ids)),
        },
    }
}

/// Title for a new pane: the explicit name, else the run command's basename,
/// else the interactive shell.
fn pane_title(name: Option<&str>, command: Option<&[String]>) -> String {
    if let Some(name) = name {
        return name.to_owned();
    }
    command
        .and_then(|args| args.first())
        .and_then(|arg| proc::process_basename(arg))
        .unwrap_or_else(|| "shell".to_owned())
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

/// Whether a state change should raise a notification (#10). Only genuine
/// transitions into `blocked`/`done` for a pane with a known agent qualify; the
/// initial detection at spawn (`last` is `None`) and same-state ticks never do.
fn should_notify(last: Option<AgentStateKind>, next: AgentStateKind, agent_known: bool) -> bool {
    agent_known
        && matches!(next, AgentStateKind::Blocked | AgentStateKind::Done)
        && last.is_some()
        && last != Some(next)
}

/// Finds the workspace and tab that currently own `pane`, if any.
fn locate_pane(state: &SessionState, pane: PaneId) -> Option<(WorkspaceId, TabId)> {
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            if layout::contains(&tab.tree, pane) {
                return Some((workspace.id, tab.id));
            }
        }
    }
    None
}

/// `state_since` moves to `now` only when the detected state changes; an
/// unchanged state keeps its original start so the age keeps growing.
fn state_since_after(
    last: Option<AgentStateKind>,
    next: AgentStateKind,
    since: Instant,
    now: Instant,
) -> Instant {
    if last == Some(next) {
        since
    } else {
        now
    }
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
    parser: Arc<Mutex<PtyParser>>,
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
/// Build a wire `Screen` from the pane's terminal state: plain `contents` for
/// copy mode plus one styled run list per visible row (#7).
fn snapshot(parser: &PtyParser) -> Screen {
    let screen = parser.screen();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let (rows, cols) = screen.size();
    Screen {
        contents: screen.contents(),
        cursor_row,
        cursor_col,
        cursor_visible: !screen.hide_cursor(),
        rows: (0..rows).map(|row| screen_row(screen, row, cols)).collect(),
        bracketed_paste: screen.bracketed_paste(),
        mouse_reporting: screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None,
    }
}

/// Coalesce one screen row into runs of identically styled cells. Empty cells
/// become spaces so column positions survive the trip; a wide char is emitted
/// once and its continuation cell skipped.
fn screen_row(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<Run> {
    // Most rows are one or two styles wide; reserve small and grow rarely.
    let mut runs: Vec<Run> = Vec::with_capacity(4);
    let mut style: Option<(CellColor, CellColor, u8)> = None;
    let mut col = 0;
    while col < cols {
        let Some(cell) = screen.cell(row, col) else {
            break;
        };
        if cell.is_wide_continuation() {
            col += 1;
            continue;
        }
        let next = (
            cell_color(cell.fgcolor()),
            cell_color(cell.bgcolor()),
            cell_attrs(cell),
        );
        if style != Some(next) {
            runs.push(Run {
                text: String::new(),
                fg: next.0,
                bg: next.1,
                attrs: next.2,
            });
            style = Some(next);
        }
        let text = &mut runs.last_mut().expect("run pushed above").text;
        if cell.has_contents() {
            text.push_str(cell.contents());
        } else {
            text.push(' ');
        }
        col += if cell.is_wide() { 2 } else { 1 };
    }
    // Trailing unstyled blanks cost bytes and draw nothing; trim them.
    if let Some(last) = runs.last_mut() {
        if last.fg == CellColor::Default && last.bg == CellColor::Default && last.attrs == 0 {
            last.text.truncate(last.text.trim_end_matches(' ').len());
            if last.text.is_empty() {
                runs.pop();
            }
        }
    }
    runs
}

fn cell_color(color: vt100::Color) -> CellColor {
    match color {
        vt100::Color::Default => CellColor::Default,
        vt100::Color::Idx(index) => CellColor::Indexed(index),
        vt100::Color::Rgb(r, g, b) => CellColor::Rgb(r, g, b),
    }
}

fn cell_attrs(cell: &vt100::Cell) -> u8 {
    let mut attrs = 0;
    if cell.bold() {
        attrs |= ATTR_BOLD;
    }
    if cell.italic() {
        attrs |= ATTR_ITALIC;
    }
    if cell.underline() {
        attrs |= ATTR_UNDERLINE;
    }
    if cell.dim() {
        attrs |= ATTR_DIM;
    }
    if cell.inverse() {
        attrs |= ATTR_INVERSE;
    }
    attrs
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
    // A fresh client only hears about transitions raised after it attached, so
    // the spawn-time backlog never replays.
    let mut last_notify_seq = session.notify_high_water();
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
                        // The first client attach sees `restored: true`; clear it
                        // afterward so later snapshots (and `ls`) report normally.
                        write_server(&mut writer, &ServerMessage::Layout(session.snapshot()?)).await?;
                        send_notifications(&mut writer, &session, &mut last_notify_seq).await?;
                        session.clear_restored();
                    }
                    Ok(()) if kill => {
                        write_server(&mut writer, &ServerMessage::Shutdown).await?;
                        return Ok(());
                    }
                    Ok(()) => {
                        write_server(&mut writer, &ServerMessage::Layout(session.snapshot()?)).await?;
                        send_notifications(&mut writer, &session, &mut last_notify_seq).await?;
                    }
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
                    send_notifications(&mut writer, &session, &mut last_notify_seq).await?;
                    last_snapshot = Instant::now();
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = process_timer.tick() => {
                if initialized {
                    write_server(&mut writer, &ServerMessage::Layout(session.snapshot()?)).await?;
                    send_notifications(&mut writer, &session, &mut last_notify_seq).await?;
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

/// Flushes any notifications this client has not seen yet, always after a fresh
/// snapshot so the client can resolve workspace/tab names from it.
async fn send_notifications(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    session: &Arc<Session>,
    last_seq: &mut u64,
) -> Result<()> {
    for notification in session.notifications_since(*last_seq) {
        *last_seq = (*last_seq).max(notification.seq);
        write_server(writer, &ServerMessage::Notification(notification)).await?;
    }
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
        let mut parser = PtyParser::new_with_callbacks(3, 10, 100, PtyCallbacks::default());
        parser.process(b"hello\r\nworld");
        assert!(snapshot(&parser).contents.contains("hello"));
    }
    /// Paint an 80x24-style sample with the escape sequences a colored `ls`,
    /// a prompt, and a 256/RGB-color TUI would emit.
    fn mixed_sample(rows: u16, cols: u16) -> PtyParser {
        let mut parser = PtyParser::new_with_callbacks(rows, cols, 100, PtyCallbacks::default());
        for row in 0..rows {
            let line = match row % 4 {
                0 => format!("\x1b[0;34mdir-{row:03}\x1b[0m  \x1b[0;32mrun.sh\x1b[0m  plain.txt"),
                1 => format!("\x1b[1mbold header {row}\x1b[0m normal tail"),
                2 => {
                    format!("\x1b[38;5;208m256-color {row}\x1b[0m \x1b[3;4mitalic underline\x1b[0m")
                }
                _ => format!("\x1b[38;2;120;200;80mrgb {row}\x1b[0m \x1b[7minverse\x1b[0m done"),
            };
            // Repeat each pattern so the row is styled edge to edge (worst case
            // for run counts), then let vt100 clip at the right margin.
            for _ in 0..cols.div_ceil(40) {
                parser.process(line.as_bytes());
            }
            parser.process(b"\r\n");
        }
        parser
    }

    #[test]
    fn snapshot_coalesces_colors_and_attributes_into_runs() {
        let mut parser = PtyParser::new_with_callbacks(2, 20, 100, PtyCallbacks::default());
        parser.process(b"\x1b[31mred\x1b[1mbold\x1b[0mplain");
        let screen = snapshot(&parser);
        let runs = &screen.rows[0];
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text, "red");
        assert_eq!(runs[0].fg, CellColor::Indexed(1));
        assert_eq!(runs[0].attrs, 0);
        assert_eq!(runs[1].text, "bold");
        assert_eq!(runs[1].attrs, ATTR_BOLD);
        assert_eq!(runs[2].text, "plain");
        assert_eq!(runs[2].fg, CellColor::Default);
        // Trailing unstyled blanks are trimmed, and the cursor rides along.
        assert!(screen.cursor_visible);
        assert_eq!(screen.rows[1], Vec::<Run>::new());
    }

    #[test]
    fn snapshot_keeps_wide_chars_in_two_columns() {
        let mut parser = PtyParser::new_with_callbacks(1, 10, 100, PtyCallbacks::default());
        parser.process("宽x".as_bytes());
        let screen = snapshot(&parser);
        let text: String = screen.rows[0].iter().map(|run| run.text.as_str()).collect();
        assert_eq!(text, "宽x");
        // Column 1 is the wide continuation cell, so `x` sits at column 2.
        assert_eq!(screen.cursor_col, 3);
    }

    #[test]
    fn snapshot_reports_terminal_modes() {
        let mut parser = PtyParser::new_with_callbacks(2, 10, 100, PtyCallbacks::default());
        parser.process(b"\x1b[?2004h\x1b[?1000h\x1b[?25l");
        let screen = snapshot(&parser);
        assert!(screen.bracketed_paste);
        assert!(screen.mouse_reporting);
        assert!(!screen.cursor_visible);
    }

    #[test]
    fn styled_snapshots_stay_within_size_budgets() {
        // proto::encode is the same serde_json path the socket uses.
        let small = encode(&snapshot(&mixed_sample(24, 80)))
            .expect("snapshot serializes")
            .len();
        let large = encode(&snapshot(&mixed_sample(60, 200)))
            .expect("snapshot serializes")
            .len();
        assert!(small < 20_000, "80x24 snapshot was {small} bytes");
        assert!(large < 100_000, "200x60 snapshot was {large} bytes");
    }

    #[test]
    fn building_snapshots_stays_linear() {
        let parser = mixed_sample(60, 200);
        let started = Instant::now();
        for _ in 0..100 {
            let screen = snapshot(&parser);
            assert_eq!(screen.rows.len(), 60);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "100 snapshots took {elapsed:?}"
        );
    }

    #[test]
    fn osc_window_title_reaches_the_detection_callback() {
        let mut parser = PtyParser::new_with_callbacks(3, 20, 100, PtyCallbacks::default());
        parser.process(b"\x1b]2;claude\x07");
        assert_eq!(parser.callbacks().title, "claude");
        parser.process(b"\x1b]0;codex\x07");
        assert_eq!(parser.callbacks().title, "codex");
    }
    #[test]
    fn pane_sizes_exclude_client_borders() {
        let mut sizes = Vec::new();
        pane_sizes(&LayoutTree::Leaf { pane: PaneId(1) }, 80, 24, &mut sizes);
        assert_eq!(sizes, vec![(PaneId(1), 78, 22)]);
    }
    #[test]
    fn state_since_resets_only_on_change() {
        let start = Instant::now();
        let later = start + Duration::from_secs(5);
        // Same state: the original start time is retained (age keeps growing).
        assert_eq!(
            state_since_after(
                Some(AgentStateKind::Working),
                AgentStateKind::Working,
                start,
                later
            ),
            start
        );
        // Changed state: the clock restarts at `now`.
        assert_eq!(
            state_since_after(
                Some(AgentStateKind::Working),
                AgentStateKind::Blocked,
                start,
                later
            ),
            later
        );
        // First observation restarts as well.
        assert_eq!(
            state_since_after(None, AgentStateKind::Idle, start, later),
            later
        );
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
                    root: None,
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
                    root: None,
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
    /// One workspace, one tab, two panes split side by side.
    fn split_state() -> SessionState {
        SessionState {
            active_workspace: WorkspaceId(1),
            next_id: 4,
            workspaces: vec![Workspace {
                id: WorkspaceId(1),
                name: "one".into(),
                active_tab: TabId(2),
                root: None,
                tabs: vec![Tab {
                    id: TabId(2),
                    name: "agents".into(),
                    tree: LayoutTree::Split {
                        axis: SplitAxis::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutTree::Leaf { pane: PaneId(3) }),
                        second: Box::new(LayoutTree::Leaf { pane: PaneId(4) }),
                    },
                    focused: PaneId(4),
                    zoomed: false,
                }],
            }],
        }
    }

    #[test]
    fn break_pane_moves_the_focused_leaf_into_a_new_tab() {
        let mut state = split_state();
        assert!(break_pane(&mut state));
        let workspace = &state.workspaces[0];
        assert_eq!(workspace.tabs.len(), 2);
        // Source tab keeps the remaining pane and focus lands on it.
        assert_eq!(workspace.tabs[0].tree, LayoutTree::Leaf { pane: PaneId(3) });
        assert_eq!(workspace.tabs[0].focused, PaneId(3));
        // New tab holds the broken-out pane and becomes active.
        assert_eq!(workspace.tabs[1].tree, LayoutTree::Leaf { pane: PaneId(4) });
        assert_eq!(workspace.active_tab, workspace.tabs[1].id);
        // A single-pane tab has nothing to break out.
        assert!(!break_pane(&mut state));
    }

    #[test]
    fn move_tab_reorders_and_clamps() {
        let mut state = split_state();
        let workspace = &mut state.workspaces[0];
        workspace.tabs.push(Tab {
            id: TabId(9),
            name: "second".into(),
            tree: LayoutTree::Leaf { pane: PaneId(9) },
            focused: PaneId(9),
            zoomed: false,
        });
        assert!(move_tab(workspace, 1));
        assert_eq!(workspace.tabs[1].id, TabId(2));
        // Already at the end: clamped, so nothing moves.
        assert!(!move_tab(workspace, 1));
        assert!(move_tab(workspace, -1));
        assert_eq!(workspace.tabs[0].id, TabId(2));
    }

    /// A persisted 2-workspace / 3-tab / 4-pane layout with `/tmp` and `/` cwds.
    fn restore_fixture() -> persist::SessionFile {
        persist::SessionFile {
            version: 1,
            name: "restored".into(),
            active_workspace: 11,
            workspaces: vec![
                persist::WorkspaceFile {
                    id: 10,
                    name: "one".into(),
                    root: Some(PathBuf::from("/tmp")),
                    active_tab: 20,
                    tabs: vec![
                        persist::TabFile {
                            id: 20,
                            name: "agents".into(),
                            zoomed: false,
                            focused: 31,
                            tree: LayoutTree::Split {
                                axis: SplitAxis::Horizontal,
                                ratio: 0.5,
                                first: Box::new(LayoutTree::Leaf { pane: PaneId(30) }),
                                second: Box::new(LayoutTree::Leaf { pane: PaneId(31) }),
                            },
                            panes: vec![
                                persist::PaneFile {
                                    id: 30,
                                    title: "codex".into(),
                                    cwd: Some(PathBuf::from("/tmp")),
                                    command: Some(vec!["codex".into()]),
                                },
                                persist::PaneFile {
                                    id: 31,
                                    title: "root".into(),
                                    cwd: Some(PathBuf::from("/")),
                                    command: None,
                                },
                            ],
                        },
                        persist::TabFile {
                            id: 21,
                            name: "logs".into(),
                            zoomed: true,
                            focused: 32,
                            tree: LayoutTree::Leaf { pane: PaneId(32) },
                            panes: vec![persist::PaneFile {
                                id: 32,
                                title: "tail".into(),
                                cwd: None,
                                command: None,
                            }],
                        },
                    ],
                },
                persist::WorkspaceFile {
                    id: 11,
                    name: "two".into(),
                    root: None,
                    active_tab: 22,
                    tabs: vec![persist::TabFile {
                        id: 22,
                        name: "shell".into(),
                        zoomed: false,
                        focused: 33,
                        tree: LayoutTree::Leaf { pane: PaneId(33) },
                        panes: vec![persist::PaneFile {
                            id: 33,
                            title: "shell".into(),
                            cwd: Some(PathBuf::from("/tmp")),
                            command: None,
                        }],
                    }],
                },
            ],
        }
    }

    #[tokio::test]
    async fn restore_rebuilds_names_trees_and_cwds() {
        let session = Session::restore(restore_fixture(), false).expect("restore");
        let state = session.state.lock().expect("state lock");
        // Workspaces and tabs come back by name, in order.
        assert_eq!(state.workspaces.len(), 2);
        assert_eq!(state.workspaces[0].name, "one");
        assert_eq!(state.workspaces[1].name, "two");
        assert_eq!(state.workspaces[0].root, Some(PathBuf::from("/tmp")));
        assert_eq!(state.workspaces[0].tabs.len(), 2);
        assert_eq!(state.workspaces[0].tabs[0].name, "agents");
        // Zoom survives the round-trip.
        assert!(state.workspaces[0].tabs[1].zoomed);
        // Tree shapes are preserved: a split of two leaves, then single leaves.
        assert!(matches!(
            state.workspaces[0].tabs[0].tree,
            LayoutTree::Split { .. }
        ));
        assert!(matches!(
            state.workspaces[0].tabs[1].tree,
            LayoutTree::Leaf { .. }
        ));
        // The active workspace resolves to the saved id (11 -> "two").
        assert_eq!(state.active_workspace, state.workspaces[1].id);
        // Ids are re-allocated (never the stale 30/31/32/33) and next_id covers them.
        let mut leaves = Vec::new();
        for workspace in &state.workspaces {
            for tab in &workspace.tabs {
                layout::leaves(&tab.tree, &mut leaves);
            }
        }
        assert_eq!(leaves.len(), 4);
        assert!(leaves.iter().all(|id| id.0 <= state.next_id && id.0 > 0));
        drop(state);
        assert_eq!(session.panes.lock().expect("pane lock").len(), 4);
        assert!(session.restored.load(Ordering::Relaxed));
        // cwds round-trip: panes were spawned in /tmp and /, captured via spawn_cwd.
        let rebuilt = session.build_file();
        let cwds: Vec<_> = rebuilt
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .flat_map(|tab| tab.panes.iter())
            .map(|pane| pane.cwd.clone())
            .collect();
        assert!(cwds.contains(&Some(PathBuf::from("/tmp"))));
        assert!(cwds.contains(&Some(PathBuf::from("/"))));
    }

    #[test]
    fn resume_command_uses_manifest_resume_only_when_enabled() {
        let manifests = vec![manifest::Manifest {
            name: "codex".into(),
            display: "Codex".into(),
            process: vec!["codex".into()],
            title: Vec::new(),
            resume: Some("codex resume --last".into()),
            rules: Vec::new(),
        }];
        let agent = persist::PaneFile {
            id: 1,
            title: "codex".into(),
            cwd: None,
            command: Some(vec!["codex".into()]),
        };
        // resume_agents off: never re-run.
        assert_eq!(resume_command(&agent, false, &manifests), None);
        // On, and a manifest with a resume matches: use the resume command.
        assert_eq!(
            resume_command(&agent, true, &manifests),
            Some(vec!["codex".into(), "resume".into(), "--last".into()])
        );
        // A plain shell pane has no command to resume.
        let shell = persist::PaneFile {
            id: 2,
            title: "shell".into(),
            cwd: None,
            command: None,
        };
        assert_eq!(resume_command(&shell, true, &manifests), None);
        // A command with no matching manifest is left as a plain shell.
        let unknown = persist::PaneFile {
            id: 3,
            title: "make".into(),
            cwd: None,
            command: Some(vec!["make".into()]),
        };
        assert_eq!(resume_command(&unknown, true, &manifests), None);
    }

    #[tokio::test]
    async fn restore_without_resume_agents_starts_plain_shells() {
        // resume_agents=false: the saved `codex` command is not re-run.
        let session = Session::restore(restore_fixture(), false).expect("restore");
        let panes = session.panes.lock().expect("pane lock");
        assert!(panes.values().all(|pane| pane.spawn_command.is_none()));
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

    #[test]
    fn should_notify_only_on_transitions_into_alert_states() {
        use AgentStateKind::*;
        // The initial detection at spawn (last is None) never notifies.
        assert!(!should_notify(None, Blocked, true));
        assert!(!should_notify(None, Done, true));
        // Genuine transitions into blocked/done for a known agent notify.
        assert!(should_notify(Some(Working), Blocked, true));
        assert!(should_notify(Some(Idle), Done, true));
        assert!(should_notify(Some(Done), Blocked, true));
        // An unknown agent never notifies, however it transitions.
        assert!(!should_notify(Some(Working), Blocked, false));
        // Staying in the same state does not re-notify.
        assert!(!should_notify(Some(Blocked), Blocked, true));
        // Transitions into non-alert states never notify.
        assert!(!should_notify(Some(Blocked), Idle, true));
        assert!(!should_notify(Some(Done), Working, true));
    }

    async fn next_server_message(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    ) -> ServerMessage {
        let line = lines
            .next_line()
            .await
            .expect("read line")
            .expect("stream open");
        decode::<ServerMessage>(line.as_bytes()).expect("decode server message")
    }

    #[tokio::test]
    async fn blocked_transition_reaches_attached_client() {
        let directory =
            std::env::temp_dir().join(format!("kodade-cli-notify-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("notify.sock");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let session = Arc::new(Session::spawn(80, 24, "notify".into()).expect("spawn session"));
        // Accept loop: serve every client that connects, just like `run`.
        let accept = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let session = Arc::clone(&session);
                    tokio::spawn(async move {
                        let _ = serve_client(stream, session, "notify".into()).await;
                    });
                }
            })
        };

        // Attached client: Hello, then read Welcome + the first Layout, which
        // settles the pane's baseline state (idle) so the later report is a
        // genuine transition.
        let (reader, mut writer) = UnixStream::connect(&socket)
            .await
            .expect("connect client")
            .into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(&encode(&ClientMessage::Hello { cols: 80, rows: 24 }).unwrap())
            .await
            .expect("send hello");
        assert!(matches!(
            next_server_message(&mut lines).await,
            ServerMessage::Welcome { .. }
        ));
        let pane = match next_server_message(&mut lines).await {
            ServerMessage::Layout(layout) => layout.panes[0].id,
            other => panic!("expected first layout, got {other:?}"),
        };

        // A second connection reports the pane blocked, as an agent hook would.
        let (_r, mut reporter) = UnixStream::connect(&socket)
            .await
            .expect("connect reporter")
            .into_split();
        reporter
            .write_all(
                &encode(&ClientMessage::AgentState {
                    pane,
                    state: AgentStateKind::Blocked,
                    source: "test".into(),
                })
                .unwrap(),
            )
            .await
            .expect("report blocked");

        // The attached client must see a Notification within a second.
        let notification = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let ServerMessage::Notification(notification) =
                    next_server_message(&mut lines).await
                {
                    break notification;
                }
            }
        })
        .await
        .expect("notification arrives within 1s");
        assert_eq!(notification.pane, pane);
        assert_eq!(notification.state, AgentStateKind::Blocked);

        accept.abort();
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir(&directory);
    }
}
