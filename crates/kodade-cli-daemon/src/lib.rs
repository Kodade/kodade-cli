//! Persistent PTY host and session model for Ködade CLI.

mod agent;
mod git;
mod graphics;
#[cfg(unix)]
mod handoff;
mod history;
mod image_paste;
pub use image_paste::{validate_png, MAX_IMAGE_BYTES};
mod layout;
mod manifest;
mod persist;
mod plugins;
mod proc;
mod terminal_replay;

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
        unix::fs::FileTypeExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use kodade_cli_proto::{
    decode, encode, schema_message, AgentInfo, AgentStateKind, CellColor, ClientMessage, Direction,
    Event, InvocationContext, LayoutSnapshot, LayoutTree, ManifestInfo, NativeSession,
    Notification, PaneId, PaneSnapshot, QueryKind, Run, Screen, ServerMessage, SidebarTabInfo,
    SplitAxis, TabId, TabInfo, WorkspaceId, WorkspaceInfo, ATTR_BOLD, ATTR_DIM, ATTR_INVERSE,
    ATTR_ITALIC, ATTR_UNDERLINE, PROTOCOL_VERSION,
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

struct Session {
    images: image_paste::Inbox,
    /// Session name; behind a lock because `session rename` moves it (#16).
    name: Mutex<String>,
    state: Mutex<SessionState>,
    panes: Mutex<HashMap<PaneId, Arc<Pane>>>,
    updates: broadcast::Sender<()>,
    shutdown: broadcast::Sender<()>,
    size: Mutex<(u16, u16)>,
    /// Reloadable detection rules. Replacing this lock happens only after a
    /// complete replacement set has parsed successfully.
    manifests: Mutex<Vec<manifest::Manifest>>,
    /// Bumped by every layout-changing mutation (via `notify`), never by PTY
    /// output. The persist task watches this so scrollback churn is not saved.
    /// Kept on `Session` (not `SessionState`) so `notify` can bump it without
    /// re-locking state — several handlers call `notify` while holding the lock.
    layout_generation: AtomicU64,
    /// Bumped by PTY output. Only the opt-in history loop observes this.
    output_generation: Arc<AtomicU64>,
    pane_history: bool,
    /// True from a cold restore until the first client `Hello`; surfaced as
    /// `LayoutSnapshot.restored` so `ls` can print `(restored)`.
    restored: AtomicBool,
    /// Ring of recent agent notifications (#10), newest last, capped at 64.
    /// Each attached client drains it by `seq` after its next snapshot.
    notifications: Mutex<Vec<Notification>>,
    /// Monotonic high-water mark for `Notification.seq`; also the id a freshly
    /// attached client uses so it never replays the backlog.
    notify_seq: AtomicU64,
    /// Session events for subscribed connections (#16). Nothing is buffered:
    /// a client that is not subscribed when an event fires never sees it.
    events: broadcast::Sender<Event>,
    /// Number of connections currently subscribed. While it is non-zero the
    /// daemon snapshots on a timer so agent-state transitions are detected even
    /// with no client attached (detection only runs inside `snapshot`).
    subscribers: AtomicUsize,
    /// Path of the bound socket file. `session rename` renames the file in
    /// place, so teardown reads it from here rather than from the start value.
    socket: Mutex<PathBuf>,
    /// Per-daemon socket link injected into every pane. Unlike the public
    /// session socket it survives a session rename, so already-running agent
    /// hooks keep reaching this daemon.
    hook_socket: Mutex<PathBuf>,
    /// Serializes the short compatibility staging used by [`ClientView`].  The
    /// stored session selection remains the scripting/persistence default; an
    /// attached client temporarily installs only its own selection while one
    /// legacy mutation is dispatched.
    view_dispatch: Mutex<()>,
    upgraded: AtomicBool,
    upgrading: broadcast::Sender<()>,
    upgrade_lock: Mutex<()>,
    handoff_active: AtomicBool,
}

/// Source-side staging guard. Dropping it on any validation, transfer, or
/// importer failure resumes every reader before the source continues serving.
struct CapturedHandoff {
    #[allow(dead_code)] // Consumed by the upgrade transaction after validation.
    manifest: handoff::HandoffManifest,
    #[allow(dead_code)] // SCM_RIGHTS duplicates these while the source retains ownership.
    fds: Vec<RawFd>,
    paused: Vec<Arc<Pane>>,
    committed: bool,
}

impl Drop for CapturedHandoff {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for pane in &self.paused {
            pane.reader.resume();
        }
    }
}

struct HandoffActive<'a>(Option<&'a AtomicBool>);
impl Drop for HandoffActive<'_> {
    fn drop(&mut self) {
        if let Some(active) = self.0 {
            active.store(false, Ordering::Release);
        }
    }
}

/// Own target cleanup before the capture guard is allowed to resume the source.
struct Replacement {
    directory: PathBuf,
    handoff: PathBuf,
    staged: PathBuf,
    staged_hook: PathBuf,
    public: PathBuf,
    hook: PathBuf,
    public_backup: PathBuf,
    hook_backup: PathBuf,
    public_saved: bool,
    hook_saved: bool,
    committed: bool,
    child: Option<std::process::Child>,
    captured: Option<CapturedHandoff>,
}

fn handoff_nonce() -> Result<String> {
    let mut bytes = [0_u8; 24];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Replacement {
    fn new(public: PathBuf, hook: PathBuf) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let directory = socket_dir().join(format!(".up-{}", &handoff_nonce()?[..16]));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(Self {
            handoff: directory.join("handoff"),
            staged: directory.join("new"),
            staged_hook: directory.join("new-hook"),
            public_backup: directory.join("old"),
            hook_backup: directory.join("old-hook"),
            directory,
            public,
            hook,
            public_saved: false,
            hook_saved: false,
            committed: false,
            child: None,
            captured: None,
        })
    }
}

impl Drop for Replacement {
    fn drop(&mut self) {
        let mut restored = true;
        if !self.committed {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            for (saved, backup, public) in [
                (self.public_saved, &self.public_backup, &self.public),
                (self.hook_saved, &self.hook_backup, &self.hook),
            ] {
                if saved {
                    if let Err(error) = fs::rename(backup, public) {
                        restored = false;
                        eprintln!(
                            "could not restore {}: {error}; recovery socket retained at {}",
                            public.display(),
                            backup.display()
                        );
                    }
                }
            }
        }
        if restored {
            let _ = fs::remove_dir_all(&self.directory);
        }
        // `captured` drops after this method: the target is gone and aliases
        // are restored before any source reader is resumed.
    }
}

/// Selection and viewport state owned by one socket connection.  It is never
/// persisted and is dropped with the connection, so detached clients cannot
/// change a later client's view.
#[derive(Clone, Debug)]
struct ClientView {
    workspace: WorkspaceId,
    tabs: HashMap<WorkspaceId, TabId>,
    focused: HashMap<TabId, PaneId>,
    scroll: HashMap<PaneId, usize>,
    cols: u16,
    rows: u16,
    compact: bool,
}

impl ClientView {
    fn from_state(state: &SessionState, cols: u16, rows: u16) -> Self {
        let workspace = state.active_workspace;
        let tabs = state
            .workspaces
            .iter()
            .map(|workspace| (workspace.id, workspace.active_tab))
            .collect::<HashMap<_, _>>();
        let focused = state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .map(|tab| (tab.id, tab.focused))
            .collect();
        Self {
            workspace,
            tabs,
            focused,
            scroll: HashMap::new(),
            cols,
            rows,
            compact: false,
        }
    }
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
    /// Sidebar swatch color as `#rrggbb`, when the user set one (#19).
    color: Option<String>,
    /// Explicit environment supplied for this workspace. New panes receive a
    /// copy at spawn time; changing it never reaches already-running panes.
    env: HashMap<String, String>,
    /// Git metadata is refreshed together every two seconds, keeping snapshot
    /// rendering free of filesystem reads.
    branch: Option<String>,
    main_worktree_root: Option<PathBuf>,
    parent: Option<WorkspaceId>,
    metadata_checked_at: Option<Instant>,
}
struct Tab {
    id: TabId,
    name: String,
    tree: LayoutTree,
    focused: PaneId,
    zoomed: bool,
}

#[derive(Clone)]
struct StoredSelection {
    workspace: WorkspaceId,
    tabs: Vec<(WorkspaceId, TabId)>,
    focused: Vec<(TabId, PaneId)>,
}
/// vt100 0.16 reports the OSC window title through callbacks instead of `Screen::title`.
#[derive(Default)]
struct PtyCallbacks {
    title: String,
    graphics: graphics::Store,
    graphics_tracker: graphics::Tracker,
    graphics_decoder: graphics::Decoder,
}
impl vt100::Callbacks for PtyCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}
type PtyParser = vt100::Parser<PtyCallbacks>;

struct Pane {
    title: Mutex<String>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<PtyParser>>,
    last_output: Arc<Mutex<Instant>>,
    reader: Arc<ReaderControl>,
    hook: Mutex<Option<ReportedHook>>,
    spawn_process: String,
    /// The command this pane was spawned with and the directory it started in,
    /// kept so persistence can record them without inspecting the live process.
    spawn_command: Option<Vec<String>>,
    spawn_cwd: Option<PathBuf>,
    native_session: Mutex<Option<NativeSession>>,
    /// Kept so dropping the pane ends its process; otherwise the PTY reader
    /// thread never sees EOF (the daemon and the test runtime would wait forever).
    child: Mutex<Option<Box<dyn portable_pty::Child + Send>>>,
    /// Adopted children cannot be waited by this daemon; signal only when the
    /// PID still has the start identity captured during handoff.
    adopted_child: Option<(i32, String)>,
    adopted_child_owned: AtomicBool,
    process: Mutex<ProcessEvidence>,
    // Tracks the current detected state and its start as one atomic transition,
    // so concurrent snapshots cannot publish the same change twice.
    state: Mutex<PaneState>,
    /// Generation of the recognized foreground agent. This changes if the
    /// process/title evidence identifies a different agent (including none).
    agent_generation: AtomicU64,
    agent_identity: Mutex<Option<String>>,
    activity_revision: AtomicU64,
    /// The daemon owns this file until the pane and its PTY child are gone.
    _context_file: Option<ContextFile>,
}

const MAX_CONTEXT_BYTES: usize = 64 * 1024;

struct ContextFile {
    path: PathBuf,
    owned: AtomicBool,
}

impl ContextFile {
    fn create(context: &InvocationContext) -> Result<Self> {
        let json = serde_json::to_vec(context)?;
        if json.len() > MAX_CONTEXT_BYTES {
            bail!("plugin context exceeds {} KiB", MAX_CONTEXT_BYTES / 1024);
        }
        let path = std::env::temp_dir().join(format!(
            "kodade-plugin-context-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let context_file = Self {
            path,
            owned: AtomicBool::new(true),
        };
        file.write_all(&json)?;
        Ok(context_file)
    }

    fn import(path: PathBuf) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(&path).context("inspect imported context")?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_CONTEXT_BYTES as u64
            || path.parent() != Some(std::env::temp_dir().as_path())
            || !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("kodade-plugin-context-"))
        {
            bail!("invalid private context file in handoff");
        }
        Ok(Self {
            path,
            owned: AtomicBool::new(false),
        })
    }
}

impl Drop for ContextFile {
    fn drop(&mut self) {
        if self.owned.load(Ordering::Acquire) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Closing a pane terminates its process so the PTY reader thread exits.
impl Drop for Pane {
    fn drop(&mut self) {
        self.reader.shutdown();
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut child) = child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some((pid, start)) = &self.adopted_child {
            if !self.adopted_child_owned.load(Ordering::Acquire) {
                return;
            }
            if proc::start_identity(*pid).as_deref() == Some(start) {
                let _ = unsafe { libc::kill(*pid, libc::SIGTERM) };
            }
        }
    }
}

/// Coordinates a PTY reader at poll/read boundaries. A handoff waits for the
/// acknowledgement before another process receives a duplicated master.
struct ReaderControl {
    state: Mutex<ReaderState>,
    changed: Condvar,
}

struct ReaderState {
    paused: bool,
    acknowledged: bool,
    shutdown: bool,
}

impl ReaderControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(ReaderState {
                paused: false,
                acknowledged: false,
                shutdown: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn paused() -> Self {
        Self {
            state: Mutex::new(ReaderState {
                paused: true,
                acknowledged: false,
                shutdown: false,
            }),
            changed: Condvar::new(),
        }
    }

    #[allow(dead_code)] // Called by the live-handoff commit phase.
    fn pause(&self, timeout: Duration) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("PTY reader control lock poisoned"))?;
        state.paused = true;
        state.acknowledged = false;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                !state.acknowledged && !state.shutdown
            })
            .map_err(|_| anyhow!("PTY reader control lock poisoned"))?;
        if state.acknowledged {
            Ok(())
        } else {
            bail!("timed out waiting for PTY reader to quiesce")
        }
    }

    #[allow(dead_code)] // Called when an import rejects or times out.
    fn resume(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.paused = false;
            state.acknowledged = false;
            self.changed.notify_all();
        }
    }

    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.paused = false;
            self.changed.notify_all();
        }
    }

    /// Returns false once teardown begins. The acknowledgement is published
    /// before waiting, so a caller that observes it knows no read is active.
    fn wait_if_paused(&self) -> bool {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return false,
        };
        while state.paused && !state.shutdown {
            state.acknowledged = true;
            self.changed.notify_all();
            state = match self.changed.wait(state) {
                Ok(state) => state,
                Err(_) => return false,
            };
        }
        !state.shutdown
    }
}

#[derive(Clone)]
struct ReportedHook {
    state: AgentStateKind,
    source: String,
    agent: Option<String>,
    process_pid: Option<i32>,
    process_name: Option<String>,
    reported_at: Instant,
}

#[derive(Clone)]
struct ProcessEvidence {
    pid: Option<i32>,
    name: Option<String>,
    cwd: Option<PathBuf>,
    checked_at: Instant,
}

struct PaneState {
    last: Option<AgentStateKind>,
    since: Instant,
}

impl PaneState {
    /// Record one observation and return the previous state plus this state's
    /// age. Holding both values together makes the returned transition real
    /// even when multiple clients snapshot at the same time.
    fn transition(&mut self, next: AgentStateKind, now: Instant) -> (Option<AgentStateKind>, u64) {
        let previous = self.last;
        self.since = state_since_after(previous, next, self.since, now);
        self.last = Some(next);
        (
            previous,
            now.saturating_duration_since(self.since).as_secs(),
        )
    }
}

/// Validate downloaded detection rules against the same schema used at startup.
pub fn validate_agent_manifest(source: &str) -> Result<String> {
    let manifest: manifest::Manifest = toml::from_str(source).context("parse agent manifest")?;
    manifest::validate(&manifest)?;
    Ok(manifest.name)
}
/// Kept in step with the CLI: a prompt is one atomic PTY writer submission.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

pub fn socket_path(session: &str) -> PathBuf {
    socket_dir().join(format!("{session}.sock"))
}

fn hook_socket_path() -> PathBuf {
    // Keep hook transport out of the public socket directory: `session ls`
    // deliberately scans its direct `*.sock` children. A PID is unique among
    // concurrent daemons and keeps this path comfortably under sockaddr limits.
    socket_dir()
        .join("hooks")
        .join(format!("{}.sock", std::process::id()))
}

/// Directory holding one `<session>.sock` per live session. `session ls` scans
/// it, so it is part of the public surface.
pub fn socket_dir() -> PathBuf {
    let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let home = dirs::home_dir();
    let uid = env::var("UID").unwrap_or_else(|_| "unknown".to_owned());
    socket_path_for(
        "unused",
        runtime.as_deref(),
        home.as_deref(),
        &uid,
        cfg!(target_os = "macos"),
    )
    .parent()
    .expect("socket path always has a parent directory")
    .to_path_buf()
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
    let hook_socket = hook_socket_path();
    fs::create_dir_all(hook_socket.parent().expect("hook socket has parent"))
        .context("create Ködade hook socket directory")?;
    if let Ok(metadata) = fs::symlink_metadata(&hook_socket) {
        if !metadata.file_type().is_socket() {
            drop(listener);
            let _ = fs::remove_file(&socket);
            bail!(
                "refusing to replace non-socket Ködade hook path {}",
                hook_socket.display()
            );
        }
        // A PID-reused stale socket is safe to remove only after this process
        // has exclusively bound the public session name. A live socket is not.
        if let Err(error) = remove_stale_socket(&hook_socket).await {
            drop(listener);
            let _ = fs::remove_file(&socket);
            return Err(error).context("remove stale Ködade hook socket");
        }
    }
    if let Err(error) = fs::hard_link(&socket, &hook_socket) {
        drop(listener);
        let _ = fs::remove_file(&socket);
        return Err(error).context("link stable Ködade hook socket");
    }
    // Binding succeeded, so no live daemon owns this session: safe to restore.
    let session = match load_session(&session_name) {
        Ok(session) => Arc::new(session),
        Err(error) => {
            drop(listener);
            let _ = fs::remove_file(&hook_socket);
            let _ = fs::remove_file(&socket);
            return Err(error);
        }
    };
    // The socket name wins over whatever the state file recorded (it may have
    // been renamed or copied), so `save` and teardown stay on this path.
    *session.name.lock().expect("name lock poisoned") = session_name.clone();
    *session.socket.lock().expect("socket lock poisoned") = socket.clone();
    session.run_plugin_hooks("startup", None);
    let mut shutdown = session.shutdown.subscribe();
    // Debounced layout persistence runs alongside the accept loop.
    tokio::spawn(persist_loop(Arc::clone(&session)));
    if session.pane_history {
        tokio::spawn(history_loop(Arc::clone(&session)));
    } else {
        history::remove_for_session(&session.session_name());
    }
    // Keeps agent detection running for event subscribers with no TUI attached.
    tokio::spawn(subscriber_tick(Arc::clone(&session)));
    // Flush state on SIGTERM so a stopped daemon (e.g. logout) can be restored.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                if session.upgraded.load(Ordering::Acquire) {
                    // The target owns duplicated PTYs. Exit without Rust drops:
                    // portable-pty closes a master by writing EOT on Drop.
                    std::process::exit(0);
                }
                // `kill-session` is deliberate: drop the state file so it does
                // not resurrect on the next cold start.
                persist::remove_session_file(&session.session_name());
                session.images.clear();
                drop(listener);
                let _ = fs::remove_file(session.socket_path());
                let _ = fs::remove_file(session.hook_socket());
                return Ok(());
            }
            _ = sigterm.recv() => {
                session.save();
                session.images.clear();
                drop(listener);
                let _ = fs::remove_file(session.socket_path());
                let _ = fs::remove_file(session.hook_socket());
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let session = Arc::clone(&session);
                tokio::spawn(async move {
                    if let Err(error) = serve_client(stream, session).await {
                        eprintln!("Ködade CLI client disconnected: {error:#}");
                    }
                });
            }
        }
    }
}

/// Hidden daemon mode used by a source daemon during a live upgrade.
pub async fn run_import(
    session_name: String,
    handoff_path: PathBuf,
    token: String,
    staged_socket: PathBuf,
    staged_hook: PathBuf,
    final_hook: PathBuf,
) -> Result<()> {
    validate_session_name(&session_name)?;
    let received = tokio::task::spawn_blocking(move || handoff::receive(&handoff_path, &token))
        .await
        .context("join handoff receiver")?
        .context("receive live handoff")?;
    let session = unsafe { Session::import_handoff(received.manifest, received.fds) }?;
    let session = Arc::new(session);
    let listener = handoff::bind_listener(&staged_socket).context("bind staged daemon socket")?;
    fs::hard_link(&staged_socket, &staged_hook).context("link staged hook socket")?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    *session.name.lock().expect("name lock poisoned") = session_name;
    *session.socket.lock().expect("socket lock poisoned") = socket_path(&session.session_name());
    *session
        .hook_socket
        .lock()
        .expect("hook socket lock poisoned") = final_hook;
    let mut control = received.stream;
    let committed = tokio::task::spawn_blocking(move || {
        handoff::ready(&mut control)?;
        handoff::wait_commit(&mut control)?;
        handoff::committed(&mut control)?;
        // A lost acknowledgment can still roll the source back. Do not read
        // output or acquire deletion rights until it releases ownership.
        handoff::wait_release(&mut control)
    })
    .await
    .context("join handoff commit")?;
    committed?;
    session.images.take_ownership();
    for pane in session.panes.lock().expect("pane lock poisoned").values() {
        pane.adopted_child_owned.store(true, Ordering::Release);
        if let Some(context) = &pane._context_file {
            context.owned.store(true, Ordering::Release);
        }
        pane.reader.resume();
    }
    // Every fallible staging operation completed before acknowledgment. The
    // published listener is already registered with the replacement runtime.
    serve_imported_listener(listener, session).await
}

async fn serve_imported_listener(listener: UnixListener, session: Arc<Session>) -> Result<()> {
    let mut shutdown = session.shutdown.subscribe();
    tokio::spawn(persist_loop(Arc::clone(&session)));
    if session.pane_history {
        tokio::spawn(history_loop(Arc::clone(&session)));
    }
    tokio::spawn(subscriber_tick(Arc::clone(&session)));
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                if session.upgraded.load(Ordering::Acquire) {
                    std::process::exit(0);
                }
                persist::remove_session_file(&session.session_name());
                drop(listener);
                let _ = fs::remove_file(session.socket_path());
                let _ = fs::remove_file(session.hook_socket());
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let session = Arc::clone(&session);
                tokio::spawn(async move { let _ = serve_client(stream, session).await; });
            }
        }
    }
}

/// Restore this session from its state file when one is present and valid; a
/// corrupt or foreign file is moved aside and a clean session starts instead.
fn load_session(name: &str) -> Result<Session> {
    if let Some(path) = persist::session_file_path(name) {
        match persist::read_session_file(&path) {
            Ok(Some(mut file)) => {
                let resume_agents = persist::resume_agents_setting();
                // The file may have been copied or survived a partial rename;
                // the bound public name and its hook link are authoritative
                // before restored panes inherit their environment.
                file.name = name.to_owned();
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

/// Persist a bounded replay at most every two seconds after output or layout
/// changes. Layout persistence remains independent and never writes terminal
/// text by default.
async fn history_loop(session: Arc<Session>) {
    let mut saved = (0, 0);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let current = (
            session.output_generation.load(Ordering::Relaxed),
            session.layout_generation.load(Ordering::Relaxed),
        );
        if current == saved {
            continue;
        }
        match session.save_history() {
            Ok(()) => saved = current,
            Err(error) => eprintln!("Ködade CLI could not persist pane history: {error:#}"),
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

/// Validate the single path component used for a session's socket and state
/// file. Clients should call this before deriving either path.
pub fn validate_session_name(session: &str) -> Result<()> {
    if session.is_empty()
        || session.len() > 64
        || session.contains(['/', '\\'])
        || session.chars().any(char::is_control)
        || session == "."
        || session == ".."
    {
        bail!("session names must be non-empty path components");
    }
    Ok(())
}

impl Session {
    fn spawn(cols: u16, rows: u16, name: String) -> Result<Self> {
        let name_for_socket = name.clone();
        let (updates, _) = broadcast::channel(64);
        let (shutdown, _) = broadcast::channel(16);
        let (events, _) = broadcast::channel(256);
        let session = Self {
            images: image_paste::Inbox::default(),
            name: Mutex::new(name),
            state: Mutex::new(SessionState {
                workspaces: Vec::new(),
                active_workspace: WorkspaceId(1),
                next_id: 1,
            }),
            panes: Mutex::new(HashMap::new()),
            updates,
            shutdown,
            size: Mutex::new((cols, rows)),
            manifests: Mutex::new(manifest::load()?),
            layout_generation: AtomicU64::new(0),
            output_generation: Arc::new(AtomicU64::new(0)),
            pane_history: persist::session_settings().pane_history,
            restored: AtomicBool::new(false),
            notifications: Mutex::new(Vec::new()),
            notify_seq: AtomicU64::new(0),
            events,
            subscribers: AtomicUsize::new(0),
            socket: Mutex::new(socket_path(&name_for_socket)),
            hook_socket: Mutex::new(hook_socket_path()),
            view_dispatch: Mutex::new(()),
            upgraded: AtomicBool::new(false),
            upgrading: broadcast::channel(16).0,
            upgrade_lock: Mutex::new(()),
            handoff_active: AtomicBool::new(false),
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
                color: None,
                env: HashMap::new(),
                branch: None,
                main_worktree_root: None,
                parent: None,
                metadata_checked_at: None,
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
        let (events, _) = broadcast::channel(256);
        let session = Self {
            images: image_paste::Inbox::default(),
            name: Mutex::new(file.name.clone()),
            state: Mutex::new(SessionState {
                workspaces: Vec::new(),
                active_workspace: WorkspaceId(1),
                next_id: 0,
            }),
            panes: Mutex::new(HashMap::new()),
            updates,
            shutdown,
            size: Mutex::new((80, 24)),
            manifests: Mutex::new(manifest::load()?),
            layout_generation: AtomicU64::new(0),
            output_generation: Arc::new(AtomicU64::new(0)),
            pane_history: persist::session_settings().pane_history,
            restored: AtomicBool::new(true),
            notifications: Mutex::new(Vec::new()),
            notify_seq: AtomicU64::new(0),
            events,
            subscribers: AtomicUsize::new(0),
            socket: Mutex::new(socket_path(&file.name)),
            hook_socket: Mutex::new(hook_socket_path()),
            view_dispatch: Mutex::new(()),
            upgraded: AtomicBool::new(false),
            upgrading: broadcast::channel(16).0,
            upgrade_lock: Mutex::new(()),
            handoff_active: AtomicBool::new(false),
        };
        let mut resumed_native_sessions = HashSet::new();
        let replay = if session.pane_history {
            persist::session_file_path(&file.name)
                .and_then(|path| {
                    history::read(&history::path_for(&path), &file)
                        .ok()
                        .flatten()
                })
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.pane, entry))
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
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
                    let command =
                        resume_command(saved_pane, resume_agents, &mut resumed_native_sessions);
                    let resumed_native = command
                        .as_ref()
                        .and(saved_pane.native_session.as_ref())
                        .cloned();
                    // A native resume owns its pane's first output; replay only
                    // plain-shell restorations so it can never overwrite it.
                    let replay = command
                        .is_none()
                        .then(|| replay.get(&saved_pane.id).cloned())
                        .flatten();
                    let new_id = session.new_pane_with_replay(
                        &saved_pane.title,
                        cwd,
                        command,
                        replay,
                        saved.env.clone(),
                    )?;
                    if let Some(native) = resumed_native {
                        session
                            .panes
                            .lock()
                            .expect("panes lock poisoned")
                            .get(&new_id)
                            .expect("new pane exists")
                            .native_session
                            .lock()
                            .expect("native session lock poisoned")
                            .replace(native);
                    }
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
                color: saved.color.clone(),
                env: saved.env.clone(),
                branch: None,
                main_worktree_root: None,
                parent: None,
                metadata_checked_at: None,
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
                color: workspace.color.clone(),
                env: workspace.env.clone(),
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
                                    native_session: pane
                                        .native_session
                                        .lock()
                                        .expect("native session lock poisoned")
                                        .clone(),
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
            name: self.session_name(),
            active_workspace: state.active_workspace.0,
            workspaces,
        }
    }

    /// Read the persisted/script projection without observing another
    /// connection's brief compatibility staging.
    fn build_file_stable(&self) -> Result<persist::SessionFile> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        Ok(self.build_file())
    }

    /// Snapshot layout ids and runtime state only after every source reader
    /// acknowledges its pause. `CapturedHandoff` is the rollback authority.
    #[allow(dead_code)] // Used by the live upgrade transaction.
    fn capture_handoff(&self) -> Result<CapturedHandoff> {
        let file = self.build_file_stable()?;
        file.validate()?;
        let panes: Vec<(PaneId, Arc<Pane>)> = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?
            .iter()
            .map(|(id, pane)| (*id, Arc::clone(pane)))
            .collect();
        if panes.len() > handoff::MAX_HANDOFF_FDS {
            bail!("handoff supports at most 64 PTYs");
        }
        let mut captured = CapturedHandoff {
            manifest: handoff::HandoffManifest::new(file, Vec::new())?,
            fds: Vec::with_capacity(panes.len()),
            paused: Vec::with_capacity(panes.len()),
            committed: false,
        };
        let mut captured_bytes = 0;
        for (id, pane) in panes {
            let (mut runtime, fd) = pane.capture_handoff()?;
            captured.paused.push(pane);
            runtime.pane_id = id.0;
            captured_bytes += serde_json::to_vec(&runtime)?.len();
            if captured_bytes > handoff::MAX_MANIFEST_BYTES {
                bail!("live runtime exceeds handoff limit of 128 MiB");
            }
            captured.manifest.panes.push(runtime);
            captured.fds.push(fd);
        }
        captured.manifest.attachments = self.images.snapshot();
        captured.manifest.notify_seq = self.notify_high_water();
        captured.manifest.notifications = self
            .notifications
            .lock()
            .map_err(|_| anyhow!("notification lock poisoned"))?
            .clone();
        captured.manifest.size = *self
            .size
            .lock()
            .map_err(|_| anyhow!("size lock poisoned"))?;
        captured.manifest.pane_history = self.pane_history;
        Ok(captured)
    }

    /// Freeze source mutation and transfer ownership only after target readiness.
    fn upgrade(&self, binary: Option<PathBuf>) -> Result<()> {
        let _upgrade_lock = self
            .upgrade_lock
            .try_lock()
            .map_err(|_| anyhow!("a live upgrade is already in progress"))?;
        {
            let _dispatch = self
                .view_dispatch
                .lock()
                .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
            if self.handoff_active.swap(true, Ordering::AcqRel) {
                bail!("a live upgrade is already in progress");
            }
        }
        let mut active = HandoffActive(Some(&self.handoff_active));
        let binary = match binary {
            Some(binary) => binary,
            None => {
                let executable = std::env::current_exe().context("find current executable")?;
                // Linux marks a running executable as deleted after an atomic
                // update. The original path now names the verified replacement.
                #[cfg(target_os = "linux")]
                let executable = {
                    use std::os::unix::ffi::{OsStrExt, OsStringExt};
                    if executable.exists() {
                        executable
                    } else {
                        executable
                            .as_os_str()
                            .as_bytes()
                            .strip_suffix(b" (deleted)")
                            .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())))
                            .unwrap_or(executable)
                    }
                };
                executable
            }
        };
        let mut transaction = Replacement::new(self.socket_path(), self.hook_socket())?;
        let listener = handoff::bind_listener(&transaction.handoff)?;
        let token = handoff_nonce()?;
        transaction.child = Some(
            Command::new(&binary)
                .arg("daemon")
                .arg(self.session_name())
                .arg("--import")
                .arg(&transaction.handoff)
                .env("KODADE_HANDOFF_TOKEN", &token)
                .arg("--staged-socket")
                .arg(&transaction.staged)
                .arg("--staged-hook")
                .arg(&transaction.staged_hook)
                .arg("--hook-socket")
                .arg(&transaction.hook)
                .spawn()
                .with_context(|| format!("start replacement daemon {}", binary.display()))?,
        );
        transaction.captured = Some(self.capture_handoff()?);
        let captured = transaction.captured.as_ref().expect("captured runtime");
        let mut control = handoff::accept_and_validate(&listener, &token, &captured.manifest)
            .context("validate replacement daemon")?;
        handoff::send_fds(&control, &captured.fds).context("transfer PTY masters")?;
        handoff::wait_ready(&mut control).context("wait for replacement daemon")?;
        fs::hard_link(&transaction.public, &transaction.public_backup)?;
        transaction.public_saved = true;
        fs::hard_link(&transaction.hook, &transaction.hook_backup)?;
        transaction.hook_saved = true;
        fs::rename(&transaction.staged, &transaction.public)?;
        fs::rename(&transaction.staged_hook, &transaction.hook)?;
        handoff::commit(&mut control).context("commit replacement daemon")?;
        handoff::wait_committed(&mut control).context("acknowledge replacement daemon")?;
        handoff::release(&mut control).context("release runtime ownership")?;
        transaction.committed = true;
        transaction
            .captured
            .as_mut()
            .expect("captured runtime")
            .committed = true;
        active.0 = None;
        Ok(())
    }

    /// Rebuild session layout with its original ids around imported PTY masters.
    /// Readers remain paused until the transport ownership stage completes.
    #[allow(dead_code)] // Called by the hidden handoff-import daemon mode.
    unsafe fn import_handoff(manifest: handoff::HandoffManifest, fds: Vec<RawFd>) -> Result<Self> {
        let fds: Vec<OwnedFd> = fds.into_iter().map(|fd| OwnedFd::from_raw_fd(fd)).collect();
        manifest.session.validate()?;
        if manifest.panes.len() != fds.len() {
            bail!("handoff runtime/descriptor count mismatch");
        }
        let (updates, _) = broadcast::channel(64);
        let (shutdown, _) = broadcast::channel(16);
        let (events, _) = broadcast::channel(256);
        let session = Self {
            images: image_paste::Inbox::import(manifest.attachments)?,
            name: Mutex::new(manifest.session.name.clone()),
            state: Mutex::new(SessionState {
                workspaces: Vec::new(),
                active_workspace: WorkspaceId(manifest.session.active_workspace),
                next_id: 0,
            }),
            panes: Mutex::new(HashMap::new()),
            updates,
            shutdown,
            size: Mutex::new(manifest.size),
            manifests: Mutex::new(manifest::load()?),
            layout_generation: AtomicU64::new(0),
            output_generation: Arc::new(AtomicU64::new(1)),
            pane_history: manifest.pane_history,
            restored: AtomicBool::new(false),
            notifications: Mutex::new(manifest.notifications),
            notify_seq: AtomicU64::new(manifest.notify_seq),
            events,
            subscribers: AtomicUsize::new(0),
            socket: Mutex::new(socket_path(&manifest.session.name)),
            hook_socket: Mutex::new(hook_socket_path()),
            view_dispatch: Mutex::new(()),
            upgraded: AtomicBool::new(false),
            upgrading: broadcast::channel(16).0,
            upgrade_lock: Mutex::new(()),
            handoff_active: AtomicBool::new(false),
        };
        let mut runtimes: HashMap<u64, (handoff::PaneRuntime, OwnedFd)> = manifest
            .panes
            .into_iter()
            .zip(fds)
            .map(|(runtime, fd)| (runtime.pane_id, (runtime, fd)))
            .collect();
        let mut maximum = 0;
        let mut workspaces = Vec::new();
        for saved in manifest.session.workspaces {
            maximum = maximum.max(saved.id).max(saved.active_tab);
            let mut tabs = Vec::new();
            for saved_tab in saved.tabs {
                maximum = maximum.max(saved_tab.id).max(saved_tab.focused);
                for saved_pane in &saved_tab.panes {
                    maximum = maximum.max(saved_pane.id);
                    let (runtime, fd) = runtimes
                        .remove(&saved_pane.id)
                        .ok_or_else(|| anyhow!("handoff missing pane {}", saved_pane.id))?;
                    session
                        .panes
                        .lock()
                        .map_err(|_| anyhow!("pane lock poisoned"))?
                        .insert(
                            PaneId(saved_pane.id),
                            Arc::new(Pane::import_handoff(
                                runtime,
                                fd.into_raw_fd(),
                                session.updates.clone(),
                                Arc::clone(&session.output_generation),
                            )?),
                        );
                }
                tabs.push(Tab {
                    id: TabId(saved_tab.id),
                    name: saved_tab.name,
                    tree: saved_tab.tree,
                    focused: PaneId(saved_tab.focused),
                    zoomed: saved_tab.zoomed,
                });
            }
            workspaces.push(Workspace {
                id: WorkspaceId(saved.id),
                name: saved.name,
                tabs,
                active_tab: TabId(saved.active_tab),
                root: saved.root,
                env: saved.env,
                color: saved.color,
                branch: None,
                main_worktree_root: None,
                parent: None,
                metadata_checked_at: None,
            });
        }
        if !runtimes.is_empty() {
            bail!("handoff includes panes outside the layout");
        }
        let mut state = session
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?;
        state.workspaces = workspaces;
        state.next_id = maximum;
        drop(state);
        Ok(session)
    }

    /// Persist the current layout to this session's state file. Best-effort:
    /// errors are logged, never propagated to the client-facing loop.
    fn save(&self) {
        // Keep publication inside dispatch so rename cannot delete the old
        // state file while an earlier save is still about to publish it.
        let Ok(_dispatch) = self.view_dispatch.lock() else {
            eprintln!("Ködade CLI could not persist: view dispatch lock poisoned");
            return;
        };
        let file = self.build_file();
        let Some(path) = persist::session_file_path(&file.name) else {
            return;
        };
        if let Err(error) = persist::write_session_file(&path, &file) {
            eprintln!(
                "Ködade CLI could not persist session '{}': {error:#}",
                file.name
            );
        }
    }

    fn save_history(&self) -> Result<()> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        let file = self.build_file();
        let Some(state_path) = persist::session_file_path(&file.name) else {
            return Ok(());
        };
        // Output can change cwd/title without a layout mutation. Publish this
        // exact identity first; a crash between writes safely rejects old replay.
        persist::write_session_file(&state_path, &file)?;
        let panes = self
            .panes
            .lock()
            .expect("pane lock poisoned")
            .iter()
            .map(|(id, pane)| {
                let mut parser = pane.parser.lock().expect("PTY parser lock poisoned");
                let text = read_history(&mut parser).join("\n");
                let text = utf8_tail(&text, history::MAX_PANE_TEXT);
                parser.screen_mut().set_scrollback(0);
                history::PaneHistory {
                    pane: id.0,
                    text,
                    screen: snapshot(&parser),
                }
            })
            .collect();
        history::write(&history::path_for(&state_path), file, panes)
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

    /// Current session name (see [`Session::name`]).
    fn session_name(&self) -> String {
        self.name.lock().expect("name lock poisoned").clone()
    }

    fn hook_socket(&self) -> PathBuf {
        self.hook_socket
            .lock()
            .expect("hook socket lock poisoned")
            .clone()
    }

    /// Publish a session event to subscribed connections. Never fails: with no
    /// subscribers the send is a no-op.
    fn emit(&self, event: Event) {
        let name = match &event {
            Event::PaneOpened { .. } => "pane_opened",
            Event::PaneClosed { .. } => "pane_closed",
            Event::TabOpened { .. } => "tab_opened",
            Event::TabClosed { .. } => "tab_closed",
            Event::TabRenamed { .. } => "tab_renamed",
            Event::WorkspaceOpened { .. } => "workspace_opened",
            Event::WorkspaceClosed { .. } => "workspace_closed",
            Event::WorkspaceRenamed { .. } => "workspace_renamed",
            Event::AgentStateChanged { .. } => "agent_state_changed",
            Event::Notification(_) => "notification",
            Event::SessionRenamed { .. } => "session_renamed",
        };
        let pane = match &event {
            Event::PaneOpened { pane } | Event::PaneClosed { pane } => Some(pane.0),
            Event::AgentStateChanged { pane, .. } => Some(pane.0),
            Event::Notification(notification) => Some(notification.pane.0),
            _ => None,
        };
        self.run_plugin_hooks(name, pane);
        let _ = self.events.send(event);
    }

    fn run_plugin_hooks(&self, event: &str, pane: Option<u64>) {
        plugins::run(event, self.session_name(), self.socket_path(), pane);
    }

    fn new_pane(
        &self,
        title: &str,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
    ) -> Result<PaneId> {
        self.new_pane_with_id(self.pane_id(), title, cwd, command, None)
    }

    fn new_pane_with_replay(
        &self,
        title: &str,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        replay: Option<history::PaneHistory>,
        env: HashMap<String, String>,
    ) -> Result<PaneId> {
        self.new_pane_with_id_replay(self.pane_id(), title, cwd, command, replay, None, env)
    }

    /// Spawn a pane under a caller-chosen id, used by `layout apply` so a pane
    /// that is already alive keeps its id.
    fn new_pane_with_id(
        &self,
        id: PaneId,
        title: &str,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        context_file: Option<ContextFile>,
    ) -> Result<PaneId> {
        self.new_pane_with_id_replay(id, title, cwd, command, None, context_file, HashMap::new())
    }
    #[allow(clippy::too_many_arguments)]
    fn new_pane_with_id_replay(
        &self,
        id: PaneId,
        title: &str,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        replay: Option<history::PaneHistory>,
        context_file: Option<ContextFile>,
        env: HashMap<String, String>,
    ) -> Result<PaneId> {
        validate_workspace_env(&env)?;
        let (cols, rows) = replay
            .as_ref()
            .and_then(|entry| history::dimensions(&entry.screen))
            .unwrap_or_else(|| *self.size.lock().expect("size lock poisoned"));
        let pane = Arc::new(Pane::spawn(
            id,
            title,
            cols,
            rows,
            self.session_name(),
            self.hook_socket(),
            self.updates.clone(),
            Arc::clone(&self.output_generation),
            cwd,
            command,
            context_file,
            replay,
            env,
        )?);
        self.panes
            .lock()
            .expect("pane lock poisoned")
            .insert(id, pane);
        self.emit(Event::PaneOpened { pane: id });
        Ok(id)
    }

    fn workspace_env(&self, workspace: WorkspaceId) -> Result<HashMap<String, String>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?
            .workspaces
            .iter()
            .find(|item| item.id == workspace)
            .map(|item| item.env.clone())
            .ok_or_else(|| anyhow!("workspace {} not found", workspace.0))
    }

    fn new_workspace_pane(
        &self,
        workspace: WorkspaceId,
        title: &str,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
    ) -> Result<PaneId> {
        self.new_pane_with_id_replay(
            self.pane_id(),
            title,
            cwd,
            command,
            None,
            None,
            self.workspace_env(workspace)?,
        )
    }

    fn new_workspace_pane_with_context(
        &self,
        workspace: WorkspaceId,
        title: &str,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        context_file: Option<ContextFile>,
    ) -> Result<PaneId> {
        self.new_pane_with_id_replay(
            self.pane_id(),
            title,
            cwd,
            command,
            None,
            context_file,
            self.workspace_env(workspace)?,
        )
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
        context: Option<InvocationContext>,
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
        let context_file = context
            .map(|context| ContextFile::create(&context))
            .transpose()?;
        let pane =
            self.new_workspace_pane_with_context(target, &title, cwd, command, context_file)?;
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

    fn save_selection(state: &SessionState) -> StoredSelection {
        StoredSelection {
            workspace: state.active_workspace,
            tabs: state
                .workspaces
                .iter()
                .map(|item| (item.id, item.active_tab))
                .collect(),
            focused: state
                .workspaces
                .iter()
                .flat_map(|item| item.tabs.iter())
                .map(|tab| (tab.id, tab.focused))
                .collect(),
        }
    }

    fn install_view(state: &mut SessionState, view: &ClientView) {
        if state
            .workspaces
            .iter()
            .any(|item| item.id == view.workspace)
        {
            state.active_workspace = view.workspace;
        }
        for workspace in &mut state.workspaces {
            if let Some(tab) = view
                .tabs
                .get(&workspace.id)
                .filter(|tab| workspace.tabs.iter().any(|item| item.id == **tab))
            {
                workspace.active_tab = *tab;
            }
            for tab in &mut workspace.tabs {
                if let Some(pane) = view
                    .focused
                    .get(&tab.id)
                    .filter(|pane| layout::contains(&tab.tree, **pane))
                {
                    tab.focused = *pane;
                }
            }
        }
    }

    fn capture_view(state: &SessionState, view: &mut ClientView) {
        view.workspace = state.active_workspace;
        view.tabs = state
            .workspaces
            .iter()
            .map(|item| (item.id, item.active_tab))
            .collect();
        view.focused = state
            .workspaces
            .iter()
            .flat_map(|item| item.tabs.iter())
            .map(|tab| (tab.id, tab.focused))
            .collect();
    }

    fn restore_selection(state: &mut SessionState, saved: StoredSelection) {
        if state
            .workspaces
            .iter()
            .any(|workspace| workspace.id == saved.workspace)
        {
            state.active_workspace = saved.workspace;
        }
        for workspace in &mut state.workspaces {
            if let Some(tab) = saved
                .tabs
                .iter()
                .find(|(id, _)| *id == workspace.id)
                .map(|(_, tab)| *tab)
                .filter(|tab| workspace.tabs.iter().any(|item| item.id == *tab))
            {
                workspace.active_tab = tab;
            }
            for tab in &mut workspace.tabs {
                if let Some(pane) = saved
                    .focused
                    .iter()
                    .find(|(id, _)| *id == tab.id)
                    .map(|(_, pane)| *pane)
                    .filter(|pane| layout::contains(&tab.tree, *pane))
                {
                    tab.focused = pane;
                }
            }
        }
    }

    /// One-shot CLI requests share the persisted selection until a Hello
    /// establishes an interactive view. Preserve focus-then-act script flows.
    fn handle_script(&self, message: ClientMessage) -> Result<()> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        let renamed = matches!(message, ClientMessage::RenameSession { .. });
        let result = self.handle(message);
        drop(_dispatch);
        if renamed && result.is_ok() {
            self.save();
        }
        result
    }

    /// Dispatch one connection-scoped request through the legacy global
    /// mutation implementation without allowing that temporary selection to
    /// escape to a different socket or into persisted state.
    fn handle_view(&self, message: ClientMessage, view: &mut ClientView) -> Result<()> {
        if let ClientMessage::SetCompactView { enabled } = message {
            view.compact = enabled;
            return self.resize_for_view(view);
        }
        if let ClientMessage::Hello { cols, rows, .. } | ClientMessage::Resize { cols, rows } =
            message
        {
            view.cols = cols;
            view.rows = rows;
            return self.resize_for_view(view);
        }
        if let ClientMessage::ScrollPane { id, delta } = message {
            let _dispatch = self
                .view_dispatch
                .lock()
                .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
            self.ensure_mutation_open()?;
            let pane = self
                .panes
                .lock()
                .map_err(|_| anyhow!("pane lock poisoned"))?
                .get(&id)
                .cloned();
            if let Some(pane) = pane {
                let current = view.scroll.get(&id).copied().unwrap_or(0);
                view.scroll
                    .insert(id, pane.scroll_offset_after(delta, current));
            }
            self.notify();
            return Ok(());
        }
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        self.ensure_mutation_open()?;
        // Set the foreground application's dimensions before delivering input;
        // it may inspect its PTY immediately when the write wakes it.
        if matches!(message, ClientMessage::Input { .. }) {
            self.resize_view_locked(view)?;
        }
        // A view-changing operation becomes the PTY size arbiter. This is
        // under the same dispatch lock as selection staging, so another client
        // cannot publish its dimensions between the arbitration and dispatch.
        // Read-only queries deliberately do not resize a session another client
        // is using.
        if !matches!(
            message,
            ClientMessage::Query(_) | ClientMessage::Subscribe | ClientMessage::ReadPane { .. }
        ) {
            *self
                .size
                .lock()
                .map_err(|_| anyhow!("size lock poisoned"))? = (view.cols, view.rows);
        }
        let saved = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let saved = Self::save_selection(&state);
            Self::install_view(&mut state, view);
            saved
        };
        let result = self.handle(message.clone());
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?;
        Self::capture_view(&state, view);
        Self::restore_selection(&mut state, saved);
        if matches!(message, ClientMessage::Input { .. }) {
            if let Some(pane) = view
                .focused
                .get(view.tabs.get(&view.workspace).unwrap_or(&TabId(0)))
            {
                view.scroll.remove(pane);
            }
        }
        drop(state);
        if result.is_ok()
            && !matches!(
                message,
                ClientMessage::Query(_)
                    | ClientMessage::Input { .. }
                    | ClientMessage::Subscribe
                    | ClientMessage::ReadPane { .. }
                    | ClientMessage::KillSession
            )
        {
            self.resize_view_locked(view)?;
        }
        drop(_dispatch);
        if matches!(message, ClientMessage::RenameSession { .. }) && result.is_ok() {
            self.save();
        }
        result
    }
    fn notify(&self) {
        // Every mutation funnels through here (directly or via `resize`), so this
        // is the one place the persist generation needs to advance. PTY output
        // sends on `updates` without calling `notify`, so it never bumps it.
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        let _ = self.updates.send(());
    }

    fn track_pane_agent(
        &self,
        pane: &Pane,
        detection: &agent::Detection,
        process: &ProcessEvidence,
    ) -> u64 {
        let (generation, cleared_native) =
            pane.track_agent_identity(&detection.agent, process, detection.identity_from_hook);
        if cleared_native {
            // Retiring a conversation is a persistence mutation even when its
            // pane is hidden and terminal-output persistence is disabled.
            self.notify();
        }
        generation
    }

    /// Clear the restored flag once a client has attached (`Hello`).
    fn clear_restored(&self) {
        self.restored.store(false, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Result<LayoutSnapshot> {
        self.snapshot_for(None)
    }

    fn snapshot_stable(&self) -> Result<LayoutSnapshot> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        self.snapshot()
    }

    fn manifest_info(&self) -> Result<Vec<ManifestInfo>> {
        let manifests = self
            .manifests
            .lock()
            .map_err(|_| anyhow!("manifest lock poisoned"))?;
        Ok(manifests
            .iter()
            .map(|manifest| ManifestInfo {
                name: manifest.name.clone(),
                display: manifest.display.clone(),
                process: manifest.process.clone(),
                title: manifest.title.clone(),
                rules: manifest.rules.len(),
                source: manifest.source.label().into(),
            })
            .collect())
    }

    /// Parse the whole replacement set before holding the active cache lock.
    /// A bad local override therefore leaves the running detection behavior
    /// untouched until the user fixes it and retries reload.
    fn reload_manifests(&self) -> Result<Vec<ManifestInfo>> {
        let replacement = manifest::load()?;
        {
            let mut manifests = self
                .manifests
                .lock()
                .map_err(|_| anyhow!("manifest lock poisoned"))?;
            *manifests = replacement;
        }
        self.notify();
        self.manifest_info()
    }

    fn snapshot_for_client(&self, view: &ClientView) -> Result<LayoutSnapshot> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        self.snapshot_for(Some(view))
    }

    fn new_client_view(&self) -> Result<ClientView> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?;
        let (cols, rows) = *self
            .size
            .lock()
            .map_err(|_| anyhow!("size lock poisoned"))?;
        Ok(ClientView::from_state(&state, cols, rows))
    }

    fn view_selection<'a>(
        state: &'a SessionState,
        view: Option<&ClientView>,
    ) -> (&'a Workspace, &'a Tab, PaneId) {
        let workspace_id = view
            .map(|view| view.workspace)
            .filter(|id| state.workspaces.iter().any(|item| item.id == *id))
            .unwrap_or(state.active_workspace);
        let workspace = state
            .workspaces
            .iter()
            .find(|item| item.id == workspace_id)
            .expect("active workspace exists");
        let tab_id = view
            .and_then(|view| view.tabs.get(&workspace.id).copied())
            .filter(|id| workspace.tabs.iter().any(|item| item.id == *id))
            .unwrap_or(workspace.active_tab);
        let tab = workspace
            .tabs
            .iter()
            .find(|item| item.id == tab_id)
            .expect("active tab exists");
        let focused = view
            .and_then(|view| view.focused.get(&tab.id).copied())
            .filter(|id| layout::contains(&tab.tree, *id))
            .unwrap_or(tab.focused);
        (workspace, tab, focused)
    }

    /// Project shared panes through either the persisted/script selection or a
    /// connection's independent view.  Detection remains session-wide; only
    /// selection and scrollback are viewer state.
    fn snapshot_for(&self, view: Option<&ClientView>) -> Result<LayoutSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?;
        // Refresh git-derived sidebar metadata at most every 2 s. Snapshot
        // rendering itself must not canonicalize worktree paths per frame.
        refresh_workspace_metadata(&mut state, Instant::now());
        let (workspace, tab, focused) = Self::view_selection(&state, view);
        let tree = if tab.zoomed || view.is_some_and(|view| view.compact) {
            LayoutTree::Leaf { pane: focused }
        } else {
            tab.tree.clone()
        };
        let mut ids = Vec::new();
        // Compact clients still need every pane's identity for the switcher;
        // the projected tree controls visibility and physical PTY sizing.
        layout::leaves(
            if view.is_some_and(|view| view.compact) {
                &tab.tree
            } else {
                &tree
            },
            &mut ids,
        );
        let panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        let now = Instant::now();
        let manifests = self
            .manifests
            .lock()
            .map_err(|_| anyhow!("manifest lock poisoned"))?;
        let detections: HashMap<_, _> = panes
            .iter()
            .map(|(id, pane)| (*id, pane.detect(&manifests, now)))
            .collect();
        // Age is tracked once per snapshot, after detection settles on a state.
        // The same pass spots transitions into blocked/done and queues a
        // notification once (track_state's mutation makes it idempotent across
        // the concurrent snapshot calls of every attached client).
        let mut ages = HashMap::new();
        for (id, pane) in panes.iter() {
            let detection = &detections[id];
            self.track_pane_agent(pane, detection, &pane.process_evidence(now, false));
            let (previous, age) = pane.transition_state(detection.state, now);
            ages.insert(*id, age);
            // The `track_state` write above makes this fire once per real
            // transition even when several clients snapshot concurrently.
            if let Some(from) = previous.filter(|from| *from != detection.state) {
                self.emit(Event::AgentStateChanged {
                    pane: *id,
                    from,
                    to: detection.state,
                });
            }
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
                panes.get(&id).map(|pane| {
                    // The parser stays at its live screen between snapshots;
                    // this view's offset is installed only long enough to
                    // render its own historical viewport.
                    let offset = view
                        .and_then(|view| view.scroll.get(&id).copied())
                        .unwrap_or(0);
                    let (screen, scroll_offset) = pane.snapshot_at(offset);
                    PaneSnapshot {
                        id,
                        title: pane.title.lock().expect("title lock poisoned").clone(),
                        focused: id == focused,
                        scroll_offset,
                        screen,
                        agent: detections[&id].agent.clone(),
                        agent_generation: pane.agent_generation.load(Ordering::Relaxed),
                        activity_revision: pane.activity_revision.load(Ordering::Relaxed),
                        state: detections[&id].state,
                        state_reason: detections[&id].reason.clone(),
                        state_age_secs: ages[&id],
                        // `detect` already refreshed this pane's cache this tick.
                        cwd: pane.cwd(now),
                    }
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
                        color: item.color.clone(),
                        branch: item.branch.clone(),
                        parent: item.parent,
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

    /// Snapshot one pane wherever it lives, including background tabs and
    /// workspaces that `snapshot` (active tab only) leaves out. This is what
    /// `agent wait` / `pane wait-output` poll.
    fn pane_snapshot(&self, id: PaneId) -> Result<PaneSnapshot> {
        let (focused, location) = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let focused = state
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .any(|tab| tab.focused == id);
            (focused, locate_pane(&state, id))
        };
        let pane = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("pane {} not found", id.0))?;
        if location.is_none() {
            bail!("pane {} not found", id.0);
        }
        let now = Instant::now();
        let manifests = self
            .manifests
            .lock()
            .map_err(|_| anyhow!("manifest lock poisoned"))?;
        let detection = pane.detect(&manifests, now);
        let (previous, age) = pane.transition_state(detection.state, now);
        if let Some(from) = previous.filter(|from| *from != detection.state) {
            self.emit(Event::AgentStateChanged {
                pane: id,
                from,
                to: detection.state,
            });
        }
        let title = pane.title.lock().expect("title lock poisoned").clone();
        let (screen, scroll_offset) = pane.snapshot();
        Ok(PaneSnapshot {
            id,
            title,
            focused,
            scroll_offset,
            screen,
            agent: detection.agent.clone(),
            agent_generation: self.track_pane_agent(
                &pane,
                &detection,
                &pane.process_evidence(now, false),
            ),
            activity_revision: pane.activity_revision.load(Ordering::Relaxed),
            state: detection.state,
            state_reason: detection.reason.clone(),
            state_age_secs: age,
            cwd: pane.cwd(now),
        })
    }

    /// Validate the expected agent identity and generation immediately before
    /// writing. This closes the client-side query/write race for automation.
    fn prompt_agent(
        &self,
        id: PaneId,
        expected_agent: &str,
        expected_generation: u64,
        bytes: &[u8],
    ) -> Result<PaneSnapshot> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        self.ensure_mutation_open()?;
        if bytes.len() > MAX_PROMPT_BYTES {
            bail!("prompt exceeds the {MAX_PROMPT_BYTES}-byte automation limit");
        }
        let pane = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("pane {} not found", id.0))?;
        let now = Instant::now();
        let manifests = self
            .manifests
            .lock()
            .map_err(|_| anyhow!("manifest lock poisoned"))?;
        let (detection, process) = pane.detect_fresh(&manifests, now);
        let generation = self.track_pane_agent(&pane, &detection, &process);
        if detection.agent.as_deref() != Some(expected_agent) || generation != expected_generation {
            bail!("agent pane {} was replaced; resolve the target again", id.0);
        }
        if detection.state == AgentStateKind::Blocked {
            bail!("agent pane {} is blocked", id.0);
        }
        pane.write(bytes)?;
        drop(manifests);
        self.notify();
        self.pane_snapshot(id)
    }

    fn pane_snapshot_stable(&self, id: PaneId) -> Result<PaneSnapshot> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        self.pane_snapshot(id)
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
        let notification = queue.last().cloned().expect("just pushed");
        let overflow = queue.len().saturating_sub(64);
        if overflow > 0 {
            queue.drain(0..overflow);
        }
        drop(queue);
        // Subscribed connections read notifications off the event stream; #10's
        // `ServerMessage::Notification` stays for plain attached clients.
        self.emit(Event::Notification(notification));
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

    /// The most recently interacting client owns the physical PTY geometry.
    /// Other views retain their requested dimensions for reconnect/inspection,
    /// while terminal applications see one coherent size instead of a resize
    /// race on every output frame.
    fn resize_for_view(&self, view: &ClientView) -> Result<()> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        self.ensure_mutation_open()?;
        self.resize_view_locked(view)
    }

    fn resize_view_locked(&self, view: &ClientView) -> Result<()> {
        *self
            .size
            .lock()
            .map_err(|_| anyhow!("size lock poisoned"))? = (view.cols, view.rows);
        // Resizing needs only the layout tree, not a full screen/agent snapshot.
        let tree = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let (_, tab, focused) = Self::view_selection(&state, Some(view));
            if tab.zoomed || view.compact {
                LayoutTree::Leaf { pane: focused }
            } else {
                tab.tree.clone()
            }
        };
        let mut sizes = Vec::new();
        pane_sizes(
            &tree,
            view.cols.max(1),
            view.rows.saturating_sub(2).max(1),
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

    /// Read a pane's text for the client's copy mode / `pane read` (see
    /// `Pane::read_text`). Errors when the pane id is unknown.
    fn read_pane_text(
        &self,
        id: PaneId,
        scrollback: bool,
        lines: Option<usize>,
    ) -> Result<(String, usize)> {
        let panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        let pane = panes.get(&id).ok_or_else(|| anyhow!("no pane #{}", id.0))?;
        Ok(pane.read_text(scrollback, lines))
    }

    fn ensure_mutation_open(&self) -> Result<()> {
        if self.handoff_active.load(Ordering::Acquire) {
            bail!("daemon upgrade in progress");
        }
        Ok(())
    }

    fn handle(&self, message: ClientMessage) -> Result<()> {
        self.ensure_mutation_open()?;
        match message {
            // `Version` / `Session` / `Schema` queries, `Subscribe`, and
            // `ReadPane` are answered by `serve_client`; no state changes here.
            ClientMessage::Query(_)
            | ClientMessage::Subscribe
            | ClientMessage::ReadPane { .. }
            | ClientMessage::PromptAgent { .. } => {}
            ClientMessage::ReloadManifests => {
                self.reload_manifests()?;
            }
            ClientMessage::ApplyLayout(file) => self.apply_layout(file)?,
            ClientMessage::MovePaneToTab { pane, tab } => self.move_pane_to_tab(pane, tab)?,
            ClientMessage::RenameSession { name } => self.rename_session(&name)?,
            ClientMessage::Hello {
                cols,
                rows,
                version: _,
            }
            | ClientMessage::Resize { cols, rows } => self.resize(cols, rows)?,
            ClientMessage::SetCompactView { .. } => {}
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
                let pane = self.new_pane_with_id_replay(
                    self.pane_id(),
                    "shell",
                    cwd,
                    None,
                    None,
                    None,
                    self.workspace_env(active)?,
                )?;
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
            ClientMessage::NewWorktreeWorkspace {
                repo_root,
                branch,
                from,
                path,
            } => self.new_worktree_workspace(repo_root, branch, from, path)?,
            ClientMessage::OpenWorktreeWorkspace { repo_root, path } => {
                self.open_worktree_workspace(repo_root, path)?
            }
            ClientMessage::RemoveWorktreeWorkspace { id, keep } => {
                self.remove_worktree_workspace(id, keep)?
            }
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
            ClientMessage::PasteImage { .. } => {
                bail!("image paste is handled by the socket server")
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
            ClientMessage::Upgrade { .. } => bail!("live upgrade is handled by the socket server"),
            ClientMessage::NewTab => {
                let active = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?
                    .active_workspace;
                let cwd = self.inherit_cwd(active, None, None);
                let pane = self.new_workspace_pane(active, "shell", cwd, None)?;
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
                self.emit(Event::TabOpened { tab: id });
                self.resize_current()?;
            }
            ClientMessage::NewPane {
                workspace,
                tab,
                split,
                command,
                name,
                context,
            } => self.new_pane_message(
                workspace,
                tab,
                split,
                command,
                name,
                context.map(|context| *context),
            )?,
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
                let opened = break_pane(&mut state);
                drop(state);
                if let Some(tab) = opened {
                    self.emit(Event::TabOpened { tab });
                }
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
                    tab.name = name.clone();
                }
                drop(state);
                self.emit(Event::TabRenamed { tab: id, name });
                self.notify();
            }
            ClientMessage::RenameWorkspaceId { id, name } => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                if let Some(workspace) = state.workspaces.iter_mut().find(|item| item.id == id) {
                    workspace.name = name.clone();
                }
                drop(state);
                self.emit(Event::WorkspaceRenamed {
                    workspace: id,
                    name,
                });
                self.notify();
            }
            ClientMessage::SetWorkspaceColor { id, color } => {
                // Reject anything that is not `#` + 6 hex digits; `None` clears it.
                if let Some(hex) = &color {
                    if !is_hex_color(hex) {
                        bail!("workspace color must be #rrggbb, got {hex:?}");
                    }
                }
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?;
                if let Some(workspace) = state.workspaces.iter_mut().find(|item| item.id == id) {
                    workspace.color = color;
                }
                drop(state);
                self.notify();
            }
            ClientMessage::NewWorkspace { name, root, env } => {
                // A workspace root seeds its first pane's cwd; later panes inherit.
                validate_workspace_env(&env)?;
                let pane = self.new_pane_with_id_replay(
                    self.pane_id(),
                    "shell",
                    root.clone(),
                    None,
                    None,
                    None,
                    env.clone(),
                )?;
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
                    color: None,
                    env,
                    branch: None,
                    main_worktree_root: None,
                    parent: None,
                    metadata_checked_at: None,
                });
                state.active_workspace = id;
                drop(state);
                self.emit(Event::WorkspaceOpened { workspace: id });
                self.emit(Event::TabOpened { tab: tab_id });
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
                let tab = Self::active_tab_mut(&mut state);
                tab.name = name.clone();
                let id = tab.id;
                drop(state);
                self.emit(Event::TabRenamed { tab: id, name });
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
                    .name = name.clone();
                drop(state);
                self.emit(Event::WorkspaceRenamed {
                    workspace: active,
                    name,
                });
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
                native_session,
            } => {
                let panes = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?;
                let pane = panes
                    .get(&pane)
                    .ok_or_else(|| anyhow!("pane {} not found", pane.0))?;
                let hook_agent = trusted_hook_agent(&source, native_session.as_ref());
                let process = pane.process_evidence(Instant::now(), true);
                let mut hook = pane.hook.lock().expect("hook lock poisoned");
                let changed = hook.as_ref().is_none_or(|previous| previous.state != state);
                *hook = Some(ReportedHook {
                    state,
                    source,
                    agent: hook_agent,
                    process_pid: process.pid,
                    process_name: process.name,
                    reported_at: Instant::now(),
                });
                drop(hook);
                let pane = Arc::clone(pane);
                drop(panes);
                if let Some(native_session) = native_session.filter(valid_native_session) {
                    // Retire the previous foreground identity before assigning
                    // this report, so its next detection cannot erase a new ID.
                    let manifests = self.manifests.lock().expect("manifest lock poisoned");
                    let (detection, process) = pane.detect_fresh(&manifests, Instant::now());
                    pane.track_agent_identity(
                        &detection.agent,
                        &process,
                        detection.identity_from_hook,
                    );
                    *pane
                        .native_session
                        .lock()
                        .expect("native session lock poisoned") = Some(native_session);
                }
                if changed && matches!(state, AgentStateKind::Working | AgentStateKind::Blocked) {
                    // Hook reports are authoritative lifecycle transitions even
                    // before the next snapshot updates sidebar state.
                    pane.activity_revision.fetch_add(1, Ordering::Relaxed);
                }
                self.notify();
            }
        }
        Ok(())
    }

    fn close_pane(&self) -> Result<()> {
        let (needs_fresh, active) = {
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
            (
                workspace.tabs.len() == 1 && matches!(tab.tree, LayoutTree::Leaf { .. }),
                state.active_workspace,
            )
        };
        // Spawn before taking the state lock: id allocation also reads session state.
        let fresh_pane = needs_fresh
            .then(|| self.new_workspace_pane(active, "shell", None, None))
            .transpose()?;
        let removed;
        let mut events = Vec::new();
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
                    let closed = workspace.tabs.remove(tab_index).id;
                    workspace.active_tab = workspace.tabs[tab_index.saturating_sub(1)].id;
                    events.push(Event::TabClosed { tab: closed });
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
            self.emit(Event::PaneClosed { pane: id });
        }
        for event in events {
            self.emit(event);
        }
        self.resize_current()
    }

    fn close_tab(&self, id: TabId) -> Result<()> {
        let replacement_workspace = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            state
                .workspaces
                .iter()
                .find(|workspace| workspace.tabs.len() == 1 && workspace.tabs[0].id == id)
                .map(|workspace| workspace.id)
        };
        let fresh = replacement_workspace
            .map(|workspace| self.new_workspace_pane(workspace, "shell", None, None))
            .transpose()?;
        let fresh_tab = replacement_workspace.map(|_| self.tab_id());
        let mut events = Vec::new();
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
            events.push(Event::TabClosed { tab: id });
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
                events.push(Event::TabOpened { tab: tab_id });
            }
            workspace.active_tab = workspace.tabs[index.saturating_sub(1)].id;
            ids
        };
        let mut panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        for pane in pane_ids {
            panes.remove(&pane);
            self.emit(Event::PaneClosed { pane });
        }
        drop(panes);
        for event in events {
            self.emit(event);
        }
        self.resize_current()
    }

    /// Create a git-worktree workspace: run `git worktree add` under the
    /// `[worktrees] directory`, then open a workspace `repo:branch` rooted in the
    /// new worktree with a fresh shell tab (#22).
    fn new_worktree_workspace(
        &self,
        repo_root: PathBuf,
        branch: String,
        from: Option<String>,
        path: Option<PathBuf>,
    ) -> Result<()> {
        let repo = git::worktree_owner(&repo_root)
            .ok_or_else(|| anyhow!("{} is not inside a git repository", repo_root.display()))?;
        let basename = repo
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("repository path has no name"))?
            .to_owned();
        // `<worktrees.directory>/<repo-basename>/<branch>`; a slashed branch nests.
        let dest = path
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    repo.join(path)
                }
            })
            .unwrap_or_else(|| persist::worktrees_directory().join(&basename).join(&branch));
        git::worktree_add(&repo, &branch, from.as_deref(), &dest)?;

        let name = format!("{basename}:{branch}");
        let pane = self.new_pane("shell", Some(dest.clone()), None)?;
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
            root: Some(dest),
            color: None,
            env: HashMap::new(),
            branch: Some(branch),
            main_worktree_root: None,
            parent: None,
            metadata_checked_at: None,
        });
        state.active_workspace = id;
        drop(state);
        self.resize_current()
    }

    /// Open an already-registered linked worktree. Its directory is only read;
    /// cleanup still goes through Git's registered-worktree checks.
    fn open_worktree_workspace(&self, repo_root: PathBuf, path: PathBuf) -> Result<()> {
        let repo = git::worktree_owner(&repo_root)
            .ok_or_else(|| anyhow!("{} is not inside a git repository", repo_root.display()))?;
        let path = if path.is_absolute() {
            path
        } else {
            repo.join(path)
        };
        let dest = path
            .canonicalize()
            .with_context(|| format!("open worktree {}", path.display()))?;
        if git::main_worktree_root(&dest).is_none() || !git::registered_worktree(&repo, &dest) {
            bail!("{} is not a worktree of {}", dest.display(), repo.display());
        }
        let already_open = self
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?
            .workspaces
            .iter()
            .any(|workspace| workspace.root.as_ref() == Some(&dest));
        if already_open {
            bail!("worktree {} is already open", dest.display());
        }
        let branch =
            git::current_branch(&dest).ok_or_else(|| anyhow!("worktree has detached HEAD"))?;
        let basename = repo
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("repository path has no name"))?;
        let pane = self.new_pane("shell", Some(dest.clone()), None)?;
        let tab_id = self.tab_id();
        let id = self.workspace_id();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("state lock poisoned"))?;
        state.workspaces.push(Workspace {
            id,
            name: format!("{basename}:{branch}"),
            active_tab: tab_id,
            tabs: vec![Tab {
                id: tab_id,
                name: "shell".into(),
                tree: LayoutTree::Leaf { pane },
                focused: pane,
                zoomed: false,
            }],
            root: Some(dest),
            color: None,
            env: HashMap::new(),
            branch: Some(branch),
            main_worktree_root: None,
            parent: None,
            metadata_checked_at: None,
        });
        state.active_workspace = id;
        drop(state);
        self.resize_current()
    }

    /// Close a worktree workspace and, unless `keep`, `git worktree remove` its
    /// directory. The directory is only ever removed when it is a registered
    /// worktree of a known main repo, so nothing outside a worktree is deleted (#22).
    fn remove_worktree_workspace(&self, id: WorkspaceId, keep: bool) -> Result<()> {
        // Resolve the worktree's own root and its main repo before we close it.
        let root = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            state
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .and_then(|workspace| workspace.root.clone())
        };
        let removal = root
            .as_deref()
            .and_then(|root| git::main_worktree_root(root).map(|main| (main, root.to_path_buf())));
        // Refuse a dirty or otherwise unremovable checkout before changing the
        // session. A failed removal must leave the user's workspace and panes
        // available to fix the problem. Git can remove a registered worktree
        // even while a pane has it as its cwd on supported Unix platforms.
        if !keep {
            let Some((main, dest)) = removal else {
                bail!("workspace is not a linked git worktree; refusing to remove its directory")
            };
            git::worktree_remove(&main, &dest, false)?;
        }
        self.close_workspace(id)?;
        Ok(())
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
            .then(|| self.new_workspace_pane(id, "shell", None, None))
            .transpose()?;
        let fresh_tab = needs_fresh.then(|| self.tab_id());
        let mut events = Vec::new();
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
            let pane_ids = if state.workspaces.len() == 1 {
                // The last workspace is reset, not removed: its tabs go away and
                // a fresh one takes their place, so no `WorkspaceClosed` here.
                let workspace = &mut state.workspaces[index];
                let old_tabs = std::mem::take(&mut workspace.tabs);
                let mut ids = Vec::new();
                for tab in old_tabs {
                    layout::leaves(&tab.tree, &mut ids);
                    events.push(Event::TabClosed { tab: tab.id });
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
                events.push(Event::TabOpened { tab: tab_id });
                ids
            } else {
                let workspace = state.workspaces.remove(index);
                let mut ids = Vec::new();
                for tab in workspace.tabs {
                    layout::leaves(&tab.tree, &mut ids);
                    events.push(Event::TabClosed { tab: tab.id });
                }
                events.push(Event::WorkspaceClosed { workspace: id });
                state.active_workspace = state.workspaces[index.saturating_sub(1)].id;
                ids
            };
            // A cached child may have pointed at the workspace just removed.
            // Refresh all relationships on the next snapshot rather than
            // presenting a dangling parent for the cache interval.
            for workspace in &mut state.workspaces {
                workspace.metadata_checked_at = None;
            }
            pane_ids
        };
        let mut panes = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?;
        for pane in pane_ids {
            panes.remove(&pane);
            self.emit(Event::PaneClosed { pane });
        }
        drop(panes);
        for event in events {
            self.emit(event);
        }
        self.resize_current()
    }
}

impl Session {
    /// Move a pane into another existing tab, splitting that tab's focused pane.
    /// The pane keeps its PTY. A tab left empty by the move is closed, and a
    /// workspace that would be left with no tabs at all gets a fresh shell tab
    /// so later lookups (`active_tab`) always find one.
    fn move_pane_to_tab(&self, pane: PaneId, tab: TabId) -> Result<()> {
        // Decide up front whether the source workspace would be emptied; the
        // replacement pane has to be spawned before the state lock is taken.
        let replacement_workspace = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let source = locate_tab_of_pane(&state, pane);
            let target = locate_tab(&state, tab);
            match (source, target) {
                (Some((source_ws, source_tab)), Some((target_ws, _))) => {
                    let workspace = &state.workspaces[source_ws];
                    let only_leaf =
                        matches!(workspace.tabs[source_tab].tree, LayoutTree::Leaf { .. });
                    (only_leaf && workspace.tabs.len() == 1 && source_ws != target_ws)
                        .then_some(workspace.id)
                }
                _ => None,
            }
        };
        let fresh_pane = replacement_workspace
            .map(|workspace| self.new_workspace_pane(workspace, "shell", None, None))
            .transpose()?;
        let fresh_tab = replacement_workspace.map(|_| self.tab_id());
        let mut events = Vec::new();
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?;
            let Some((workspace_index, tab_index)) = locate_tab_of_pane(&state, pane) else {
                bail!("pane {} not found", pane.0);
            };
            if state.workspaces[workspace_index].tabs[tab_index].id == tab {
                return Ok(()); // Already there: nothing to move.
            }
            let Some((target_workspace, _)) = locate_tab(&state, tab) else {
                bail!("tab {} not found", tab.0);
            };
            // Detach from the source tab, dropping the tab when it empties.
            let workspace = &mut state.workspaces[workspace_index];
            match layout::close(workspace.tabs[tab_index].tree.clone(), pane) {
                Some(tree) => {
                    let source = &mut workspace.tabs[tab_index];
                    source.tree = tree;
                    source.zoomed = false;
                    let mut leaves = Vec::new();
                    layout::leaves(&source.tree, &mut leaves);
                    source.focused = leaves[0];
                }
                None if workspace.tabs.len() > 1 => {
                    let removed = workspace.tabs.remove(tab_index).id;
                    if workspace.active_tab == removed {
                        workspace.active_tab = workspace.tabs[tab_index.saturating_sub(1)].id;
                    }
                    events.push(Event::TabClosed { tab: removed });
                }
                // The source workspace's only tab: it cannot be left empty, so
                // it is replaced by a fresh shell tab (a workspace always has
                // at least one tab). Moving inside the same workspace would
                // instead destroy the tab we are moving into, so that is an error.
                None if workspace_index != target_workspace => {
                    let removed = workspace.tabs.remove(tab_index).id;
                    let pane = fresh_pane.expect("emptied workspace received a pane");
                    let tab_id = fresh_tab.expect("emptied workspace received a tab");
                    workspace.tabs.push(Tab {
                        id: tab_id,
                        name: "shell".into(),
                        tree: LayoutTree::Leaf { pane },
                        focused: pane,
                        zoomed: false,
                    });
                    workspace.active_tab = tab_id;
                    events.push(Event::TabClosed { tab: removed });
                    events.push(Event::TabOpened { tab: tab_id });
                }
                None => bail!("a workspace's last pane cannot be moved"),
            }
            // Re-resolve the target: removing a tab may have shifted indices.
            let workspace = &mut state.workspaces[target_workspace];
            let Some(target) = workspace.tabs.iter_mut().find(|item| item.id == tab) else {
                bail!("tab {} not found", tab.0);
            };
            let focused = target.focused;
            layout::split(&mut target.tree, focused, SplitAxis::Horizontal, pane);
            target.focused = pane;
            target.zoomed = false;
            workspace.active_tab = tab;
            let active = workspace.id;
            // A cross-workspace move follows the pane, like `pane focus` does.
            state.active_workspace = active;
        }
        for event in events {
            self.emit(event);
        }
        self.resize_current()
    }

    /// Rename a live session: the bound socket file and the state file move with
    /// it, so `-s NEW` reaches the same daemon and PTYs.
    fn rename_session(&self, new_name: &str) -> Result<()> {
        validate_session_name(new_name)?;
        let old_name = self.session_name();
        if old_name == new_name {
            return Ok(());
        }
        let old_socket = self.socket_path();
        let new_socket = socket_path(new_name);
        // Link-then-unlink instead of `exists()` + `rename`: `hard_link` fails
        // when the target exists, so two daemons cannot race onto one name.
        // The listener stays bound to the inode, so it answers on both paths
        // until the old one is unlinked.
        match fs::hard_link(&old_socket, &new_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("session '{new_name}' already exists");
            }
            Err(error) => {
                return Err(error).context("link the session socket to its new name");
            }
        }
        fs::remove_file(&old_socket).context("remove the old session socket")?;
        *self.socket.lock().expect("socket lock poisoned") = new_socket.clone();
        *self.name.lock().expect("name lock poisoned") = new_name.to_owned();
        if let Some(path) = persist::session_file_path(&old_name) {
            let _ = fs::remove_file(path);
        }
        history::rename_for_session(&old_name, new_name);
        self.emit(Event::SessionRenamed {
            name: new_name.to_owned(),
            socket: new_socket,
        });
        self.notify();
        Ok(())
    }

    fn socket_path(&self) -> PathBuf {
        self.socket.lock().expect("socket lock poisoned").clone()
    }

    fn paste_image(&self, id: PaneId, data: &str) -> Result<PathBuf> {
        let _dispatch = self
            .view_dispatch
            .lock()
            .map_err(|_| anyhow!("view dispatch lock poisoned"))?;
        self.ensure_mutation_open()?;
        let pane = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("pane {} not found", id.0))?;
        let path = self.images.save(data)?;
        let text = format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let bracketed = pane
            .parser
            .lock()
            .expect("parser lock")
            .screen()
            .bracketed_paste();
        let bytes = if bracketed {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        if let Err(error) = pane.write(&bytes) {
            self.images.discard(&path);
            return Err(error);
        }
        pane.reset_scrollback();
        self.notify();
        Ok(path)
    }

    /// Replace the layout with a persisted one (`layout apply`). Panes whose ids
    /// are still alive keep their PTY; every other pane named by a saved tree is
    /// spawned fresh through the same path a cold restore uses — including the
    /// saved command, run through the login shell in the saved cwd. Panes the
    /// file does not mention are closed. A spawn failure leaves the session
    /// untouched (the panes spawned so far are dropped).
    fn apply_layout(&self, file: kodade_cli_proto::SessionFile) -> Result<()> {
        file.validate()?;
        let live: Vec<PaneId> = self
            .panes
            .lock()
            .map_err(|_| anyhow!("pane lock poisoned"))?
            .keys()
            .copied()
            .collect();
        let mut spawned: Vec<PaneId> = Vec::new();
        match self.build_applied_workspaces(&file, &live, &mut spawned) {
            Ok((workspaces, used, highest)) => {
                let (before_tabs, before_workspaces) = {
                    let state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow!("state lock poisoned"))?;
                    let tabs: Vec<TabId> = state
                        .workspaces
                        .iter()
                        .flat_map(|workspace| workspace.tabs.iter().map(|tab| tab.id))
                        .collect();
                    let ids: Vec<WorkspaceId> =
                        state.workspaces.iter().map(|item| item.id).collect();
                    (tabs, ids)
                };
                let after_tabs: Vec<TabId> = workspaces
                    .iter()
                    .flat_map(|workspace| workspace.tabs.iter().map(|tab| tab.id))
                    .collect();
                let after_workspaces: Vec<WorkspaceId> =
                    workspaces.iter().map(|item| item.id).collect();
                let active_workspace = workspaces
                    .iter()
                    .find(|workspace| workspace.id == WorkspaceId(file.active_workspace))
                    .map(|workspace| workspace.id)
                    .unwrap_or(workspaces[0].id);
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow!("state lock poisoned"))?;
                    state.workspaces = workspaces;
                    state.active_workspace = active_workspace;
                    // Ids from the file are now in use, so never hand them out again.
                    state.next_id = state.next_id.max(highest);
                }
                let mut panes = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?;
                for pane in live.into_iter().filter(|id| !used.contains(id)) {
                    panes.remove(&pane);
                    self.emit(Event::PaneClosed { pane });
                }
                drop(panes);
                for tab in before_tabs.iter().filter(|id| !after_tabs.contains(id)) {
                    self.emit(Event::TabClosed { tab: *tab });
                }
                for workspace in before_workspaces
                    .iter()
                    .filter(|id| !after_workspaces.contains(id))
                {
                    self.emit(Event::WorkspaceClosed {
                        workspace: *workspace,
                    });
                }
                for workspace in after_workspaces
                    .iter()
                    .filter(|id| !before_workspaces.contains(id))
                {
                    self.emit(Event::WorkspaceOpened {
                        workspace: *workspace,
                    });
                }
                for tab in after_tabs.iter().filter(|id| !before_tabs.contains(id)) {
                    self.emit(Event::TabOpened { tab: *tab });
                }
                self.resize_current()
            }
            Err(error) => {
                // Roll back: the session state was never touched, so dropping the
                // panes spawned so far restores the pre-apply world exactly.
                let mut panes = self
                    .panes
                    .lock()
                    .map_err(|_| anyhow!("pane lock poisoned"))?;
                for pane in &spawned {
                    panes.remove(pane);
                }
                drop(panes);
                for pane in spawned {
                    self.emit(Event::PaneClosed { pane });
                }
                Err(error)
            }
        }
    }

    /// Build the workspace list `apply_layout` will install, spawning the panes
    /// a saved tree names that are not already alive. Records every spawn in
    /// `spawned` so the caller can undo them when a later spawn fails.
    #[allow(clippy::type_complexity)]
    fn build_applied_workspaces(
        &self,
        file: &kodade_cli_proto::SessionFile,
        live: &[PaneId],
        spawned: &mut Vec<PaneId>,
    ) -> Result<(Vec<Workspace>, Vec<PaneId>, u64)> {
        let mut workspaces = Vec::new();
        let mut used: Vec<PaneId> = Vec::new();
        let mut highest = 0;
        for saved in &file.workspaces {
            let mut tabs = Vec::new();
            for saved_tab in &saved.tabs {
                // The tree is the layout of record; `panes` only carries the
                // metadata for the ids it names (validate keeps them in step).
                let mut leaves = Vec::new();
                layout::leaves(&saved_tab.tree, &mut leaves);
                for id in &leaves {
                    highest = highest.max(id.0);
                    used.push(*id);
                    if live.contains(id) {
                        continue;
                    }
                    let saved_pane = saved_tab
                        .panes
                        .iter()
                        .find(|pane| pane.id == id.0)
                        .ok_or_else(|| {
                            anyhow!("tab {} has no entry for pane {}", saved_tab.id, id.0)
                        })?;
                    let cwd = restore_cwd(saved_pane.cwd.clone(), saved.root.clone());
                    self.new_pane_with_id_replay(
                        *id,
                        &saved_pane.title,
                        cwd,
                        saved_pane.command.clone(),
                        None,
                        None,
                        saved.env.clone(),
                    )?;
                    spawned.push(*id);
                }
                let focused = if leaves.contains(&PaneId(saved_tab.focused)) {
                    PaneId(saved_tab.focused)
                } else {
                    leaves[0]
                };
                highest = highest.max(saved_tab.id);
                tabs.push(Tab {
                    id: TabId(saved_tab.id),
                    name: saved_tab.name.clone(),
                    tree: saved_tab.tree.clone(),
                    focused,
                    zoomed: saved_tab.zoomed,
                });
            }
            highest = highest.max(saved.id);
            let active_tab = tabs
                .iter()
                .find(|tab| tab.id == TabId(saved.active_tab))
                .map(|tab| tab.id)
                .unwrap_or(tabs[0].id);
            workspaces.push(Workspace {
                id: WorkspaceId(saved.id),
                name: saved.name.clone(),
                tabs,
                active_tab,
                root: saved.root.clone(),
                color: saved.color.clone(),
                env: saved.env.clone(),
                branch: None,
                main_worktree_root: None,
                parent: None,
                metadata_checked_at: None,
            });
        }
        Ok((workspaces, used, highest))
    }
}

/// Index of the workspace and tab holding `pane`, if any.
fn locate_tab_of_pane(state: &SessionState, pane: PaneId) -> Option<(usize, usize)> {
    state
        .workspaces
        .iter()
        .enumerate()
        .find_map(|(workspace, item)| {
            item.tabs
                .iter()
                .position(|item| layout::contains(&item.tree, pane))
                .map(|index| (workspace, index))
        })
}

/// Index of the workspace and tab with id `tab`, if any. Tab ids are global, so
/// this searches every workspace.
fn locate_tab(state: &SessionState, tab: TabId) -> Option<(usize, usize)> {
    state
        .workspaces
        .iter()
        .enumerate()
        .find_map(|(index, item)| {
            item.tabs
                .iter()
                .position(|item| item.id == tab)
                .map(|position| (index, position))
        })
}

/// Refresh git-derived sidebar metadata at most every two seconds. Parent
/// relationships are calculated from this cache, so hot screen snapshots do no
/// filesystem work beyond the timed refresh.
fn refresh_workspace_metadata(state: &mut SessionState, now: Instant) {
    let mut refreshed = false;
    for workspace in &mut state.workspaces {
        let Some(root) = workspace.root.clone() else {
            workspace.branch = None;
            workspace.main_worktree_root = None;
            workspace.parent = None;
            continue;
        };
        let stale = workspace
            .metadata_checked_at
            .map(|at| now.saturating_duration_since(at) >= Duration::from_secs(2))
            .unwrap_or(true);
        if stale {
            workspace.branch = git::branch_of(&root);
            workspace.main_worktree_root = git::main_worktree_root(&root);
            workspace.metadata_checked_at = Some(now);
            refreshed = true;
        }
    }
    if !refreshed {
        return;
    }
    let roots: Vec<_> = state
        .workspaces
        .iter()
        .filter_map(|workspace| workspace.root.clone().map(|root| (workspace.id, root)))
        .collect();
    for workspace in &mut state.workspaces {
        workspace.parent = workspace.main_worktree_root.as_deref().and_then(|main| {
            roots
                .iter()
                .find(|(id, root)| *id != workspace.id && same_dir(root, main))
                .map(|(id, _)| *id)
        });
    }
}

/// Whether two paths point at the same directory, comparing canonical forms so
/// symlinks and `..` segments do not defeat the match.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
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
fn break_pane(state: &mut SessionState) -> Option<TabId> {
    let active = state.active_workspace;
    let workspace = state.workspaces.iter_mut().find(|item| item.id == active)?;
    let index = workspace
        .tabs
        .iter()
        .position(|tab| tab.id == workspace.active_tab)?;
    let source = &mut workspace.tabs[index];
    let pane = source.focused;
    let tree = layout::close(source.tree.clone(), pane)?;
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
    Some(id)
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
        hook_socket: PathBuf,
        updates: broadcast::Sender<()>,
        output_generation: Arc<AtomicU64>,
        cwd: Option<PathBuf>,
        run: Option<Vec<String>>,
        context_file: Option<ContextFile>,
        replay: Option<history::PaneHistory>,
        environment: HashMap<String, String>,
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
        for (key, value) in environment {
            command.env(key, value);
        }
        // Ködade's routing identity remains authoritative over workspace env.
        command.env("KODADE_PANE", id.0.to_string());
        command.env("KODADE_SOCKET", hook_socket);
        command.env("KODADE_SESSION", session);
        if let Some(context_file) = &context_file {
            command.env("KODADE_PLUGIN_CONTEXT", &context_file.path);
            command.env("KODADE_PLUGIN_CONTEXT_FORMAT", "json");
        }
        // Scripts inside a pane call the same binary that hosts them.
        if let Ok(exe) = env::current_exe() {
            command.env("KODADE_BIN", exe);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .context("spawn login shell in PTY")?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        // `dup` gives the poll loop its own descriptor. We must not toggle
        // O_NONBLOCK on a cloned open-file description because that would also
        // change the PTY writer used for pane input.
        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow!("PTY master has no Unix descriptor"))?;
        let reader_fd = unsafe { libc::fcntl(master_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if reader_fd < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicate PTY reader");
        }
        let reader = unsafe { fs::File::from_raw_fd(reader_fd) };
        let mut restored_parser =
            PtyParser::new_with_callbacks(rows, cols, 10_000, PtyCallbacks::default());
        if let Some(replay) = replay.as_ref() {
            restore_screen(&mut restored_parser, &replay.screen, &replay.text);
        }
        let parser = Arc::new(Mutex::new(restored_parser));
        let last_output = Arc::new(Mutex::new(Instant::now()));
        let reader_control = Arc::new(ReaderControl::new());
        read_pty(
            reader,
            Arc::clone(&parser),
            Arc::clone(&last_output),
            Arc::clone(&writer),
            updates,
            Arc::clone(&reader_control),
            output_generation,
        );
        Ok(Self {
            title: Mutex::new(title.into()),
            writer,
            master: Mutex::new(pair.master),
            parser,
            last_output,
            reader: reader_control,
            hook: Mutex::new(None),
            spawn_process,
            spawn_command: run,
            spawn_cwd: cwd,
            child: Mutex::new(Some(child)),
            adopted_child: None,
            adopted_child_owned: AtomicBool::new(false),
            native_session: Mutex::new(None),
            process: Mutex::new(ProcessEvidence {
                pid: None,
                name: None,
                cwd: None,
                checked_at: Instant::now() - Duration::from_secs(2),
            }),
            state: Mutex::new(PaneState {
                last: None,
                since: Instant::now(),
            }),
            agent_generation: AtomicU64::new(0),
            agent_identity: Mutex::new(None),
            activity_revision: AtomicU64::new(0),
            _context_file: context_file,
        })
    }
    fn snapshot(&self) -> (Screen, usize) {
        let parser = self.parser.lock().expect("PTY parser lock poisoned");
        (snapshot(&parser), parser.screen().scrollback())
    }

    /// Freeze this pane's reader and capture only bounded, replayable terminal
    /// state. The caller owns resuming it on every failed handoff path.
    fn capture_handoff(&self) -> Result<(handoff::PaneRuntime, RawFd)> {
        self.reader.pause(Duration::from_secs(2))?;
        let result = (|| {
            let fd = self
                .master
                .lock()
                .map_err(|_| anyhow!("PTY master lock poisoned"))?
                .as_raw_fd()
                .ok_or_else(|| anyhow!("PTY master has no Unix descriptor"))?;
            let parser = self
                .parser
                .lock()
                .map_err(|_| anyhow!("PTY parser lock poisoned"))?;
            let (screen_ansi, history) = terminal_handoff(&parser)?;
            let child_pid = self
                .child
                .lock()
                .map_err(|_| anyhow!("child lock poisoned"))?
                .as_ref()
                .and_then(|child| child.process_id())
                .map(|pid| pid as i32)
                .or_else(|| self.adopted_child.as_ref().map(|(pid, _)| *pid));
            let now = Instant::now();
            let output_age_ms = now
                .saturating_duration_since(
                    *self
                        .last_output
                        .lock()
                        .map_err(|_| anyhow!("output lock poisoned"))?,
                )
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            let hook = self
                .hook
                .lock()
                .map_err(|_| anyhow!("hook lock poisoned"))?
                .clone();
            let state_guard = self
                .state
                .lock()
                .map_err(|_| anyhow!("pane state lock poisoned"))?;
            let state = state_guard.last;
            let state_age_ms = now
                .saturating_duration_since(state_guard.since)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            Ok((
                handoff::PaneRuntime {
                    pane_id: 0,
                    child_pid: child_pid.unwrap_or_default(),
                    rows: parser.screen().size().0,
                    cols: parser.screen().size().1,
                    start_identity: child_pid.and_then(proc::start_identity),
                    terminal_title: parser.callbacks().title.clone(),
                    title: self
                        .title
                        .lock()
                        .map_err(|_| anyhow!("title lock poisoned"))?
                        .clone(),
                    spawn_command: self.spawn_command.clone(),
                    native_session: self
                        .native_session
                        .lock()
                        .map_err(|_| anyhow!("native session lock poisoned"))?
                        .clone(),
                    context_file: self
                        ._context_file
                        .as_ref()
                        .map(|context| context.path.clone()),
                    cwd: self.saved_cwd(),
                    agent_identity: self
                        .agent_identity
                        .lock()
                        .map_err(|_| anyhow!("agent identity lock poisoned"))?
                        .clone(),
                    agent_generation: self.agent_generation.load(Ordering::Relaxed),
                    activity_revision: self.activity_revision.load(Ordering::Relaxed),
                    output_age_ms,
                    state_age_ms,
                    hook_age_ms: hook
                        .as_ref()
                        .map(|hook| {
                            now.saturating_duration_since(hook.reported_at)
                                .as_millis()
                                .min(u128::from(u64::MAX)) as u64
                        })
                        .unwrap_or(0),
                    hook_agent: hook.as_ref().and_then(|hook| hook.agent.clone()),
                    hook_process_pid: hook.as_ref().and_then(|hook| hook.process_pid),
                    hook_process_name: hook.as_ref().and_then(|hook| hook.process_name.clone()),
                    hook_state: hook.as_ref().map(|hook| hook.state),
                    hook_source: hook.map(|hook| hook.source),
                    state,
                    screen_ansi,
                    history_ansi: history,
                    graphics: graphics::HandoffState::capture(
                        &parser.callbacks().graphics,
                        &parser.callbacks().graphics_decoder,
                        &parser.callbacks().graphics_tracker,
                    )?,
                },
                fd,
            ))
        })();
        if result.is_err() {
            self.reader.resume();
        }
        result
    }

    /// Rebuild a pane around an SCM_RIGHTS master. It deliberately starts
    /// paused: the source continues reading until the ownership stage commits.
    unsafe fn import_handoff(
        runtime: handoff::PaneRuntime,
        fd: RawFd,
        updates: broadcast::Sender<()>,
        output_generation: Arc<AtomicU64>,
    ) -> Result<Self> {
        let master: Box<dyn MasterPty + Send> = Box::new(handoff::ImportedMaster::from_raw_fd(fd));
        let writer = Arc::new(Mutex::new(master.take_writer()?));
        let reader_fd = libc::fcntl(
            master
                .as_raw_fd()
                .ok_or_else(|| anyhow!("imported master has no fd"))?,
            libc::F_DUPFD_CLOEXEC,
            0,
        );
        if reader_fd < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicate imported PTY reader");
        }
        let reader = fs::File::from_raw_fd(reader_fd);
        let mut parser = PtyParser::new_with_callbacks(
            runtime.rows,
            runtime.cols,
            10_000,
            PtyCallbacks::default(),
        );
        parser.process(runtime.history_ansi.as_bytes());
        parser.process(&runtime.screen_ansi);
        parser.process(runtime.graphics.pending_text());
        let (graphics, decoder, tracker) = runtime.graphics.restore();
        parser.callbacks_mut().graphics = graphics;
        parser.callbacks_mut().graphics_decoder = decoder;
        parser.callbacks_mut().graphics_tracker = tracker;
        parser.callbacks_mut().title = runtime.terminal_title;
        let parser = Arc::new(Mutex::new(parser));
        let last_output = Arc::new(Mutex::new(
            Instant::now()
                .checked_sub(Duration::from_millis(runtime.output_age_ms))
                .unwrap_or_else(Instant::now),
        ));
        let context_file = runtime.context_file.map(ContextFile::import).transpose()?;
        let reader_control = Arc::new(ReaderControl::paused());
        read_pty(
            reader,
            Arc::clone(&parser),
            Arc::clone(&last_output),
            Arc::clone(&writer),
            updates,
            Arc::clone(&reader_control),
            output_generation,
        );
        Ok(Self {
            title: Mutex::new(runtime.title),
            writer,
            master: Mutex::new(master),
            parser,
            last_output,
            reader: reader_control,
            hook: Mutex::new(
                runtime
                    .hook_state
                    .zip(runtime.hook_source)
                    .map(|(state, source)| ReportedHook {
                        state,
                        source,
                        agent: runtime.hook_agent,
                        process_pid: runtime.hook_process_pid,
                        process_name: runtime.hook_process_name,
                        reported_at: Instant::now()
                            .checked_sub(Duration::from_millis(runtime.hook_age_ms))
                            .unwrap_or_else(Instant::now),
                    }),
            ),
            spawn_process: runtime
                .spawn_command
                .as_ref()
                .and_then(|command| command.first())
                .and_then(|command| proc::process_basename(command))
                .unwrap_or_else(|| "unknown".into()),
            spawn_command: runtime.spawn_command,
            spawn_cwd: runtime.cwd,
            native_session: Mutex::new(runtime.native_session.filter(valid_native_session)),
            _context_file: context_file,
            child: Mutex::new(None),
            adopted_child_owned: AtomicBool::new(false),
            adopted_child: runtime
                .start_identity
                .map(|start| (runtime.child_pid, start)),
            process: Mutex::new(ProcessEvidence {
                pid: (runtime.child_pid > 0).then_some(runtime.child_pid),
                name: None,
                cwd: None,
                checked_at: Instant::now() - Duration::from_secs(2),
            }),
            state: Mutex::new(PaneState {
                last: runtime.state,
                since: Instant::now()
                    .checked_sub(Duration::from_millis(runtime.state_age_ms))
                    .unwrap_or_else(Instant::now),
            }),
            agent_generation: AtomicU64::new(runtime.agent_generation),
            agent_identity: Mutex::new(runtime.agent_identity),
            activity_revision: AtomicU64::new(runtime.activity_revision),
        })
    }
    /// Render a requested historical offset without leaving the shared parser
    /// scrolled for another client. `set_scrollback` clamps to available history.
    fn snapshot_at(&self, offset: usize) -> (Screen, usize) {
        let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
        parser.screen_mut().set_scrollback(offset);
        let actual = parser.screen().scrollback();
        let screen = snapshot(&parser);
        parser.screen_mut().set_scrollback(0);
        (screen, actual)
    }
    /// Full pane text for copy mode / `pane read`. With `scrollback`, walks the
    /// vt100 history and appends the visible screen; otherwise just the visible
    /// screen. `lines` keeps the last N lines. Returns the text and its line
    /// count. The pane's live scroll offset is saved and restored.
    fn read_text(&self, scrollback: bool, lines: Option<usize>) -> (String, usize) {
        let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
        let offset = parser.screen().scrollback();
        let mut history = if scrollback {
            read_history(&mut parser)
        } else {
            parser.screen_mut().set_scrollback(offset);
            parser
                .screen()
                .contents()
                .lines()
                .map(String::from)
                .collect()
        };
        // Restore the live view the client scrolled to.
        parser.screen_mut().set_scrollback(offset);
        drop(parser);
        // Drop trailing blank rows so `pane read` line counts track real output.
        while history.last().is_some_and(|line| line.is_empty()) {
            history.pop();
        }
        if let Some(n) = lines {
            if history.len() > n {
                history.drain(0..history.len() - n);
            }
        }
        let count = history.len();
        (history.join("\n"), count)
    }
    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if self
            .parser
            .lock()
            .map_err(|_| anyhow!("PTY parser lock poisoned"))?
            .screen()
            .size()
            == (rows, cols)
        {
            return Ok(());
        }
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
        let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
        let offset = scroll_offset_after_delta(parser.screen().scrollback(), delta, usize::MAX);
        parser.screen_mut().set_scrollback(offset);
    }
    fn scroll_offset_after(&self, delta: i16, current: usize) -> usize {
        let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
        parser.screen_mut().set_scrollback(current);
        let offset = scroll_offset_after_delta(parser.screen().scrollback(), delta, usize::MAX);
        parser.screen_mut().set_scrollback(offset);
        let actual = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(0);
        actual
    }
    fn reset_scrollback(&self) {
        self.parser
            .lock()
            .expect("PTY parser lock poisoned")
            .screen_mut()
            .set_scrollback(0);
    }
    fn detect(&self, manifests: &[manifest::Manifest], now: Instant) -> agent::Detection {
        self.detect_with_evidence(manifests, now, false).0
    }

    /// Probe the PTY process group immediately. Guarded automation calls this
    /// path so its identity check is not protected by the ordinary 2 s cache.
    fn detect_fresh(
        &self,
        manifests: &[manifest::Manifest],
        now: Instant,
    ) -> (agent::Detection, ProcessEvidence) {
        self.detect_with_evidence(manifests, now, true)
    }

    fn detect_with_evidence(
        &self,
        manifests: &[manifest::Manifest],
        now: Instant,
        fresh_process: bool,
    ) -> (agent::Detection, ProcessEvidence) {
        let (screen, title) = {
            let mut parser = self.parser.lock().expect("PTY parser lock poisoned");
            let screen = live_screen_contents(&mut parser);
            (screen, parser.callbacks().title.clone())
        };
        // portable-pty obtains the foreground process-group leader from the PTY itself.
        // `ps` turns that portable pid into a basename without sysctl; unavailable leaders fall
        // back to the login-shell process captured at spawn, with OSC terminal title as evidence.
        let process = self.process_evidence(now, fresh_process);
        let last_output = *self.last_output.lock().expect("output lock poisoned");
        let hook = self
            .hook
            .lock()
            .expect("hook lock poisoned")
            .clone()
            .map(|hook| agent::HookState {
                state: hook.state,
                source: hook.source,
                agent: hook.agent,
                process_pid: hook.process_pid,
                process_name: hook.process_name,
                age: now.saturating_duration_since(hook.reported_at),
                // A `done` report is released once the pane prints anything new.
                output_since_report: last_output > hook.reported_at,
            });
        let output_age = now.saturating_duration_since(last_output);
        let mut detection = agent::detect(
            manifests,
            process.name.as_deref().or(Some(&self.spawn_process)),
            process.pid,
            &title,
            &screen,
            output_age,
            hook,
        );
        // A hook-backed adapter can run under Node/Python. Once it exits to a
        // shell, ignore a stale title as an additional conservative guard.
        if process.name.as_deref().is_some_and(agent::is_shell)
            && self
                .agent_identity
                .lock()
                .expect("agent identity lock poisoned")
                .as_deref()
                .is_some_and(|identity| identity.ends_with("|hook=true"))
        {
            detection.agent = None;
            detection.state = AgentStateKind::Idle;
            detection.reason = "foreground shell after agent exit".into();
            detection.from_hook = false;
            detection.identity_from_hook = false;
        }
        (detection, process)
    }

    /// Return the generation associated with this detection. The first known
    /// agent receives generation 1; a shell/replacement then a new agent always
    /// receives a later value, which makes stale prompt guards fail closed.
    fn track_agent_identity(
        &self,
        agent: &Option<String>,
        process: &ProcessEvidence,
        from_hook: bool,
    ) -> (u64, bool) {
        let mut identity = self
            .agent_identity
            .lock()
            .expect("agent identity lock poisoned");
        // A display label alone cannot distinguish two consecutive Codex
        // processes, or a shell retaining an old OSC title. Include the live
        // foreground PID/process when the platform can expose it.
        let next = agent.as_ref().map(|agent| {
            format!(
                "{agent}|pid={}|process={}|hook={from_hook}",
                process
                    .pid
                    .map_or_else(|| "unknown".into(), |pid| pid.to_string()),
                process.name.as_deref().unwrap_or("unknown")
            )
        });
        let mut cleared_native = false;
        if *identity != next {
            // A native session belongs to the foreground agent process. Once it
            // exits or is replaced, do not let its old conversation resurrect.
            if identity.is_some() {
                cleared_native = self
                    .native_session
                    .lock()
                    .expect("native session lock poisoned")
                    .take()
                    .is_some();
            }
            *identity = next;
            self.agent_generation.fetch_add(1, Ordering::Relaxed);
        }
        (
            self.agent_generation.load(Ordering::Relaxed),
            cleared_native,
        )
    }

    /// Record one detection atomically and return the preceding state plus age.
    fn transition_state(
        &self,
        state: AgentStateKind,
        now: Instant,
    ) -> (Option<AgentStateKind>, u64) {
        self.state
            .lock()
            .expect("pane state lock poisoned")
            .transition(state, now)
    }

    fn process_evidence(&self, now: Instant, force: bool) -> ProcessEvidence {
        self.refresh_process(now, force);
        self.process.lock().expect("process lock poisoned").clone()
    }

    fn cwd(&self, now: Instant) -> Option<PathBuf> {
        self.refresh_process(now, false);
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
    fn refresh_process(&self, now: Instant, force: bool) {
        let mut process = self.process.lock().expect("process lock poisoned");
        if !force && now.saturating_duration_since(process.checked_at) < Duration::from_secs(2) {
            return;
        }
        let pid = self
            .master
            .lock()
            .expect("PTY master lock poisoned")
            .process_group_leader();
        if let Some(pid) = pid {
            process.pid = Some(pid);
            process.name = proc::command_of(pid)
                .as_deref()
                .and_then(proc::process_basename);
            process.cwd = proc::cwd_of(pid);
        } else {
            process.pid = None;
            process.name = None;
            process.cwd = None;
        }
        process.checked_at = now;
    }
}

fn validate_workspace_env(env: &HashMap<String, String>) -> Result<()> {
    for key in env.keys() {
        let valid = key.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || (byte.is_ascii_alphabetic()) || (index > 0 && byte.is_ascii_digit())
        });
        if !valid {
            bail!("workspace environment key must be a shell-style identifier: {key:?}");
        }
        if matches!(
            key.as_str(),
            "KODADE_PANE" | "KODADE_SOCKET" | "KODADE_SESSION" | "KODADE_BIN"
        ) {
            bail!("workspace environment cannot override reserved key {key}");
        }
    }
    if env.values().any(|value| value.contains('\0')) {
        bail!("workspace environment values cannot contain NUL");
    }
    Ok(())
}

/// Directory a restored pane should start in: its saved cwd if it still exists,
/// else the workspace root, else the pane spawn's own default (home / `$SHELL`).
fn restore_cwd(saved: Option<PathBuf>, root: Option<PathBuf>) -> Option<PathBuf> {
    saved
        .filter(|path| path.is_dir())
        .or_else(|| root.filter(|path| path.is_dir()))
}

/// The command a restored pane should run. Native conversation references are
/// explicit per pane; the old manifest `--last` fallback is deliberately not
/// used because two agents in one cwd would otherwise restore the same session.
fn resume_command(
    pane: &persist::PaneFile,
    resume_agents: bool,
    resumed: &mut HashSet<String>,
) -> Option<Vec<String>> {
    if !resume_agents {
        return None;
    }
    let native = pane
        .native_session
        .as_ref()
        .filter(|native| valid_native_session(native))?;
    let argv = native_resume_argv(native)?;
    let key = native_session_key(native);
    resumed.insert(key).then_some(argv)
}

fn native_session_key(native: &NativeSession) -> String {
    format!(
        "{}\0{}\0{}",
        native.agent,
        native.id.as_deref().unwrap_or_default(),
        native
            .path
            .as_deref()
            .and_then(Path::to_str)
            .unwrap_or_default()
    )
}

fn valid_native_session(native: &NativeSession) -> bool {
    const MAX_VALUE: usize = 4096;
    let valid_text = |value: &str| {
        !value.is_empty()
            && value.len() <= MAX_VALUE
            && !value.starts_with('-')
            && !value
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
    };
    let valid_id = native.id.as_deref().is_some_and(valid_text);
    let valid_path = native.path.as_ref().is_some_and(|path| {
        path.is_absolute()
            && path.is_file()
            && path.as_os_str().len() <= MAX_VALUE
            && !path.to_string_lossy().chars().any(char::is_control)
    });
    matches!(
        (
            native.source.as_str(),
            native.agent.as_str(),
            valid_id,
            valid_path
        ),
        ("kodade:claude-code", "claude", true, false)
            | ("kodade:codex", "codex", true, false)
            | ("kodade:gemini-cli", "gemini", true, false)
            | ("kodade:opencode", "opencode", true, false)
            | ("kodade:omp", "omp", true, false)
            | ("kodade:omp", "omp", false, true)
            | ("kodade:copilot", "copilot", true, false)
            | ("kodade:cursor", "cursor", true, false)
            | ("kodade:droid", "droid", true, false)
            | ("kodade:kimi", "kimi", true, false)
            | ("kodade:qwen", "qwen", true, false)
            | ("kodade:pi", "pi", true, false)
            | ("kodade:pi", "pi", false, true)
            | ("kodade:hermes", "hermes", true, false)
            | ("kodade:devin", "devin", true, false)
            | ("kodade:grok", "grok", true, false)
    )
}

/// Accept identity only from one of our fixed adapter/source pairs. Hook
/// `source` is user-supplied protocol text, so it must never become a pane
/// identity by itself.
fn trusted_hook_agent(source: &str, native: Option<&NativeSession>) -> Option<String> {
    let native = native?;
    let (agent, display) = match (source, native.source.as_str(), native.agent.as_str()) {
        ("kodade:claude-code", "kodade:claude-code", "claude") => ("claude", "Claude Code"),
        ("kodade:codex", "kodade:codex", "codex") => ("codex", "Codex"),
        ("kodade:gemini-cli", "kodade:gemini-cli", "gemini") => ("gemini", "Gemini CLI"),
        ("kodade:copilot", "kodade:copilot", "copilot") => ("copilot", "GitHub Copilot CLI"),
        ("kodade:cursor", "kodade:cursor", "cursor") => ("cursor", "Cursor CLI"),
        ("kodade:droid", "kodade:droid", "droid") => ("droid", "Droid"),
        ("kodade:kimi", "kodade:kimi", "kimi") => ("kimi", "Kimi CLI"),
        ("kodade:qwen", "kodade:qwen", "qwen") => ("qwen", "Qwen Code"),
        ("kodade:omp", "kodade:omp", "omp") => ("omp", "OMP"),
        ("kodade:kilo", "kodade:kilo", "kilo") => ("kilo", "Kilo Code"),
        ("kodade:hermes", "kodade:hermes", "hermes") => ("hermes", "Hermes"),
        ("kodade:antigravity", "kodade:antigravity", "antigravity") => {
            ("antigravity", "Antigravity")
        }
        ("kodade:devin", "kodade:devin", "devin") => ("devin", "Devin"),
        ("kodade:mastra", "kodade:mastra", "mastra") => ("mastra", "Mastra Code"),
        ("kodade:grok", "kodade:grok", "grok") => ("grok", "Grok Build"),
        ("kodade:opencode", "kodade:opencode", "opencode") => ("opencode", "OpenCode"),
        ("kodade:pi", "kodade:pi", "pi") => ("pi", "Pi"),
        _ => return None,
    };
    debug_assert!(!agent.is_empty());
    Some(display.into())
}

fn native_resume_argv(native: &NativeSession) -> Option<Vec<String>> {
    let id = native.id.as_ref();
    match (
        native.source.as_str(),
        native.agent.as_str(),
        id,
        native.path.as_ref(),
    ) {
        ("kodade:claude-code", "claude", Some(id), None) => {
            Some(vec!["claude".into(), "--resume".into(), id.clone()])
        }
        // `codex resume ID` launches the interactive TUI. `codex exec resume`
        // is a non-interactive command and must never be used for a pane.
        ("kodade:codex", "codex", Some(id), None) => {
            Some(vec!["codex".into(), "resume".into(), id.clone()])
        }
        ("kodade:gemini-cli", "gemini", Some(id), None) => {
            Some(vec!["gemini".into(), "--resume".into(), id.clone()])
        }
        ("kodade:opencode", "opencode", Some(id), None) => {
            Some(vec!["opencode".into(), "--session".into(), id.clone()])
        }
        ("kodade:omp", "omp", Some(id), None) => Some(vec!["omp".into(), format!("--resume={id}")]),
        ("kodade:omp", "omp", None, Some(path)) => Some(vec![
            "omp".into(),
            format!("--resume={}", path.to_string_lossy()),
        ]),
        ("kodade:copilot", "copilot", Some(id), None) => {
            Some(vec!["copilot".into(), format!("--resume={id}")])
        }
        ("kodade:cursor", "cursor", Some(id), None) => {
            Some(vec!["cursor-agent".into(), "--resume".into(), id.clone()])
        }
        ("kodade:droid", "droid", Some(id), None) => {
            Some(vec!["droid".into(), "--resume".into(), id.clone()])
        }
        ("kodade:kimi", "kimi", Some(id), None) => {
            Some(vec!["kimi".into(), "--session".into(), id.clone()])
        }
        ("kodade:qwen", "qwen", Some(id), None) => {
            Some(vec!["qwen".into(), "--resume".into(), id.clone()])
        }
        ("kodade:pi", "pi", Some(id), None) => {
            Some(vec!["pi".into(), "--session".into(), id.clone()])
        }
        ("kodade:hermes", "hermes", Some(id), None) => {
            Some(vec!["hermes".into(), "--resume".into(), id.clone()])
        }
        ("kodade:devin", "devin", Some(id), None) => {
            Some(vec!["devin".into(), "--resume".into(), id.clone()])
        }
        ("kodade:grok", "grok", Some(id), None) => {
            Some(vec!["grok".into(), "--resume".into(), id.clone()])
        }
        ("kodade:pi", "pi", None, Some(path)) => Some(vec![
            "pi".into(),
            "--session".into(),
            path.to_string_lossy().into_owned(),
        ]),
        _ => None,
    }
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

/// A `#rrggbb` color literal: a leading `#` and exactly six hex digits (#19).
fn is_hex_color(value: &str) -> bool {
    value
        .strip_prefix('#')
        .is_some_and(|rest| rest.len() == 6 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Poll a private duplicate of the PTY master. Polling at 50 ms bounds the
/// handoff pause acknowledgement without changing flags on the shared writer.
fn read_pty(
    mut reader: fs::File,
    parser: Arc<Mutex<PtyParser>>,
    last_output: Arc<Mutex<Instant>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    updates: broadcast::Sender<()>,
    control: Arc<ReaderControl>,
    output_generation: Arc<AtomicU64>,
) {
    tokio::task::spawn_blocking(move || {
        let mut bytes = [0_u8; 4096];
        loop {
            if !control.wait_if_paused() {
                break;
            }
            let mut ready = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let polled = unsafe { libc::poll(&mut ready, 1, 50) };
            if polled < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if polled == 0 {
                continue;
            }
            if ready.revents & libc::POLLIN == 0 {
                if ready.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    break;
                }
                continue;
            }
            // A pause may have arrived while poll was asleep; acknowledge it
            // before consuming the readable byte stream.
            if !control.wait_if_paused() {
                break;
            }
            let count = match reader.read(&mut bytes) {
                Ok(count) => count,
                Err(_) => break,
            };
            if count == 0 {
                break;
            }
            let mut replies = Vec::new();
            {
                let mut parser = parser.lock().expect("PTY parser lock poisoned");
                let tokens = parser
                    .callbacks_mut()
                    .graphics_decoder
                    .feed(&bytes[..count]);
                for token in tokens {
                    match token {
                        graphics::Token::Invalid => {
                            parser.callbacks_mut().graphics.cancel_transfer()
                        }
                        graphics::Token::Text(text) => graphics_text(&mut parser, &text),
                        graphics::Token::Graphics(frame) => {
                            let cursor = parser.screen().cursor_position();
                            let alternate = parser.screen().alternate_screen();
                            let result = parser
                                .callbacks_mut()
                                .graphics
                                .command(&frame, cursor, alternate);
                            replies.extend(result.reply);
                            if let Some((rows, cols)) = result.advance {
                                let (height, width) = parser.screen().size();
                                let rows = rows.min(height);
                                graphics_text(&mut parser, &vec![b'\n'; usize::from(rows)]);
                                parser.process(
                                    format!(
                                        "\x1b[{}G",
                                        cursor.1.saturating_add(cols).min(width.saturating_sub(1))
                                            + 1
                                    )
                                    .as_bytes(),
                                );
                            }
                        }
                    }
                }
            }
            if !replies.is_empty() {
                if let Ok(mut writer) = writer.lock() {
                    let _ = writer.write_all(&replies);
                }
            }
            *last_output.lock().expect("output lock poisoned") = Instant::now();
            output_generation.fetch_add(1, Ordering::Relaxed);
            let _ = updates.send(());
        }
    });
}
/// Track the cell movement that also moves graphics; terminal controls remain
/// interpreted by vt100, and image state follows clear/reset/alternate buffers.
fn graphics_text(parser: &mut PtyParser, text: &[u8]) {
    let mut tracker = std::mem::take(&mut parser.callbacks_mut().graphics_tracker);
    let mut store = std::mem::take(&mut parser.callbacks_mut().graphics);
    for &byte in text {
        let before_alt = parser.screen().alternate_screen();
        tracker.feed(byte, parser.screen(), &mut store);
        parser.process(&[byte]);
        let alternate = parser.screen().alternate_screen();
        if alternate && !before_alt {
            store.clear(true);
        }
    }
    parser.callbacks_mut().graphics_tracker = tracker;
    parser.callbacks_mut().graphics = store;
}

/// Inspect cloned grids so capturing an editor's alternate screen cannot
/// mutate its live parser or discard the shell beneath it.
fn terminal_handoff(parser: &PtyParser) -> Result<(Vec<u8>, String)> {
    terminal_replay::terminal_handoff(parser)
}

/// Collect the pane's full scrollback plus visible screen as plain-text lines,
/// oldest first. Walks the vt100 scrollback in screen-height windows from the
/// top down; the caller restores the live scroll offset afterward. Pure over the
/// parser so it can be unit-tested on a synthetic buffer.
fn read_history(parser: &mut PtyParser) -> Vec<String> {
    let (rows, cols) = parser.screen().size();
    let rows = rows as usize;
    parser.screen_mut().set_scrollback(usize::MAX);
    let max = parser.screen().scrollback();
    let total = max + rows;
    let mut lines: Vec<String> = Vec::with_capacity(total);
    while lines.len() < total {
        let filled = lines.len();
        // Offset so the window's top row is the next line we still need.
        parser
            .screen_mut()
            .set_scrollback(max.saturating_sub(filled));
        let offset = parser.screen().scrollback();
        let window_top = max - offset;
        let before = lines.len();
        for (r, line) in parser.screen().rows(0, cols).enumerate() {
            // Only append rows that continue the sequence (skip re-seen overlap).
            if window_top + r == lines.len() {
                lines.push(line);
            }
        }
        // Guard against a clamp that makes no forward progress.
        if lines.len() == before {
            break;
        }
    }
    lines
}

/// Read the live grid without disturbing a user's local-history viewport.
fn live_screen_contents(parser: &mut PtyParser) -> String {
    let offset = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(0);
    let contents = parser.screen().contents();
    parser.screen_mut().set_scrollback(offset);
    contents
}

/// Keep the newest complete UTF-8 suffix; terminal text never cuts a scalar in
/// half when it is written to the private history record.
fn utf8_tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
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
        graphics: parser
            .callbacks()
            .graphics
            .placements(screen.alternate_screen(), screen.scrollback()),
    }
}

/// Rebuild recent text and the saved visible grid before the PTY reader starts.
fn restore_screen(parser: &mut PtyParser, screen: &Screen, history: &str) {
    let (rows, cols) = parser.screen().size();
    if screen.rows.len() > usize::from(rows)
        || screen.rows.iter().any(|row| row.len() > usize::from(cols))
    {
        return;
    }
    parser.process(b"\x1b[2J\x1b[H");
    // Populate scrollback first, then redraw the formatted active frame.
    parser.process(history.replace('\n', "\r\n").as_bytes());
    parser.process(b"\x1b[H");
    for (row, runs) in screen.rows.iter().enumerate() {
        for run in runs {
            let mut sgr = String::from("\x1b[0");
            if run.attrs & ATTR_BOLD != 0 {
                sgr.push_str(";1");
            }
            if run.attrs & ATTR_DIM != 0 {
                sgr.push_str(";2");
            }
            if run.attrs & ATTR_ITALIC != 0 {
                sgr.push_str(";3");
            }
            if run.attrs & ATTR_UNDERLINE != 0 {
                sgr.push_str(";4");
            }
            if run.attrs & ATTR_INVERSE != 0 {
                sgr.push_str(";7");
            }
            append_sgr_color(&mut sgr, &run.fg, true);
            append_sgr_color(&mut sgr, &run.bg, false);
            sgr.push('m');
            parser.process(sgr.as_bytes());
            parser.process(run.text.as_bytes());
        }
        if row + 1 < screen.rows.len() {
            parser.process(b"\r\n");
        }
    }
    parser.process(b"\x1b[0m");
    let cursor_row = screen.cursor_row.min(rows.saturating_sub(1)) + 1;
    let cursor_col = screen.cursor_col.min(cols.saturating_sub(1)) + 1;
    parser.process(format!("\x1b[{cursor_row};{cursor_col}H").as_bytes());
    if !screen.cursor_visible {
        parser.process(b"\x1b[?25l");
    }
}

fn append_sgr_color(out: &mut String, color: &CellColor, foreground: bool) {
    match color {
        CellColor::Default => out.push_str(if foreground { ";39" } else { ";49" }),
        CellColor::Indexed(value) => {
            out.push_str(if foreground { ";38;5;" } else { ";48;5;" });
            out.push_str(&value.to_string());
        }
        CellColor::Rgb(red, green, blue) => {
            out.push_str(if foreground { ";38;2;" } else { ";48;2;" });
            out.push_str(&format!("{red};{green};{blue}"));
        }
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

/// Maximum newline-delimited client request. This permits the largest encoded
/// 8 MiB PNG plus protocol envelope without buffering an unbounded peer line.
const MAX_CLIENT_LINE: usize = 16 * 1024 * 1024;

async fn read_client_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(std::mem::take(line)))
            };
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if line.len().saturating_add(end) > MAX_CLIENT_LINE {
            bail!("client message exceeds 16 MiB");
        }
        line.extend_from_slice(&available[..end]);
        reader.consume(end);
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
            return Ok(Some(std::mem::take(line)));
        }
    }
}

async fn serve_client(stream: UnixStream, session: Arc<Session>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line_buffer = Vec::new();
    let mut updates = session.updates.subscribe();
    let mut events = session.events.subscribe();
    let mut shutdown = session.shutdown.subscribe();
    let mut upgrading = session.upgrading.subscribe();
    // Set by `Subscribe`: this connection then receives every session event.
    let mut subscribed = false;
    let mut subscription: Option<SubscriberGuard> = None;
    let mut process_timer = tokio::time::interval(Duration::from_secs(2));
    process_timer.tick().await;
    let mut last_snapshot = Instant::now() - Duration::from_millis(16);
    let mut initialized = false;
    let mut view = session.new_client_view()?;
    // A fresh client only hears about transitions raised after it attached, so
    // the spawn-time backlog never replays.
    let mut last_notify_seq = session.notify_high_water();
    loop {
        tokio::select! {
            line = read_client_line(&mut reader, &mut line_buffer) => {
                let Some(line) = line? else { return Ok(()); };
                let message = decode::<ClientMessage>(&line)?;
                // A client whose protocol version differs cannot be served; tell
                // it plainly and close so it fails fast instead of misbehaving.
                if let ClientMessage::Hello { version, .. } = message {
                    if version != PROTOCOL_VERSION {
                        write_server(&mut writer, &ServerMessage::Error {
                            message: format!(
                                "protocol version mismatch: client {version}, daemon {PROTOCOL_VERSION} — upgrade kodade-cli on both ends"
                            ),
                        }).await?;
                        return Ok(());
                    }
                }
                // Read-only messages answer directly; they never touch state.
                match message {
                    // Version probe: answer and keep the connection open so a
                    // caller can follow up on the same socket.
                    ClientMessage::Query(QueryKind::Version) => {
                        write_server(&mut writer, &ServerMessage::Version { version: PROTOCOL_VERSION }).await?;
                        continue;
                    }
                    ClientMessage::ReadPane { id, scrollback, lines } => {
                        match session.read_pane_text(id, scrollback, lines) {
                            Ok((text, scrollback_lines)) => {
                                write_server(&mut writer, &ServerMessage::PaneText { id, text, scrollback_lines }).await?;
                            }
                            Err(error) => {
                                write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    ClientMessage::PromptAgent { pane, expected_agent, expected_generation, bytes } => {
                        match session.prompt_agent(pane, &expected_agent, expected_generation, &bytes) {
                            Ok(snapshot) => write_server(&mut writer, &ServerMessage::Pane(snapshot)).await?,
                            Err(error) => {
                                write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    ClientMessage::Query(QueryKind::Image { pane, id, revision }) => {
                        let image = session.panes.lock().map_err(|_| anyhow!("pane lock poisoned"))?
                            .get(&pane).cloned().ok_or_else(|| anyhow!("pane not found"))
                            .and_then(|pane| pane.parser.lock().map_err(|_| anyhow!("parser lock poisoned"))?
                                .callbacks().graphics.image(id, revision));
                        let message = match image {
                            Ok(image) => ServerMessage::Image { pane, image },
                            Err(error) => ServerMessage::Error { message: error.to_string() },
                        };
                        write_server(&mut writer, &message).await?;
                        continue;
                    }
                    ClientMessage::PasteImage { pane, data } => {
                        let reply = match session.paste_image(pane, &data) {
                            Ok(path) => ServerMessage::ImagePasted { pane, path },
                            Err(error) => ServerMessage::Error { message: error.to_string() },
                        };
                        write_server(&mut writer, &reply).await?;
                        continue;
                    }
                    ClientMessage::Query(QueryKind::Pane(id)) => {
                        match session.pane_snapshot_stable(id) {
                            Ok(pane) => write_server(&mut writer, &ServerMessage::Pane(pane)).await?,
                            Err(error) => {
                                write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    ClientMessage::Query(QueryKind::Session) => {
                        write_server(&mut writer, &ServerMessage::Session(session.build_file_stable()?)).await?;
                        continue;
                    }
                    ClientMessage::Query(QueryKind::Schema) => {
                        write_server(&mut writer, &schema_message()).await?;
                        continue;
                    }
                    ClientMessage::Query(QueryKind::Manifests) => {
                        match session.manifest_info() {
                            Ok(manifests) => {
                                write_server(&mut writer, &ServerMessage::Manifests(manifests)).await?;
                            }
                            Err(error) => {
                                write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    ClientMessage::ReloadManifests => {
                        let result = {
                            let _dispatch = session.view_dispatch.lock().map_err(|_| anyhow!("view dispatch lock poisoned"))?;
                            session.ensure_mutation_open().and_then(|()| session.reload_manifests())
                        };
                        match result {
                            Ok(manifests) => {
                                write_server(&mut writer, &ServerMessage::Manifests(manifests)).await?;
                            }
                            Err(error) => {
                                write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                                continue;
                            }
                        }
                        continue;
                    }
                    ClientMessage::Upgrade { binary } => {
                        let session_for_upgrade = Arc::clone(&session);
                        let result = tokio::task::spawn_blocking(move || session_for_upgrade.upgrade(binary))
                            .await
                            .context("join live upgrade")?;
                        match result {
                            Ok(()) => {
                                session.upgraded.store(true, Ordering::Release);
                                let _ = session.upgrading.send(());
                                let shutdown = session.shutdown.clone();
                                // This must outlive the requesting socket: a client can vanish
                                // immediately after committing a valid replacement.
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                    let _ = shutdown.send(());
                                });
                                let _ = write_server(&mut writer, &ServerMessage::Upgrading).await;
                                return Ok(());
                            }
                            Err(error) => {
                                write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                                continue;
                            }
                        }
                    }
                    ClientMessage::Subscribe => {
                        // Counted once per connection; the guard decrements it
                        // however this loop exits.
                        if subscription.is_none() {
                            session.subscribers.fetch_add(1, Ordering::Relaxed);
                            subscription = Some(SubscriberGuard(Arc::clone(&session)));
                        }
                        subscribed = true;
                    }
                    _ => {}
                }
                let hello = matches!(message, ClientMessage::Hello { .. });
                let kill = matches!(message, ClientMessage::KillSession);
                let result = if initialized || hello {
                    session.handle_view(message, &mut view)
                } else {
                    session.handle_script(message)
                };
                match result {
                    Ok(()) if hello => {
                        initialized = true;
                        write_server(&mut writer, &ServerMessage::Welcome { session: session.session_name(), version: PROTOCOL_VERSION }).await?;
                        // The first client attach sees `restored: true`; clear it
                        // afterward so later snapshots (and `ls`) report normally.
                        write_server(&mut writer, &ServerMessage::Layout(session.snapshot_for_client(&view)?)).await?;
                        send_notifications(&mut writer, &session, &mut last_notify_seq, subscribed).await?;
                        session.clear_restored();
                    }
                    Ok(()) if kill => {
                        write_server(&mut writer, &ServerMessage::Shutdown).await?;
                        return Ok(());
                    }
                    Ok(()) => {
                        let layout = if initialized {
                            session.snapshot_for_client(&view)?
                        } else {
                            session.snapshot_stable()?
                        };
                        write_server(&mut writer, &ServerMessage::Layout(layout)).await?;
                        send_notifications(&mut writer, &session, &mut last_notify_seq, subscribed).await?;
                    }
                    Err(error) => {
                        write_server(&mut writer, &ServerMessage::Error { message: error.to_string() }).await?;
                        // A rejected interactive action (for example a dirty
                        // worktree removal) must not strand the attached TUI.
                        // One-shot callers still receive this Error as their
                        // reply and terminate on their own.
                        continue;
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
                    write_server(&mut writer, &ServerMessage::Layout(session.snapshot_for_client(&view)?)).await?;
                    send_notifications(&mut writer, &session, &mut last_notify_seq, subscribed).await?;
                    last_snapshot = Instant::now();
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = process_timer.tick() => {
                if initialized {
                    write_server(&mut writer, &ServerMessage::Layout(session.snapshot_for_client(&view)?)).await?;
                    send_notifications(&mut writer, &session, &mut last_notify_seq, subscribed).await?;
                    last_snapshot = Instant::now();
                }
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if subscribed {
                        write_server(&mut writer, &ServerMessage::Event(event)).await?;
                    }
                }
                // A slow subscriber just misses events; it never stalls the session.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = shutdown.recv() => {
                write_server(&mut writer, &ServerMessage::Shutdown).await?;
                return Ok(());
            },
            _ = upgrading.recv() => {
                write_server(&mut writer, &ServerMessage::Upgrading).await?;
                return Ok(());
            },
        }
    }
}
/// Keeps `Session::subscribers` accurate: dropped when the connection ends,
/// whichever way `serve_client` returns.
struct SubscriberGuard(Arc<Session>);

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        self.0.subscribers.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How often the daemon refreshes state while someone is subscribed.
const SUBSCRIBER_TICK: Duration = Duration::from_secs(2);

/// Refresh the session snapshot on a timer while at least one connection is
/// subscribed. Agent detection only runs inside `snapshot`, so without this a
/// `kodade-cli events` stream with no attached TUI would never see a state
/// change (#16).
async fn subscriber_tick(session: Arc<Session>) {
    let mut timer = tokio::time::interval(SUBSCRIBER_TICK);
    timer.tick().await; // The first tick completes immediately.
    loop {
        timer.tick().await;
        tick_subscribers(&session);
    }
}

/// One tick of [`subscriber_tick`], split out so tests can drive it directly.
fn tick_subscribers(session: &Session) {
    if session.subscribers.load(Ordering::Relaxed) > 0 {
        let _ = session.snapshot_stable();
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
    subscribed: bool,
) -> Result<()> {
    for notification in session.notifications_since(*last_seq) {
        *last_seq = (*last_seq).max(notification.seq);
        // Subscribers already receive these as `Event::Notification`.
        if !subscribed {
            write_server(writer, &ServerMessage::Notification(notification)).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn graphics_follow_bottom_row_autowrap_and_scroll_commands() {
        let mut parser = PtyParser::new_with_callbacks(4, 10, 100, PtyCallbacks::default());
        parser.callbacks_mut().graphics.command(
            b"a=T,f=24,s=1,v=1,i=7,c=1,r=1,C=1;AAAA",
            (2, 0),
            false,
        );
        graphics_text(&mut parser, b"\x1b[4;1H0123456789x");
        assert_eq!(parser.callbacks().graphics.placements(false, 0)[0].row, 1);
        graphics_text(&mut parser, b"\x1b[1S");
        assert_eq!(parser.callbacks().graphics.placements(false, 0)[0].row, 0);
        graphics_text(&mut parser, b"\x1b[1T");
        assert_eq!(parser.callbacks().graphics.placements(false, 0)[0].row, 1);
        graphics_text(&mut parser, b"\x1b[2J");
        assert!(parser.callbacks().graphics.placements(false, 0).is_empty());
    }
    use super::*;

    #[tokio::test]
    async fn pane_context_is_created_after_acceptance_and_removed_with_the_pane() {
        let session = Session::spawn(80, 24, "pane-context-lifetime".into()).expect("session");
        session
            .handle(ClientMessage::NewPane {
                workspace: None,
                tab: None,
                split: None,
                command: Some(vec!["sleep".into(), "60".into()]),
                name: Some("context".into()),
                context: Some(Box::new(InvocationContext {
                    selected_text: Some("selection is data".into()),
                    ..Default::default()
                })),
            })
            .expect("accept context pane");
        let pane = session
            .snapshot()
            .expect("snapshot")
            .panes
            .into_iter()
            .find(|pane| pane.focused)
            .expect("focused pane")
            .id;
        let path = session
            .panes
            .lock()
            .expect("panes")
            .get(&pane)
            .and_then(|pane| pane._context_file.as_ref())
            .map(|file| file.path.clone())
            .expect("daemon context file");
        assert!(path.exists());
        session
            .handle(ClientMessage::ClosePane)
            .expect("close pane");
        assert!(!path.exists(), "closing a pane releases its context file");
    }

    #[tokio::test]
    async fn handoff_keeps_native_environment_history_and_staged_context_ownership() {
        let mut source = Session::spawn(76, 21, "handoff-runtime".into()).unwrap();
        source.pane_history = true;
        source.state.lock().unwrap().workspaces[0]
            .env
            .insert("HANDOFF_VALUE".into(), "kept".into());
        source
            .handle(ClientMessage::NewPane {
                workspace: None,
                tab: None,
                split: None,
                command: Some(vec!["sleep".into(), "60".into()]),
                name: Some("context".into()),
                context: Some(Box::new(InvocationContext {
                    selected_text: Some("live context".into()),
                    ..Default::default()
                })),
            })
            .unwrap();
        let pane_id = source
            .snapshot()
            .unwrap()
            .panes
            .into_iter()
            .find(|pane| pane.focused)
            .unwrap()
            .id;
        let native = NativeSession {
            source: "kodade:codex".into(),
            agent: "codex".into(),
            id: Some("handoff-conversation".into()),
            path: None,
        };
        let path = {
            let panes = source.panes.lock().unwrap();
            let pane = &panes[&pane_id];
            *pane.native_session.lock().unwrap() = Some(native.clone());
            pane._context_file.as_ref().unwrap().path.clone()
        };
        source.notify_seq.store(99, Ordering::Relaxed);
        let captured = source.capture_handoff().unwrap();
        let fds = captured
            .fds
            .iter()
            .map(|fd| {
                let copy = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 0) };
                assert!(copy >= 0);
                copy
            })
            .collect();
        let imported = unsafe { Session::import_handoff(captured.manifest.clone(), fds) }.unwrap();
        assert_eq!(*imported.size.lock().unwrap(), (76, 21));
        assert!(imported.pane_history);
        assert_eq!(imported.notify_high_water(), 99);
        assert_eq!(
            imported.state.lock().unwrap().workspaces[0].env["HANDOFF_VALUE"],
            "kept"
        );
        assert_eq!(
            *imported.panes.lock().unwrap()[&pane_id]
                .native_session
                .lock()
                .unwrap(),
            Some(native)
        );
        drop(imported);
        assert!(
            path.exists(),
            "failed target must preserve the source action context"
        );
        drop(captured);
        source.handle(ClientMessage::ClosePane).unwrap();
        assert!(
            !path.exists(),
            "source still owns context cleanup after rollback"
        );
    }

    #[tokio::test]
    async fn rejected_pane_context_never_spawns_or_persists_a_file() {
        let session = Session::spawn(80, 24, "reject-pane-context".into()).expect("session");
        let before = session.panes.lock().expect("panes").len();
        let result = session.handle(ClientMessage::NewPane {
            workspace: None,
            tab: None,
            split: None,
            command: Some(vec!["sleep".into(), "60".into()]),
            name: Some("too-large".into()),
            context: Some(Box::new(InvocationContext {
                selected_text: Some("x".repeat(MAX_CONTEXT_BYTES)),
                ..Default::default()
            })),
        });
        assert!(result.is_err());
        assert_eq!(session.panes.lock().expect("panes").len(), before);
    }

    #[tokio::test]
    async fn client_line_reader_accepts_empty_and_rejects_oversized_requests() {
        let (mut write, read) = tokio::io::duplex(MAX_CLIENT_LINE + 2);
        write.write_all(b"\n").await.unwrap();
        drop(write);
        assert_eq!(
            read_client_line(&mut BufReader::new(read), &mut Vec::new())
                .await
                .unwrap(),
            Some(Vec::new())
        );

        let (mut write, read) = tokio::io::duplex(MAX_CLIENT_LINE + 2);
        let payload = vec![b'x'; MAX_CLIENT_LINE + 1];
        tokio::spawn(async move {
            write.write_all(&payload).await.unwrap();
        });
        assert!(read_client_line(&mut BufReader::new(read), &mut Vec::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn client_line_reader_keeps_partial_upload_when_a_snapshot_interrupts() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let mut reader = BufReader::new(reader);
        let mut pending = Vec::new();
        writer.write_all(b"{\"Input\":").await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            read_client_line(&mut reader, &mut pending)
        )
        .await
        .is_err());
        writer.write_all(b"{\"bytes\":[]}}\n").await.unwrap();
        assert_eq!(
            read_client_line(&mut reader, &mut pending)
                .await
                .unwrap()
                .unwrap(),
            b"{\"Input\":{\"bytes\":[]}}"
        );
    }
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
    fn session_names_are_safe_single_path_components() {
        for invalid in ["", ".", "..", "a/b", "a\\b", "line\nbreak", &"x".repeat(65)] {
            assert!(
                validate_session_name(invalid).is_err(),
                "{invalid:?} should fail"
            );
        }
        assert!(validate_session_name("agent-session_01").is_ok());
    }
    #[test]
    fn vt100_snapshot_retains_terminal_contents() {
        let mut parser = PtyParser::new_with_callbacks(3, 10, 100, PtyCallbacks::default());
        parser.process(b"hello\r\nworld");
        assert!(snapshot(&parser).contents.contains("hello"));
    }
    #[test]
    fn read_history_recovers_full_scrollback() {
        // A 24-row screen with plenty of scrollback fed 3000 numbered lines.
        let mut parser = PtyParser::new_with_callbacks(24, 20, 10_000, PtyCallbacks::default());
        for n in 1..=3000 {
            parser.process(format!("{n}\r\n").as_bytes());
        }
        let lines = read_history(&mut parser);
        let numbers: Vec<&str> = lines
            .iter()
            .filter(|line| !line.is_empty())
            .map(String::as_str)
            .collect();
        // Every line survives in order, oldest first, none dropped.
        assert_eq!(numbers.len(), 3000);
        assert_eq!(numbers.first(), Some(&"1"));
        assert_eq!(numbers.last(), Some(&"3000"));
        // A mid-history line is present at its expected position.
        assert_eq!(numbers[1499], "1500");
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
    fn agent_state_transition_reports_a_change_once() {
        let start = Instant::now();
        let mut state = PaneState {
            last: Some(AgentStateKind::Working),
            since: start,
        };

        let first = state.transition(AgentStateKind::Blocked, start + Duration::from_secs(1));
        let second = state.transition(AgentStateKind::Blocked, start + Duration::from_secs(2));

        assert_eq!(first.0, Some(AgentStateKind::Working));
        assert_eq!(second.0, Some(AgentStateKind::Blocked));
        assert_eq!(second.1, 1);
    }

    #[tokio::test]
    async fn concurrent_snapshots_publish_one_agent_transition() {
        let session = Arc::new(Session::spawn(80, 24, "transition-race".into()).expect("session"));
        let pane = session.snapshot().expect("initial snapshot").panes[0].id;
        session
            .handle(ClientMessage::AgentState {
                pane,
                state: AgentStateKind::Working,
                source: "test".into(),
                native_session: None,
            })
            .expect("report working");
        session.snapshot().expect("settle working state");
        let mut events = session.events.subscribe();
        session
            .handle(ClientMessage::AgentState {
                pane,
                state: AgentStateKind::Blocked,
                source: "test".into(),
                native_session: None,
            })
            .expect("report blocked");

        let first = Arc::clone(&session);
        let second = Arc::clone(&session);
        let first = std::thread::spawn(move || first.snapshot().expect("first snapshot"));
        let second = std::thread::spawn(move || second.snapshot().expect("second snapshot"));
        first.join().expect("first thread");
        second.join().expect("second thread");

        let transitions = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| {
                matches!(
                    event,
                    Event::AgentStateChanged {
                        pane: changed,
                        from: AgentStateKind::Working,
                        to: AgentStateKind::Blocked,
                    } if *changed == pane
                )
            })
            .count();
        assert_eq!(transitions, 1);
    }

    #[test]
    fn scroll_offset_clamps_to_available_history() {
        assert_eq!(scroll_offset_after_delta(1, 99, 2), 2);
        assert_eq!(scroll_offset_after_delta(1, -99, 2), 0);
    }

    #[test]
    fn cold_history_replay_restores_formatted_active_screen() {
        let mut source = PtyParser::new_with_callbacks(3, 20, 100, PtyCallbacks::default());
        source.process(b"\x1b[31mred ready\x1b[0m");
        let saved = snapshot(&source);
        let mut restored = PtyParser::new_with_callbacks(3, 20, 100, PtyCallbacks::default());
        restore_screen(
            &mut restored,
            &saved,
            "earlier one\nearlier two\nearlier three\nearlier four\n",
        );
        assert!(restored.screen().contents().contains("red ready"));
    }

    #[tokio::test]
    async fn pane_snapshot_keeps_history_anchored_while_output_arrives() {
        let (updates, _) = broadcast::channel(1);
        // A quiet command avoids the user's interactive login-shell prompt
        // racing the synthetic terminal output below.
        let mut pane = Pane::spawn(
            PaneId(1),
            "test",
            20,
            2,
            "scroll-test".into(),
            hook_socket_path(),
            updates,
            Arc::new(AtomicU64::new(0)),
            None,
            Some(vec!["sleep".into(), "60".into()]),
            None,
            None,
            HashMap::new(),
        )
        .expect("test pane");
        // The reader keeps the original parser, so shell profile output cannot
        // race the synthetic parser this regression test controls.
        pane.parser = Arc::new(Mutex::new(PtyParser::new_with_callbacks(
            2,
            20,
            100,
            PtyCallbacks::default(),
        )));
        {
            let mut parser = pane.parser.lock().expect("parser lock");
            parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        }
        pane.scroll(2);
        let before = pane.snapshot().0.contents;
        pane.parser
            .lock()
            .expect("parser lock")
            .process(b"five\r\n");
        let (after, offset) = pane.snapshot();
        assert_eq!(offset, 3);
        assert_eq!(after.contents, before);
        pane.scroll(i16::MAX);
        let (_, oldest) = pane.snapshot();
        assert!(oldest >= 3);
        pane.scroll(i16::MAX);
        assert_eq!(pane.snapshot().1, oldest);
        pane.scroll(i16::MIN);
        assert_eq!(pane.snapshot().1, 0);
    }

    #[tokio::test]
    async fn paused_pty_reader_stops_parser_until_resumed() {
        let (updates, _) = broadcast::channel(8);
        let pane = Pane::spawn(
            PaneId(1), "ticker", 40, 4, "pause-test".into(), hook_socket_path(), updates,
            Arc::new(AtomicU64::new(0)), None,
            Some(vec!["sh".into(), "-c".into(), "i=0; while [ $i -lt 40 ]; do printf 'tick-%s\\n' \"$i\"; i=$((i+1)); sleep 0.03; done".into()]),
            None, None, HashMap::new(),
        ).expect("spawn ticker");
        tokio::time::sleep(Duration::from_millis(120)).await;
        pane.reader
            .pause(Duration::from_secs(1))
            .expect("reader acknowledges pause");
        let before = pane.snapshot().0.contents;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            pane.snapshot().0.contents,
            before,
            "reader consumed output after acknowledging pause"
        );
        pane.reader.resume();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_ne!(
            pane.snapshot().0.contents,
            before,
            "reader did not catch buffered output after resume"
        );
    }

    #[test]
    fn paused_reader_shutdown_wakes_without_hanging() {
        let control = Arc::new(ReaderControl::new());
        let reader = Arc::clone(&control);
        let worker = std::thread::spawn(move || reader.wait_if_paused());
        // The first worker call can return before pause is requested. Exercise
        // the blocked state deterministically with a second waiting thread.
        let _ = worker.join();
        let reader = Arc::clone(&control);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            reader.wait_if_paused()
        });
        control
            .pause(Duration::from_secs(1))
            .expect("reader paused");
        control.shutdown();
        assert!(!worker.join().expect("reader thread joins"));
    }

    #[tokio::test]
    async fn direct_mutators_reject_a_handoff_before_writing_or_creating_attachments() {
        let session = Session::spawn(40, 8, "handoff-guard".into()).unwrap();
        let pane = session.snapshot().unwrap().panes[0].id;
        session.handoff_active.store(true, Ordering::Release);
        assert!(session
            .prompt_agent(pane, "codex", 0, b"must not execute\n")
            .unwrap_err()
            .to_string()
            .contains("upgrade in progress"));
        assert!(session
            .paste_image(pane, "invalid PNG")
            .unwrap_err()
            .to_string()
            .contains("upgrade in progress"));
        assert!(session.images.snapshot().is_none());
        assert!(session.handoff_active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn imported_master_replays_screen_and_reads_only_after_ownership_stage() {
        let (updates, _) = broadcast::channel(8);
        let source = Pane::spawn(
            PaneId(1),
            "cat",
            40,
            4,
            "import-test".into(),
            hook_socket_path(),
            updates.clone(),
            Arc::new(AtomicU64::new(0)),
            None,
            Some(vec!["cat".into()]),
            None,
            None,
            HashMap::new(),
        )
        .expect("spawn cat");
        source.write(b"before-handoff\n").expect("write source");
        tokio::time::sleep(Duration::from_millis(100)).await;
        source.agent_generation.store(9, Ordering::Relaxed);
        source.activity_revision.store(4, Ordering::Relaxed);
        source.parser.lock().unwrap().callbacks_mut().title = "editor status".into();
        let (runtime, fd) = source.capture_handoff().expect("pause and capture source");
        let (sender, receiver) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let transfer =
            std::thread::spawn(move || handoff::send_fds(&sender, &[fd]).expect("send master"));
        let received = handoff::recv_fds(&receiver, 1).expect("receive master");
        transfer.join().expect("transfer joins");
        let imported = unsafe {
            Pane::import_handoff(runtime, received[0], updates, Arc::new(AtomicU64::new(0)))
        }
        .expect("import master");
        assert!(imported.snapshot().0.contents.contains("before-handoff"));
        assert_eq!(imported.agent_generation.load(Ordering::Relaxed), 9);
        assert_eq!(imported.activity_revision.load(Ordering::Relaxed), 4);
        assert_eq!(
            imported.parser.lock().unwrap().callbacks().title,
            "editor status"
        );
        let frozen = imported.snapshot().0.contents;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            imported.snapshot().0.contents,
            frozen,
            "target reader started before ownership stage"
        );
        imported.reader.resume();
        imported.write(b"after-handoff\n").expect("write imported");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(imported.snapshot().0.contents.contains("after-handoff"));
        source.reader.resume();
    }

    #[test]
    fn handoff_restores_both_terminal_buffers_and_saved_cursor() {
        let mut source = PtyParser::new_with_callbacks(6, 30, 100, PtyCallbacks::default());
        source.process(b"shell line\r\nshell prompt> ");
        let primary = source.screen().contents_formatted();
        source.process(b"\x1b[?1049h\x1b[31meditor contents\x1b[0m\x1b[3;5H\x1b7\x1b[5;9H");
        let alternate = source.screen().contents_formatted();
        let (screen, history) = terminal_handoff(&source).unwrap();
        let mut target = PtyParser::new_with_callbacks(6, 30, 100, PtyCallbacks::default());
        target.process(history.as_bytes());
        target.process(&screen);
        assert!(target.screen().alternate_screen());
        assert_eq!(target.screen().contents_formatted(), alternate);
        target.process(b"\x1b8");
        assert_eq!(target.screen().cursor_position(), (2, 4));
        target.process(b"\x1b[?1049l");
        assert!(!target.screen().alternate_screen());
        assert_eq!(target.screen().contents_formatted(), primary);
    }

    #[test]
    fn alternate_screen_has_no_local_history_and_restores_normal_screen() {
        let mut parser = PtyParser::new_with_callbacks(2, 20, 100, PtyCallbacks::default());
        parser.process(b"one\r\ntwo\r\nthree\r\n");
        parser.process(b"\x1b[?1049halt\r\n");
        assert!(parser.screen().alternate_screen());
        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(parser.screen().scrollback(), 0);
        parser.process(b"\x1b[?1049l");
        assert!(!parser.screen().alternate_screen());
        assert!(parser.screen().contents().contains("three"));
    }

    #[test]
    fn live_detection_screen_ignores_history_viewport() {
        let mut parser = PtyParser::new_with_callbacks(2, 20, 100, PtyCallbacks::default());
        parser.process(b"old\r\nolder\r\nlive\r\nnow\r\n");
        parser.screen_mut().set_scrollback(2);
        let offset = parser.screen().scrollback();
        assert_eq!(offset, 2);
        let history = parser.screen().contents();
        let live = live_screen_contents(&mut parser);
        assert_ne!(live, history);
        assert_eq!(parser.screen().scrollback(), offset);
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
                    color: None,
                    env: HashMap::new(),
                    branch: None,
                    main_worktree_root: None,
                    parent: None,
                    metadata_checked_at: None,
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
                    color: None,
                    env: HashMap::new(),
                    branch: None,
                    main_worktree_root: None,
                    parent: None,
                    metadata_checked_at: None,
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
                color: None,
                env: HashMap::new(),
                branch: None,
                main_worktree_root: None,
                parent: None,
                metadata_checked_at: None,
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
        assert!(break_pane(&mut state).is_some());
        let workspace = &state.workspaces[0];
        assert_eq!(workspace.tabs.len(), 2);
        // Source tab keeps the remaining pane and focus lands on it.
        assert_eq!(workspace.tabs[0].tree, LayoutTree::Leaf { pane: PaneId(3) });
        assert_eq!(workspace.tabs[0].focused, PaneId(3));
        // New tab holds the broken-out pane and becomes active.
        assert_eq!(workspace.tabs[1].tree, LayoutTree::Leaf { pane: PaneId(4) });
        assert_eq!(workspace.active_tab, workspace.tabs[1].id);
        // A single-pane tab has nothing to break out.
        assert!(break_pane(&mut state).is_none());
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
                    color: Some("#e7a33b".into()),
                    env: HashMap::new(),
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
                                    native_session: None,
                                },
                                persist::PaneFile {
                                    id: 31,
                                    title: "root".into(),
                                    cwd: Some(PathBuf::from("/")),
                                    command: None,
                                    native_session: None,
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
                                native_session: None,
                            }],
                        },
                    ],
                },
                persist::WorkspaceFile {
                    id: 11,
                    name: "two".into(),
                    root: None,
                    color: None,
                    env: HashMap::new(),
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
                            native_session: None,
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
    fn resume_command_uses_unique_explicit_native_sessions() {
        let native = |id: &str| NativeSession {
            source: "kodade:codex".into(),
            agent: "codex".into(),
            id: Some(id.into()),
            path: None,
        };
        let agent = |id: &str| persist::PaneFile {
            id: 1,
            title: "codex".into(),
            cwd: None,
            command: Some(vec!["codex".into()]),
            native_session: Some(native(id)),
        };
        let mut resumed = HashSet::new();
        assert_eq!(resume_command(&agent("one"), false, &mut resumed), None);
        assert_eq!(
            resume_command(&agent("one"), true, &mut resumed),
            Some(vec!["codex".into(), "resume".into(), "one".into()])
        );
        assert_eq!(
            resume_command(&agent("two"), true, &mut resumed),
            Some(vec!["codex".into(), "resume".into(), "two".into()])
        );
        // A duplicate or foreign native reference deliberately becomes a shell,
        // never the ambiguous old manifest `resume --last` command.
        assert_eq!(resume_command(&agent("one"), true, &mut resumed), None);
        let invalid = persist::PaneFile {
            id: 4,
            title: "codex".into(),
            cwd: None,
            command: Some(vec!["codex".into()]),
            native_session: Some(NativeSession {
                source: "untrusted".into(),
                agent: "codex".into(),
                id: Some("one".into()),
                path: None,
            }),
        };
        assert_eq!(resume_command(&invalid, true, &mut resumed), None);
    }

    #[test]
    fn verified_native_resume_contracts_use_reported_ids() {
        let devin = NativeSession {
            source: "kodade:devin".into(),
            agent: "devin".into(),
            id: Some("devin-session".into()),
            path: None,
        };
        assert!(valid_native_session(&devin));
        assert_eq!(
            native_resume_argv(&devin),
            Some(vec![
                "devin".into(),
                "--resume".into(),
                "devin-session".into()
            ])
        );

        let grok = NativeSession {
            source: "kodade:grok".into(),
            agent: "grok".into(),
            id: Some("grok-session".into()),
            path: None,
        };
        assert!(valid_native_session(&grok));
        assert_eq!(
            native_resume_argv(&grok),
            Some(vec![
                "grok".into(),
                "--resume".into(),
                "grok-session".into()
            ])
        );
    }

    #[tokio::test]
    async fn hook_report_persists_native_identity_for_interactive_agents() {
        let session = Session::spawn(80, 24, "native-report".into()).expect("spawn session");
        let pane = session.snapshot().expect("snapshot").panes[0].id;
        *session.panes.lock().expect("panes")[&pane]
            .agent_identity
            .lock()
            .expect("identity") = Some("previous-agent-process".into());
        session
            .handle(ClientMessage::AgentState {
                pane,
                state: AgentStateKind::Working,
                source: "kodade:codex".into(),
                native_session: Some(NativeSession {
                    source: "kodade:codex".into(),
                    agent: "codex".into(),
                    id: Some("thread-123".into()),
                    path: None,
                }),
            })
            .expect("report native session");
        session
            .snapshot()
            .expect("next detection preserves new report");
        let file = session.build_file();
        assert_eq!(
            file.workspaces[0].tabs[0].panes[0]
                .native_session
                .as_ref()
                .and_then(|native| native.id.as_deref()),
            Some("thread-123")
        );
    }

    #[tokio::test]
    async fn restore_uses_each_explicit_native_session_in_the_same_cwd() {
        let mut file = restore_fixture();
        let panes = &mut file.workspaces[0].tabs[0].panes;
        for (pane, id) in panes.iter_mut().take(2).zip(["first", "second"]) {
            pane.native_session = Some(NativeSession {
                source: "kodade:codex".into(),
                agent: "codex".into(),
                id: Some(id.into()),
                path: None,
            });
            pane.cwd = Some(PathBuf::from("/tmp"));
        }
        let session = Session::restore(file, true).expect("restore");
        let commands: Vec<_> = session
            .panes
            .lock()
            .expect("panes")
            .values()
            .filter_map(|pane| pane.spawn_command.clone())
            .collect();
        assert!(commands.contains(&vec!["codex".into(), "resume".into(), "first".into()]));
        assert!(commands.contains(&vec!["codex".into(), "resume".into(), "second".into()]));
    }

    #[tokio::test]
    async fn retiring_hidden_native_conversations_marks_persistence_dirty() {
        let session = Session::spawn(80, 24, "native-retirement".into()).unwrap();
        let id = session.snapshot().unwrap().panes[0].id;
        let pane = Arc::clone(&session.panes.lock().unwrap()[&id]);
        session.handle(ClientMessage::NewTab).unwrap();
        *pane.agent_identity.lock().unwrap() = Some("previous-agent".into());
        *pane.native_session.lock().unwrap() = Some(NativeSession {
            source: "kodade:codex".into(),
            agent: "codex".into(),
            id: Some("old-conversation".into()),
            path: None,
        });
        let before = session.layout_generation.load(Ordering::Relaxed);
        session.snapshot().unwrap();
        assert!(pane.native_session.lock().unwrap().is_none());
        assert!(session.layout_generation.load(Ordering::Relaxed) > before);
    }

    #[tokio::test]
    async fn restore_without_resume_agents_starts_plain_shells() {
        let mut file = restore_fixture();
        file.workspaces[0].tabs[0].panes[0].native_session = Some(NativeSession {
            source: "kodade:codex".into(),
            agent: "codex".into(),
            id: Some("previous-conversation".into()),
            path: None,
        });
        let session = Session::restore(file, false).expect("restore");
        let panes = session.panes.lock().expect("pane lock");
        assert!(panes.values().all(|pane| pane.spawn_command.is_none()));
        assert!(panes.values().all(|pane| pane
            .native_session
            .lock()
            .expect("native session")
            .is_none()));
    }

    #[tokio::test]
    async fn duplicate_restore_does_not_leave_a_native_session_on_the_shell() {
        let mut file = restore_fixture();
        for pane in &mut file.workspaces[0].tabs[0].panes {
            pane.native_session = Some(NativeSession {
                source: "kodade:codex".into(),
                agent: "codex".into(),
                id: Some("same-conversation".into()),
                path: None,
            });
        }
        let session = Session::restore(file, true).expect("restore");
        let panes = session.panes.lock().expect("panes");
        assert_eq!(
            panes
                .values()
                .filter(|pane| pane.spawn_command.is_some())
                .count(),
            1
        );
        for pane in panes.values().filter(|pane| pane.spawn_command.is_none()) {
            assert!(pane
                .native_session
                .lock()
                .expect("native session")
                .is_none());
        }
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

    #[tokio::test]
    async fn stable_readers_wait_out_a_staged_client_view() {
        use std::sync::mpsc;

        let session = Arc::new(Session::spawn(80, 24, "stable-view".into()).expect("spawn"));
        let first = session.snapshot().expect("first snapshot");
        let workspace = first.active_workspace;
        let first_tab = first.active_tab;
        session.handle(ClientMessage::NewTab).expect("new tab");
        let second_tab = session.snapshot().expect("second snapshot").active_tab;
        {
            let mut state = session.state.lock().expect("state");
            state.workspaces[0].active_tab = first_tab;
        }
        let mut view = session.new_client_view().expect("client view");
        view.workspace = workspace;
        view.tabs.insert(workspace, second_tab);
        let (staged_tx, staged_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let staging_session = Arc::clone(&session);
        let staging = std::thread::spawn(move || {
            let _dispatch = staging_session.view_dispatch.lock().expect("dispatch");
            let saved = {
                let mut state = staging_session.state.lock().expect("state");
                let saved = Session::save_selection(&state);
                Session::install_view(&mut state, &view);
                saved
            };
            staged_tx.send(()).expect("staged");
            release_rx.recv().expect("release staging");
            let mut state = staging_session.state.lock().expect("state");
            Session::restore_selection(&mut state, saved);
            ready_tx.send(()).expect("restored");
        });
        staged_rx.recv().expect("view installed");
        let reader_session = Arc::clone(&session);
        let reader = std::thread::spawn(move || {
            let file = reader_session.build_file_stable().expect("stable export");
            let view = reader_session
                .new_client_view()
                .expect("stable client view");
            (file, view)
        });
        // The reader cannot complete while the alternate selection is staged.
        std::thread::sleep(Duration::from_millis(20));
        assert!(!reader.is_finished());
        release_tx.send(()).expect("release");
        ready_rx.recv().expect("restored");
        staging.join().expect("staging thread");
        let (file, new_view) = reader.join().expect("reader thread");
        assert_eq!(file.active_workspace, workspace.0);
        assert_eq!(file.workspaces[0].active_tab, first_tab.0);
        assert_eq!(new_view.workspace, workspace);
        assert_eq!(new_view.tabs[&workspace], first_tab);
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

    async fn next_layout_matching(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        predicate: impl Fn(&LayoutSnapshot) -> bool,
    ) -> LayoutSnapshot {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let ServerMessage::Layout(layout) = next_server_message(lines).await {
                    if predicate(&layout) {
                        break layout;
                    }
                }
            }
        })
        .await
        .expect("matching layout")
    }

    async fn next_error(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    ) -> String {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let ServerMessage::Error { message } = next_server_message(lines).await {
                    break message;
                }
            }
        })
        .await
        .expect("error reply")
    }

    #[tokio::test]
    async fn compact_focus_and_input_restore_the_interacting_clients_pty_size() {
        let session = Session::spawn(120, 30, "compact-size".into()).unwrap();
        session.handle(ClientMessage::SplitRight).unwrap();
        let mut narrow = session.new_client_view().unwrap();
        narrow.cols = 40;
        narrow.rows = 20;
        session
            .handle_view(ClientMessage::SetCompactView { enabled: true }, &mut narrow)
            .unwrap();
        session
            .handle_view(ClientMessage::FocusPaneCycle { forward: true }, &mut narrow)
            .unwrap();
        let focused = narrow.focused[&narrow.tabs[&narrow.workspace]];
        let pane = session.panes.lock().unwrap()[&focused].clone();
        assert_eq!(pane.master.lock().unwrap().get_size().unwrap().cols, 38);
        let mut wide = session.new_client_view().unwrap();
        wide.cols = 120;
        session.resize_for_view(&wide).unwrap();
        session
            .handle_view(ClientMessage::Input { bytes: Vec::new() }, &mut narrow)
            .unwrap();
        assert_eq!(pane.master.lock().unwrap().get_size().unwrap().cols, 38);
    }

    #[tokio::test]
    async fn compact_view_projects_focused_pane_without_mutating_tab_layout() {
        let session = Session::spawn(100, 30, "compact-view".into()).expect("spawn session");
        session.handle(ClientMessage::SplitRight).expect("split");
        let full = session.snapshot().expect("full snapshot");
        assert!(matches!(full.tree, LayoutTree::Split { .. }));
        let mut view = session.new_client_view().expect("view");
        session
            .handle_view(ClientMessage::SetCompactView { enabled: true }, &mut view)
            .expect("enable compact");
        let compact = session
            .snapshot_for_client(&view)
            .expect("compact snapshot");
        assert!(matches!(compact.tree, LayoutTree::Leaf { .. }));
        assert_eq!(
            compact.panes.len(),
            2,
            "switcher retains hidden pane identities"
        );
        assert!(!compact.zoomed);
        let state = session.state.lock().expect("state");
        assert!(!state.workspaces[0].tabs[0].zoomed);
        assert!(matches!(
            state.workspaces[0].tabs[0].tree,
            LayoutTree::Split { .. }
        ));
        drop(state);
        session
            .handle_view(ClientMessage::SetCompactView { enabled: false }, &mut view)
            .expect("disable compact");
        assert!(matches!(
            session
                .snapshot_for_client(&view)
                .expect("restored snapshot")
                .tree,
            LayoutTree::Split { .. }
        ));
    }

    #[tokio::test]
    async fn two_socket_clients_keep_selection_scroll_and_size_independent() {
        let session = Arc::new(Session::spawn(80, 24, "views".into()).expect("spawn session"));
        let (a_server, a_client) = UnixStream::pair().expect("first socket pair");
        let (b_server, b_client) = UnixStream::pair().expect("second socket pair");
        let a_task = tokio::spawn(serve_client(a_server, Arc::clone(&session)));
        let b_task = tokio::spawn(serve_client(b_server, Arc::clone(&session)));
        let (a_reader, mut a_writer) = a_client.into_split();
        let (b_reader, mut b_writer) = b_client.into_split();
        let mut a = BufReader::new(a_reader).lines();
        let mut b = BufReader::new(b_reader).lines();
        for (writer, cols, rows) in [(&mut a_writer, 100, 30), (&mut b_writer, 60, 20)] {
            writer
                .write_all(
                    &encode(&ClientMessage::Hello {
                        cols,
                        rows,
                        version: PROTOCOL_VERSION,
                    })
                    .unwrap(),
                )
                .await
                .expect("hello");
        }
        assert!(matches!(
            next_server_message(&mut a).await,
            ServerMessage::Welcome { .. }
        ));
        let first = match next_server_message(&mut a).await {
            ServerMessage::Layout(layout) => layout,
            other => panic!("expected layout, got {other:?}"),
        };
        assert!(matches!(
            next_server_message(&mut b).await,
            ServerMessage::Welcome { .. }
        ));
        let b_first = match next_server_message(&mut b).await {
            ServerMessage::Layout(layout) => layout,
            other => panic!("expected layout, got {other:?}"),
        };
        let first_tab = first.active_tab;
        let first_pane = first.panes[0].id;
        assert_eq!(b_first.active_tab, first_tab);

        // Client A creates and selects a tab. Client B keeps the original tab.
        a_writer
            .write_all(&encode(&ClientMessage::NewTab).unwrap())
            .await
            .expect("new tab");
        let a_second = next_layout_matching(&mut a, |layout| layout.tabs.len() == 2).await;
        let second_tab = a_second.active_tab;
        assert_ne!(second_tab, first_tab);
        b_writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Layout)).unwrap())
            .await
            .expect("query b");
        let b_still_first =
            next_layout_matching(&mut b, |layout| layout.active_tab == first_tab).await;
        assert_eq!(b_still_first.active_tab, first_tab);

        // `Input` follows each connection's focused pane, rather than the
        // session's persisted/script focus. Use the live shells so this is a
        // real PTY routing assertion instead of inspecting implementation state.
        let second_pane = a_second.panes[0].id;
        a_writer
            .write_all(
                &encode(&ClientMessage::Input {
                    bytes: b"printf VIEW_A\\n\r".to_vec(),
                })
                .unwrap(),
            )
            .await
            .expect("input a");
        b_writer
            .write_all(
                &encode(&ClientMessage::Input {
                    bytes: b"printf VIEW_B\\n\r".to_vec(),
                })
                .unwrap(),
            )
            .await
            .expect("input b");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (first_text, _) = session
                    .read_pane_text(first_pane, true, None)
                    .expect("read first");
                let (second_text, _) = session
                    .read_pane_text(second_pane, true, None)
                    .expect("read second");
                if first_text.contains("VIEW_B") && second_text.contains("VIEW_A") {
                    assert!(!first_text.contains("VIEW_A"));
                    assert!(!second_text.contains("VIEW_B"));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("inputs reach their own panes");

        // Scrollback belongs to B's view. Seed real terminal history directly,
        // then verify B sees it while A remains on its own tab.
        let pane = session
            .panes
            .lock()
            .expect("panes")
            .get(&first_pane)
            .cloned()
            .expect("first pane");
        let history = (0..40)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        pane.parser
            .lock()
            .expect("parser")
            .process(history.as_bytes());
        b_writer
            .write_all(
                &encode(&ClientMessage::ScrollPane {
                    id: first_pane,
                    delta: 3,
                })
                .unwrap(),
            )
            .await
            .expect("scroll b");
        let b_scrolled = next_layout_matching(&mut b, |layout| {
            layout
                .panes
                .iter()
                .any(|pane| pane.id == first_pane && pane.scroll_offset > 0)
        })
        .await;
        assert!(b_scrolled.panes[0].scroll_offset > 0);
        a_writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Layout)).unwrap())
            .await
            .expect("query a");
        let a_still_second =
            next_layout_matching(&mut a, |layout| layout.active_tab == second_tab).await;
        assert_eq!(a_still_second.active_tab, second_tab);

        // A's selection mutation takes physical size ownership; B's read-only
        // query did not resize it back to 60x20.
        a_writer
            .write_all(
                &encode(&ClientMessage::FocusPaneId {
                    id: a_second.panes[0].id,
                })
                .unwrap(),
            )
            .await
            .expect("focus a");
        let _ = next_layout_matching(&mut a, |layout| layout.active_tab == second_tab).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if *session.size.lock().expect("size") == (100, 30) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("A's dispatched focus takes geometry ownership");
        drop(a_writer);
        // Disconnecting A leaves B and the panes alive.
        b_writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Layout)).unwrap())
            .await
            .expect("query after detach");
        let alive = next_layout_matching(&mut b, |layout| layout.active_tab == first_tab).await;
        assert_eq!(alive.panes[0].id, first_pane);
        drop(b_writer);
        a_task.abort();
        b_task.abort();
    }

    #[tokio::test]
    async fn rejected_interactive_command_keeps_attached_socket_usable() {
        let session = Arc::new(Session::spawn(80, 24, "error-view".into()).expect("spawn session"));
        let (server, client) = UnixStream::pair().expect("socket pair");
        let task = tokio::spawn(serve_client(server, session));
        let (reader, mut writer) = client.into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(
                &encode(&ClientMessage::Hello {
                    cols: 80,
                    rows: 24,
                    version: PROTOCOL_VERSION,
                })
                .unwrap(),
            )
            .await
            .expect("hello");
        assert!(matches!(
            next_server_message(&mut lines).await,
            ServerMessage::Welcome { .. }
        ));
        let layout = match next_server_message(&mut lines).await {
            ServerMessage::Layout(layout) => layout,
            other => panic!("expected layout, got {other:?}"),
        };
        writer
            .write_all(
                &encode(&ClientMessage::SetWorkspaceColor {
                    id: layout.active_workspace,
                    color: Some("not-a-color".into()),
                })
                .unwrap(),
            )
            .await
            .expect("bad command");
        assert!(next_error(&mut lines).await.contains("workspace color"));
        writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Layout)).unwrap())
            .await
            .expect("follow-up query");
        let recovered = next_layout_matching(&mut lines, |_| true).await;
        assert_eq!(recovered.active_workspace, layout.active_workspace);
        drop(writer);
        task.abort();
    }

    #[test]
    fn is_hex_color_requires_hash_and_six_hex_digits() {
        assert!(is_hex_color("#001122"));
        assert!(is_hex_color("#AbCdEf"));
        assert!(!is_hex_color("#0011"));
        assert!(!is_hex_color("#00112g"));
        assert!(!is_hex_color("001122"));
        assert!(!is_hex_color("red"));
    }

    #[tokio::test]
    async fn set_workspace_color_validates_and_clears() {
        let session = Session::spawn(80, 24, "color".into()).expect("spawn session");
        let id = session.snapshot().expect("snapshot").workspaces[0].id;
        // Bad values are rejected without mutating the workspace.
        assert!(session
            .handle(ClientMessage::SetWorkspaceColor {
                id,
                color: Some("red".into()),
            })
            .is_err());
        assert!(session
            .handle(ClientMessage::SetWorkspaceColor {
                id,
                color: Some("#12345".into()),
            })
            .is_err());
        assert_eq!(session.snapshot().unwrap().workspaces[0].color, None);
        // A valid `#rrggbb` sticks and `None` clears it again.
        session
            .handle(ClientMessage::SetWorkspaceColor {
                id,
                color: Some("#AbCdEf".into()),
            })
            .expect("valid color accepted");
        assert_eq!(
            session.snapshot().unwrap().workspaces[0].color.as_deref(),
            Some("#AbCdEf")
        );
        session
            .handle(ClientMessage::SetWorkspaceColor { id, color: None })
            .expect("clear accepted");
        assert_eq!(session.snapshot().unwrap().workspaces[0].color, None);
    }

    #[tokio::test]
    async fn guarded_prompt_rejects_replacement_and_blocked_agent_before_write() {
        let session = Session::spawn(80, 24, "guarded-prompt".into()).expect("spawn");
        *session.manifests.lock().unwrap() = vec![manifest::Manifest {
            name: "fake".into(),
            display: "Fake Agent".into(),
            process: vec!["fake-agent".into()],
            title: vec![],
            resume: None,
            rules: vec![],
            source: manifest::ManifestSource::Builtin,
        }];
        let pane_id = session.snapshot().expect("snapshot").panes[0].id;
        let pane = session.panes.lock().unwrap().get(&pane_id).unwrap().clone();
        {
            let mut process = pane.process.lock().unwrap();
            process.name = Some("fake-agent".into());
            process.checked_at = Instant::now();
        }
        let first = session.pane_snapshot(pane_id).expect("recognized pane");
        assert_eq!(first.agent.as_deref(), Some("Fake Agent"));

        // Same pane id but a new foreground shell invalidates the old guard.
        pane.process.lock().unwrap().name = Some("sh".into());
        assert!(session
            .prompt_agent(
                pane_id,
                "Fake Agent",
                first.agent_generation,
                b"must-not-write",
            )
            .is_err());

        pane.process.lock().unwrap().name = Some("fake-agent".into());
        let current = session.pane_snapshot(pane_id).expect("agent restored");
        *pane.hook.lock().unwrap() = Some(ReportedHook {
            state: AgentStateKind::Blocked,
            source: "test".into(),
            agent: None,
            process_pid: None,
            process_name: None,
            reported_at: Instant::now(),
        });
        assert!(session
            .prompt_agent(
                pane_id,
                "Fake Agent",
                current.agent_generation,
                b"must-not-write",
            )
            .is_err());
    }

    #[tokio::test]
    async fn recognized_hook_identity_retires_after_a_non_shell_replacement() {
        let session = Session::spawn(80, 24, "hook-wrapper-identity".into()).unwrap();
        let pane_id = session.snapshot().unwrap().panes[0].id;
        let pane = Arc::clone(&session.panes.lock().unwrap()[&pane_id]);
        {
            let mut process = pane.process.lock().unwrap();
            process.pid = Some(100);
            process.name = Some("node".into());
            process.checked_at = Instant::now();
        }
        *pane.hook.lock().unwrap() = Some(ReportedHook {
            state: AgentStateKind::Working,
            source: "kodade:pi".into(),
            agent: Some("Pi".into()),
            process_pid: Some(100),
            process_name: Some("node".into()),
            reported_at: Instant::now(),
        });
        let pi = session.pane_snapshot(pane_id).unwrap();
        assert_eq!(pi.agent.as_deref(), Some("Pi"));

        {
            let mut process = pane.process.lock().unwrap();
            process.pid = Some(101);
            process.name = Some("sleep".into());
        }
        assert_eq!(session.pane_snapshot(pane_id).unwrap().agent, None);
        assert!(session
            .prompt_agent(pane_id, "Pi", pi.agent_generation, b"must-not-write")
            .is_err());
    }

    #[tokio::test]
    async fn existing_pane_hooks_keep_their_stable_socket_after_session_rename() {
        let unique = format!(
            "hook-rename-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let renamed = format!("{unique}-new");
        let public = socket_path(&unique);
        let stable = hook_socket_path();
        let new_public = socket_path(&renamed);
        let _ = fs::remove_file(&public);
        let _ = fs::remove_file(&stable);
        let _ = fs::remove_file(&new_public);
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::create_dir_all(stable.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&public).unwrap();
        fs::hard_link(&public, &stable).unwrap();
        let session = Arc::new(Session::spawn(80, 24, unique.clone()).expect("spawn session"));

        session.rename_session(&renamed).expect("rename session");
        let pane = session.snapshot().unwrap().panes[0].id;
        let server = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept hook reporter");
                let _ = serve_client(stream, session).await;
            })
        };
        let shim = std::env::temp_dir().join(format!("kodade-hook-report-{unique}"));
        fs::write(
            &shim,
            "#!/usr/bin/env python3\nimport json, os, socket, sys\ns = socket.socket(socket.AF_UNIX)\ns.connect(os.environ['KODADE_SOCKET'])\ns.sendall((json.dumps({'AgentState': {'pane': int(sys.argv[3]), 'state': sys.argv[4], 'source': 'hook'}}) + '\\n').encode())\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
        let command = format!(
            "KODADE_BIN={}; export KODADE_BIN; KODADE_INTEGRATION=kodade-cli; if [ -n \"${{KODADE_PANE:-}}\" ] && [ -n \"${{KODADE_SOCKET:-}}\" ]; then \"$KODADE_BIN\" agent report \"$KODADE_PANE\" working; fi\n",
            shim.display()
        );
        session
            .panes
            .lock()
            .unwrap()
            .get(&pane)
            .unwrap()
            .write(command.as_bytes())
            .unwrap();
        let mut seen = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if session
                .snapshot()
                .is_ok_and(|layout| layout.panes[0].state == AgentStateKind::Working)
            {
                seen = true;
                break;
            }
        }
        assert!(
            seen,
            "existing pane hook did not report through the stable socket: {:?}",
            session.snapshot().unwrap().panes[0].screen.contents
        );
        assert!(stable.exists());
        server.await.unwrap();

        let _ = fs::remove_file(&shim);
        let _ = fs::remove_file(&stable);
        let _ = fs::remove_file(&new_public);
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
                        let _ = serve_client(stream, session).await;
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
            .write_all(
                &encode(&ClientMessage::Hello {
                    cols: 80,
                    rows: 24,
                    version: PROTOCOL_VERSION,
                })
                .unwrap(),
            )
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
                    native_session: None,
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

    #[tokio::test]
    async fn version_mismatch_is_rejected_and_probe_answered() {
        let directory =
            std::env::temp_dir().join(format!("kodade-cli-version-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("version.sock");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let session = Arc::new(Session::spawn(80, 24, "version".into()).expect("spawn session"));
        let accept = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let session = Arc::clone(&session);
                    tokio::spawn(async move {
                        let _ = serve_client(stream, session).await;
                    });
                }
            })
        };

        // A Hello carrying the wrong version gets an Error and the socket closes.
        let (reader, mut writer) = UnixStream::connect(&socket)
            .await
            .expect("connect client")
            .into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(
                &encode(&ClientMessage::Hello {
                    cols: 80,
                    rows: 24,
                    version: PROTOCOL_VERSION + 1,
                })
                .unwrap(),
            )
            .await
            .expect("send hello");
        match next_server_message(&mut lines).await {
            ServerMessage::Error { message } => {
                assert!(message.contains("protocol version mismatch"));
                assert!(message.contains(&format!("daemon {PROTOCOL_VERSION}")));
            }
            other => panic!("expected an error, got {other:?}"),
        }
        // The daemon closed the connection after the mismatch.
        assert!(lines.next_line().await.expect("read").is_none());

        // A fresh connection can probe the version cheaply.
        let (reader, mut writer) = UnixStream::connect(&socket)
            .await
            .expect("connect probe")
            .into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Version)).unwrap())
            .await
            .expect("send probe");
        assert_eq!(
            next_server_message(&mut lines).await,
            ServerMessage::Version {
                version: PROTOCOL_VERSION
            }
        );

        accept.abort();
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir(&directory);
    }
    #[tokio::test]
    async fn subscribe_streams_agent_state_changes() {
        let directory =
            std::env::temp_dir().join(format!("kodade-cli-events-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("events.sock");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let session = Arc::new(Session::spawn(80, 24, "events".into()).expect("spawn session"));
        let accept = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let session = Arc::clone(&session);
                    tokio::spawn(async move {
                        let _ = serve_client(stream, session).await;
                    });
                }
            })
        };

        // Subscriber: the reply snapshot also settles the pane's baseline state.
        let (reader, mut writer) = UnixStream::connect(&socket)
            .await
            .expect("connect subscriber")
            .into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(&encode(&ClientMessage::Subscribe).unwrap())
            .await
            .expect("subscribe");
        let pane = match next_server_message(&mut lines).await {
            ServerMessage::Layout(layout) => layout.panes[0].id,
            other => panic!("expected the subscribe reply layout, got {other:?}"),
        };

        // A hook-style connection reports the pane blocked.
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
                    native_session: None,
                })
                .unwrap(),
            )
            .await
            .expect("report blocked");

        let event = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let ServerMessage::Event(Event::AgentStateChanged { pane, from, to }) =
                    next_server_message(&mut lines).await
                {
                    break (pane, from, to);
                }
            }
        })
        .await
        .expect("agent state change arrives within 2s");
        assert_eq!(event.0, pane);
        assert_eq!(event.2, AgentStateKind::Blocked);

        accept.abort();
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir(&directory);
    }

    #[tokio::test]
    async fn applying_an_exported_layout_keeps_live_panes() {
        let session = Session::spawn(80, 24, "apply".into()).expect("spawn session");
        session
            .handle(ClientMessage::SplitRight)
            .expect("split the first pane");
        let exported = session.build_file();
        let before: Vec<PaneId> = {
            let panes = session.panes.lock().expect("pane lock");
            let mut ids: Vec<_> = panes.keys().copied().collect();
            ids.sort_by_key(|id| id.0);
            ids
        };
        assert_eq!(before.len(), 2);

        // Re-applying the same file is a no-op for live panes.
        session
            .handle(ClientMessage::ApplyLayout(exported.clone()))
            .expect("apply the exported layout");
        let after: Vec<PaneId> = {
            let panes = session.panes.lock().expect("pane lock");
            let mut ids: Vec<_> = panes.keys().copied().collect();
            ids.sort_by_key(|id| id.0);
            ids
        };
        assert_eq!(before, after);

        // Dropping a pane from the file closes it and rebuilds the tree.
        let mut trimmed = exported;
        let tab = &mut trimmed.workspaces[0].tabs[0];
        let keep = before[0];
        tab.tree = LayoutTree::Leaf { pane: keep };
        tab.focused = keep.0;
        tab.panes.retain(|pane| pane.id == keep.0);
        session
            .handle(ClientMessage::ApplyLayout(trimmed))
            .expect("apply the trimmed layout");
        let remaining: Vec<PaneId> = session
            .panes
            .lock()
            .expect("pane lock")
            .keys()
            .copied()
            .collect();
        assert_eq!(remaining, vec![keep]);
    }
    #[tokio::test]
    async fn moving_a_workspaces_last_pane_leaves_it_a_working_tab() {
        // Regression: the move used to remove the source workspace's only tab,
        // after which any `active_tab` lookup panicked and poisoned the lock.
        let session = Session::spawn(80, 24, "move".into()).expect("spawn session");
        session
            .handle(ClientMessage::NewWorkspace {
                name: "two".into(),
                root: None,
                env: HashMap::new(),
            })
            .expect("second workspace");
        let (first_workspace, moved_pane, target_tab) = {
            let state = session.state.lock().expect("state lock");
            (
                state.workspaces[0].id,
                state.workspaces[0].tabs[0].focused,
                state.workspaces[1].tabs[0].id,
            )
        };
        session
            .handle(ClientMessage::MovePaneToTab {
                pane: moved_pane,
                tab: target_tab,
            })
            .expect("move across workspaces");
        {
            let state = session.state.lock().expect("state lock");
            let source = &state.workspaces[0];
            assert_eq!(source.id, first_workspace);
            assert_eq!(source.tabs.len(), 1, "source workspace kept a tab");
            assert!(
                source.tabs.iter().any(|tab| tab.id == source.active_tab),
                "active tab still resolves"
            );
            // The moved pane now lives in the target workspace's tab.
            assert!(layout::contains(
                &state.workspaces[1].tabs[0].tree,
                moved_pane
            ));
        }
        // Both of these used to panic on `active tab exists`.
        session
            .handle(ClientMessage::SelectWorkspace {
                id: first_workspace,
            })
            .expect("select the source workspace");
        session.snapshot().expect("snapshot after the move");
    }

    #[tokio::test]
    async fn failed_worktree_removal_keeps_its_workspace_open() {
        let base = std::env::temp_dir().join(format!(
            "kodade-cli-worktree-remove-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = base.join("repo");
        let worktree = base.join("worktree");
        fs::create_dir_all(&repo).expect("create repo");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .expect("run git")
                .success());
        }
        fs::write(repo.join("README.md"), "base\n").expect("seed repo");
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "."])
            .status()
            .expect("stage")
            .success());
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "-q", "-m", "base"])
            .status()
            .expect("commit")
            .success());
        git::worktree_add(&repo, "feature", None, &worktree).expect("add worktree");
        fs::write(worktree.join("dirty.txt"), "preserve me\n").expect("dirty worktree");

        let session = Session::spawn(80, 24, "dirty-worktree".into()).expect("spawn session");
        session
            .open_worktree_workspace(repo.clone(), worktree.clone())
            .expect("open existing worktree workspace");
        let worktree = worktree.canonicalize().expect("canonical worktree");
        let workspace = session
            .state
            .lock()
            .expect("state")
            .workspaces
            .iter()
            .find(|workspace| workspace.root.as_deref() == Some(worktree.as_path()))
            .expect("worktree workspace")
            .id;

        assert!(session.remove_worktree_workspace(workspace, false).is_err());
        assert!(worktree.join("dirty.txt").exists());
        assert!(session
            .state
            .lock()
            .expect("state")
            .workspaces
            .iter()
            .any(|item| item.id == workspace));

        git::worktree_remove(&repo, &worktree, true).expect("forced cleanup");
        fs::remove_dir_all(base).ok();
    }

    #[tokio::test]
    async fn workspace_environment_reaches_future_pty_panes_and_persists() {
        let session = Session::spawn(80, 24, "workspace-env".into()).expect("spawn session");
        let env = HashMap::from([("KODADE_ISSUE52".into(), "per-workspace".into())]);
        session
            .handle(ClientMessage::NewWorkspace {
                name: "environment".into(),
                root: None,
                env: env.clone(),
            })
            .expect("create workspace");
        let workspace = session.state.lock().expect("state").active_workspace;
        for name in ["one", "two"] {
            session
                .handle(ClientMessage::NewPane {
                    workspace: Some(workspace),
                    tab: None,
                    split: None,
                    command: Some(vec![
                        "sh".into(),
                        "-c".into(),
                        "printf %s \"$KODADE_ISSUE52\"; sleep 1".into(),
                    ]),
                    name: Some(name.into()),
                    context: None,
                })
                .expect("spawn pane");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let contents: Vec<_> = session
            .panes
            .lock()
            .expect("panes")
            .values()
            .map(|pane| pane.snapshot().0.contents)
            .collect();
        assert_eq!(
            contents
                .iter()
                .filter(|text| text.contains("per-workspace"))
                .count(),
            2
        );
        assert_eq!(
            session
                .build_file()
                .workspaces
                .iter()
                .find(|item| item.id == workspace.0)
                .unwrap()
                .env,
            env
        );
        let restored = Session::restore(session.build_file(), false).expect("cold restore");
        let restored_workspace = restored
            .state
            .lock()
            .expect("state")
            .workspaces
            .iter()
            .find(|workspace| workspace.name == "environment")
            .expect("restored workspace")
            .id;
        restored
            .handle(ClientMessage::NewPane {
                workspace: Some(restored_workspace),
                tab: None,
                split: None,
                command: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "printf %s \"$KODADE_ISSUE52\"; sleep 1".into(),
                ]),
                name: Some("restored".into()),
                context: None,
            })
            .expect("spawn restored pane");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(restored
            .panes
            .lock()
            .expect("panes")
            .values()
            .any(|pane| pane.snapshot().0.contents.contains("per-workspace")));
    }

    #[tokio::test]
    async fn daemon_rejects_invalid_and_reserved_workspace_environment() {
        let session = Session::spawn(80, 24, "workspace-env-invalid".into()).expect("spawn");
        for env in [
            HashMap::from([("KODADE_SOCKET".into(), "forged".into())]),
            HashMap::from([("1INVALID".into(), "value".into())]),
        ] {
            assert!(session
                .handle(ClientMessage::NewWorkspace {
                    name: "invalid".into(),
                    root: None,
                    env,
                })
                .is_err());
        }
    }

    #[tokio::test]
    async fn pane_query_reaches_panes_outside_the_active_tab() {
        let directory =
            std::env::temp_dir().join(format!("kodade-cli-panequery-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("panequery.sock");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let session = Arc::new(Session::spawn(80, 24, "panequery".into()).expect("spawn session"));
        let background = {
            let state = session.state.lock().expect("state lock");
            state.workspaces[0].tabs[0].focused
        };
        // A second tab takes over, leaving the first pane in the background.
        session.handle(ClientMessage::NewTab).expect("new tab");
        let accept = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let session = Arc::clone(&session);
                    tokio::spawn(async move {
                        let _ = serve_client(stream, session).await;
                    });
                }
            })
        };
        let (reader, mut writer) = UnixStream::connect(&socket)
            .await
            .expect("connect client")
            .into_split();
        let mut lines = BufReader::new(reader).lines();

        // The layout only carries the active tab, which is why `agent wait`
        // polls `Query(Pane)` instead.
        writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Layout)).unwrap())
            .await
            .expect("query layout");
        match next_server_message(&mut lines).await {
            ServerMessage::Layout(layout) => {
                assert!(layout.panes.iter().all(|pane| pane.id != background));
            }
            other => panic!("expected a layout, got {other:?}"),
        }

        writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Pane(background))).unwrap())
            .await
            .expect("query pane");
        match next_server_message(&mut lines).await {
            ServerMessage::Pane(pane) => assert_eq!(pane.id, background),
            other => panic!("expected the background pane, got {other:?}"),
        }

        // An unknown pane is a clean error, not a hang.
        writer
            .write_all(&encode(&ClientMessage::Query(QueryKind::Pane(PaneId(9999)))).unwrap())
            .await
            .expect("query missing pane");
        assert!(matches!(
            next_server_message(&mut lines).await,
            ServerMessage::Error { .. }
        ));

        accept.abort();
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir(&directory);
    }

    #[tokio::test]
    async fn subscriber_tick_detects_state_changes_with_no_client_attached() {
        let directory =
            std::env::temp_dir().join(format!("kodade-cli-tick-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create test directory");
        let socket = directory.join("tick.sock");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let session = Arc::new(Session::spawn(80, 24, "tick".into()).expect("spawn session"));
        let accept = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let session = Arc::clone(&session);
                    tokio::spawn(async move {
                        let _ = serve_client(stream, session).await;
                    });
                }
            })
        };
        // A bare `kodade-cli events` subscriber: no Hello, no TUI anywhere.
        let (reader, mut writer) = UnixStream::connect(&socket)
            .await
            .expect("connect subscriber")
            .into_split();
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(&encode(&ClientMessage::Subscribe).unwrap())
            .await
            .expect("subscribe");
        let pane = match next_server_message(&mut lines).await {
            ServerMessage::Layout(layout) => layout.panes[0].id,
            other => panic!("expected the subscribe reply layout, got {other:?}"),
        };
        // The daemon counts the subscription, which is what arms the tick.
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.subscribers.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("subscription is counted");

        // Report the state directly on the session: no connection sends a
        // message, so nothing but the tick can run detection.
        session
            .handle(ClientMessage::AgentState {
                pane,
                state: AgentStateKind::Blocked,
                source: "test".into(),
                native_session: None,
            })
            .expect("report blocked");
        tokio::spawn(subscriber_tick(Arc::clone(&session)));

        let event = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let ServerMessage::Event(Event::AgentStateChanged { pane, to, .. }) =
                    next_server_message(&mut lines).await
                {
                    break (pane, to);
                }
            }
        })
        .await
        .expect("the tick delivers the transition");
        assert_eq!(event, (pane, AgentStateKind::Blocked));

        accept.abort();
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir(&directory);
    }
}
