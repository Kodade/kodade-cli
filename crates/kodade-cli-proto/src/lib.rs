//! Shared Ködade CLI socket protocol.
//!
//! Each JSON message is UTF-8 and terminated by one newline. Message payloads
//! that contain byte streams use serde's JSON byte-array representation.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Versioned, local extension manifest shared by the CLI and detached daemon.
/// The manifest remains deliberately small: commands run through the user's
/// shell, while Ködade supplies only session/workspace/pane context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub min_kodade_version: Option<String>,
    /// Optional build command run only for a managed GitHub installation.
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub actions: Vec<PluginAction>,
    #[serde(default)]
    pub startup: Vec<PluginHook>,
    #[serde(default)]
    pub events: Vec<PluginEventHook>,
    #[serde(default)]
    pub panes: Vec<PluginPane>,
    #[serde(default)]
    pub link_handlers: Vec<PluginLinkHandler>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginAction {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub pane: bool,
    /// Where this action is meaningful. An empty list preserves the original
    /// always-available action behavior.
    #[serde(default)]
    pub contexts: Vec<PluginActionContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActionContext {
    Global,
    Workspace,
    Tab,
    Pane,
    Selection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLinkHandler {
    pub id: String,
    pub title: String,
    /// A regular expression matched against a ctrl-clicked URL.
    pub pattern: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHook {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEventHook {
    pub event: String,
    pub command: String,
}

/// Data supplied to an extension command. It travels with `NewPane` so the
/// daemon can own the private context file for the entire pane lifetime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationContext {
    pub endpoint: String,
    pub workspace: Option<String>,
    pub workspace_id: Option<String>,
    pub tab: Option<String>,
    pub tab_id: Option<String>,
    pub pane: Option<String>,
    pub cwd: Option<PathBuf>,
    pub selected_text: Option<String>,
    pub clicked_url: Option<String>,
}

impl InvocationContext {
    pub fn supports_contexts(&self, contexts: &[PluginActionContext]) -> bool {
        contexts.iter().all(|scope| match scope {
            PluginActionContext::Global => true,
            PluginActionContext::Workspace => self.workspace_id.is_some(),
            PluginActionContext::Tab => self.tab_id.is_some(),
            PluginActionContext::Pane => self.pane.is_some(),
            PluginActionContext::Selection => self
                .selected_text
                .as_ref()
                .is_some_and(|text| !text.is_empty()),
        })
    }

    pub fn supports(&self, action: &PluginAction) -> bool {
        action.contexts.is_empty() || self.supports_contexts(&action.contexts)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPane {
    pub name: String,
    pub command: String,
}

/// Validate a manifest before either client or detached daemon executes it.
/// Keeping this beside the shared schema prevents the two processes from
/// accepting different extension contracts.
pub fn validate_plugin_manifest(manifest: &PluginManifest, current_version: &str) -> Result<()> {
    if manifest.manifest_version != 1 {
        bail!(
            "unsupported plugin manifest version {}",
            manifest.manifest_version
        );
    }
    if !plugin_id(&manifest.id) {
        bail!("plugin id must use lowercase letters, digits, '-' or '_'");
    }
    if manifest.name.trim().is_empty() || manifest.version.trim().is_empty() {
        bail!("plugin name and version are required");
    }
    if let Some(required) = &manifest.min_kodade_version {
        if plugin_version_gt(required, current_version)? {
            bail!("plugin requires Ködade CLI {required} or newer");
        }
    }
    let mut action_ids = std::collections::HashSet::new();
    for action in &manifest.actions {
        if !plugin_id(&action.id)
            || action.name.trim().is_empty()
            || action.command.trim().is_empty()
            || !action_ids.insert(&action.id)
        {
            bail!("plugin actions need unique ids, names, and commands");
        }
    }
    let action_ids: std::collections::HashSet<_> = manifest
        .actions
        .iter()
        .map(|action| action.id.as_str())
        .collect();
    let mut handler_ids = std::collections::HashSet::new();
    for handler in &manifest.link_handlers {
        if !plugin_id(&handler.id)
            || handler.title.trim().is_empty()
            || handler.pattern.trim().is_empty()
            || !action_ids.contains(handler.action.as_str())
            || !handler_ids.insert(&handler.id)
        {
            bail!("plugin link handlers need unique ids, a pattern, and a known action");
        }
    }
    if manifest
        .panes
        .iter()
        .any(|pane| pane.name.trim().is_empty() || pane.command.trim().is_empty())
    {
        bail!("plugin panes need names and commands");
    }
    if manifest
        .startup
        .iter()
        .any(|hook| hook.command.trim().is_empty())
    {
        bail!("plugin hook command cannot be empty");
    }
    if manifest
        .events
        .iter()
        .any(|hook| hook.event.trim().is_empty() || hook.command.trim().is_empty())
    {
        bail!("plugin event hooks need an event and command");
    }
    Ok(())
}
fn plugin_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}
fn plugin_version_gt(required: &str, current: &str) -> Result<bool> {
    fn parts(value: &str) -> Result<Vec<u64>> {
        value
            .split('.')
            .map(|part| {
                part.parse()
                    .map_err(|_| anyhow!("invalid version {value:?}"))
            })
            .collect()
    }
    Ok(parts(required)? > parts(current)?)
}

/// Wire protocol version. Bumped whenever a client and daemon can no longer
/// understand each other. Both ends compare it at attach time (see `Hello` /
/// `Welcome`) so a stale binary fails fast instead of misbehaving (#23).
pub const PROTOCOL_VERSION: u32 = 1;

// No `Eq`: `ApplyLayout` carries the split ratios, which are floats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Query(QueryKind),
    /// Turn this connection into an event stream: every later `Event` raised by
    /// the session is pushed as `ServerMessage::Event` (#16).
    Subscribe,
    /// Replace the session layout with a persisted one. Panes whose ids are
    /// still alive are kept; the rest are spawned fresh (`layout apply`).
    ApplyLayout(SessionFile),
    Hello {
        cols: u16,
        rows: u16,
        /// Protocol version the client speaks. `#[serde(default)]` decodes an
        /// old client that never sent the field as version 0, so the daemon can
        /// still report a clear mismatch instead of failing to parse (#23).
        #[serde(default)]
        version: u32,
    },
    Input {
        bytes: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Per-client narrow projection: show only the focused pane while keeping
    /// the shared tab tree and its other PTYs intact.
    SetCompactView {
        enabled: bool,
    },
    SplitRight,
    SplitDown,
    ClosePane,
    CloseTab {
        id: TabId,
    },
    CloseWorkspace {
        id: WorkspaceId,
    },
    FocusPane {
        direction: Direction,
    },
    FocusPaneId {
        id: PaneId,
    },
    SendToPane {
        id: PaneId,
        bytes: Vec<u8>,
    },
    /// Upload a bounded PNG and paste its private path, without submitting input.
    PasteImage {
        pane: PaneId,
        data: String,
    },
    /// Atomically verify a recognized agent identity then write to its pane.
    /// Scripts use this for prompts so a query/write gap cannot send input to
    /// a replacement shell or another agent in the same pane.
    PromptAgent {
        pane: PaneId,
        expected_agent: String,
        expected_generation: u64,
        bytes: Vec<u8>,
    },
    RenamePaneId {
        id: PaneId,
        name: String,
    },
    KillSession,
    NewTab,
    NextTab,
    PrevTab,
    SelectTab {
        id: TabId,
    },
    /// One-based position of a tab in the active workspace (`select_tab_1..9`).
    SelectTabIndex {
        index: u8,
    },
    /// Reorder the active tab by `delta` positions, clamped to the ends.
    MoveTab {
        delta: i8,
    },
    /// Swap the focused pane with its neighbour in `direction`.
    SwapPane {
        direction: Direction,
    },
    /// Move the focused pane out of its tab and into a new one.
    BreakPane,
    /// Reset every split ratio in the active tab to 0.5.
    EqualizeLayout,
    /// Focus the next / previous leaf of the active tab.
    FocusPaneCycle {
        forward: bool,
    },
    /// Activate the workspace `delta` positions away, wrapping.
    SelectWorkspaceDelta {
        delta: i8,
    },
    RenameTabId {
        id: TabId,
        name: String,
    },
    RenameWorkspaceId {
        id: WorkspaceId,
        name: String,
    },
    NewWorkspace {
        name: String,
        /// Root directory new panes in this workspace start in.
        root: Option<PathBuf>,
    },
    /// Create a pane; `split: None` opens a new tab, otherwise it splits the
    /// focused pane. The new pane becomes focused so the reply snapshot names it.
    NewPane {
        workspace: Option<WorkspaceId>,
        tab: Option<TabId>,
        split: Option<SplitAxis>,
        /// Run this command through the login shell instead of an interactive one.
        command: Option<Vec<String>>,
        name: Option<String>,
        /// Extension context whose private file is created and held by the
        /// daemon only after this request is accepted.
        context: Option<Box<InvocationContext>>,
    },
    SelectWorkspace {
        id: WorkspaceId,
    },
    RenamePane {
        name: String,
    },
    RenameTab {
        name: String,
    },
    RenameWorkspace {
        name: String,
    },
    ResizePane {
        direction: Direction,
        cells: i16,
    },
    /// Positive deltas move back through terminal history.
    ScrollPane {
        id: PaneId,
        delta: i16,
    },
    /// Read a pane's text for copy mode / `pane read`. When `scrollback`, the
    /// reply carries the full scrollback plus the visible screen; otherwise only
    /// the visible screen. `lines` keeps just the last N lines when set.
    ReadPane {
        id: PaneId,
        scrollback: bool,
        lines: Option<usize>,
    },
    ZoomPane,
    /// Move a pane out of its tab and into an existing one, splitting that
    /// tab's focused pane (`pane move PANE --tab TAB`).
    MovePaneToTab {
        pane: PaneId,
        tab: TabId,
    },
    /// Rename the live session: the socket file and the state file move with it
    /// (`session rename NAME`).
    RenameSession {
        name: String,
    },
    AgentState {
        pane: PaneId,
        state: AgentStateKind,
        source: String,
        /// Optional native conversation identity reported by an agent hook.
        /// Older clients omit this field and continue to decode normally.
        #[serde(default)]
        native_session: Option<NativeSession>,
    },
    /// Set (or clear) a workspace's sidebar swatch color, as a `#rrggbb` hex
    /// string. `None` clears it back to the auto-hashed fallback (#19).
    SetWorkspaceColor {
        id: WorkspaceId,
        color: Option<String>,
    },
    /// Create a git-worktree workspace: `git worktree add` for `branch` (created
    /// from `from`, or checked out if it already exists) under `[worktrees]
    /// directory`, then a workspace `repo:branch` rooted there with a shell tab (#22).
    NewWorktreeWorkspace {
        repo_root: PathBuf,
        branch: String,
        from: Option<String>,
    },
    /// Close a worktree workspace; unless `keep`, also `git worktree remove` its
    /// directory (#22).
    RemoveWorktreeWorkspace {
        id: WorkspaceId,
        keep: bool,
    },
    /// Re-read bundled and user override detection manifests without restarting
    /// the daemon. The old set remains active when the replacement is invalid.
    ReloadManifests,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryKind {
    Layout,
    /// One pane's snapshot, wherever it lives. Unlike `Layout` (active tab
    /// only) this reaches panes in background tabs and workspaces, which is
    /// what `agent wait` / `pane wait-output` poll (#16).
    Pane(PaneId),
    /// Fetch an image only when its revision changes; layouts carry metadata only.
    Image {
        pane: PaneId,
        id: u32,
        revision: u64,
    },
    /// Cheap version probe: the daemon replies with `ServerMessage::Version` and
    /// nothing else, so `--remote` can check compatibility before attaching (#23).
    Version,
    /// The persisted-layout view of the session (`layout export`).
    Session,
    /// The protocol schema: version plus the message names this daemon knows.
    Schema,
    /// The active manifest set, including whether each entry is bundled or a
    /// user override. This is intentionally metadata, not screen rules.
    Manifests,
}

/// Inspectable metadata for one active agent detection manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestInfo {
    pub name: String,
    pub display: String,
    pub process: Vec<String>,
    pub title: Vec<String>,
    pub rules: usize,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    ImagePasted {
        pane: PaneId,
        path: PathBuf,
    },
    Welcome {
        session: String,
        /// Protocol version the daemon speaks; an unexpected value makes the
        /// client abort before touching the terminal (#23).
        #[serde(default)]
        version: u32,
    },
    /// Reply to `Query(Version)`.
    Version {
        version: u32,
    },
    Layout(LayoutSnapshot),
    /// Pushed to every attached client that has not subscribed when a known
    /// agent transitions into `blocked` or `done` (#10). Subscribed clients get
    /// the same payload as `Event::Notification` instead.
    Notification(Notification),
    /// Reply to `ReadPane`: `text` is the joined pane text and `scrollback_lines`
    /// is the number of lines it contains (after any `lines` truncation).
    PaneText {
        id: PaneId,
        text: String,
        scrollback_lines: usize,
    },
    /// Pushed only to connections that sent `Subscribe` (#16).
    Event(Event),
    /// Reply to `Query(QueryKind::Pane(_))`.
    Pane(PaneSnapshot),
    Image {
        pane: PaneId,
        image: ImageData,
    },
    /// Reply to `Query(QueryKind::Session)` — the persisted layout.
    Session(SessionFile),
    /// Reply to `Query(QueryKind::Schema)`.
    Schema {
        version: u32,
        client_messages: Vec<String>,
        server_messages: Vec<String>,
    },
    /// Reply to `Query(Manifests)` and `ReloadManifests`.
    Manifests(Vec<ManifestInfo>),
    Error {
        message: String,
    },
    Shutdown,
}

/// Session events delivered on a subscribed connection. Ids are resolved
/// against the latest `LayoutSnapshot` by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    AgentStateChanged {
        pane: PaneId,
        from: AgentStateKind,
        to: AgentStateKind,
    },
    PaneOpened {
        pane: PaneId,
    },
    PaneClosed {
        pane: PaneId,
    },
    TabOpened {
        tab: TabId,
    },
    TabClosed {
        tab: TabId,
    },
    TabRenamed {
        tab: TabId,
        name: String,
    },
    WorkspaceOpened {
        workspace: WorkspaceId,
    },
    WorkspaceClosed {
        workspace: WorkspaceId,
    },
    WorkspaceRenamed {
        workspace: WorkspaceId,
        name: String,
    },
    Notification(Notification),
    /// The session was renamed: `socket` is the path clients must use from now
    /// on (the old socket file is gone). Shells spawned before the rename keep
    /// the old `KODADE_SESSION` / `KODADE_SOCKET` values.
    SessionRenamed {
        name: String,
        socket: PathBuf,
    },
}

/// A single agent-state alert. Workspace/tab are carried as ids; the client
/// resolves their display names from the current `LayoutSnapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub pane: PaneId,
    pub workspace: WorkspaceId,
    pub tab: TabId,
    /// Agent display name (e.g. `codex`).
    pub agent: String,
    pub state: AgentStateKind,
    /// Monotonic per-session sequence so clients can drop duplicates.
    pub seq: u64,
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub u64);
    };
}

id_type!(WorkspaceId);
id_type!(TabId);
id_type!(PaneId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStateKind {
    Blocked,
    Working,
    Done,
    Idle,
    Unknown,
}

/// A native agent conversation that can be resumed independently of its cwd.
/// Values are treated as data and are later passed as individual argv entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSession {
    pub source: String,
    pub agent: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LayoutTree {
    Leaf {
        pane: PaneId,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<LayoutTree>,
        second: Box<LayoutTree>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub active: bool,
    pub state: AgentStateKind,
    /// Root directory new panes in this workspace start in, if one is set.
    pub root: Option<PathBuf>,
    /// Optional sidebar swatch color as `#rrggbb`; `None` uses the auto-hashed
    /// fallback in the client (#19). Older daemons omit it.
    #[serde(default)]
    pub color: Option<String>,
    /// Git branch of this workspace's root, when it is a repo; refreshed on the
    /// daemon's 2s process tick and shown dimmed in the sidebar (#22). Older
    /// daemons omit it.
    #[serde(default)]
    pub branch: Option<String>,
    /// For a git-worktree workspace, the workspace whose root is the main repo,
    /// so the sidebar can nest it under its parent (#22). `None` for a normal
    /// workspace or a worktree whose main repo has no open workspace.
    #[serde(default)]
    pub parent: Option<WorkspaceId>,
    /// Metadata for every tab, including panes outside the active screen.
    pub tabs: Vec<SidebarTabInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarTabInfo {
    pub id: TabId,
    pub name: String,
    pub state: AgentStateKind,
    pub agents: Vec<AgentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub pane: PaneId,
    pub name: String,
    pub state: AgentStateKind,
    /// Seconds the current state has held, for sidebar age labels.
    #[serde(default)]
    pub state_age_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: TabId,
    pub name: String,
    pub active: bool,
    pub state: AgentStateKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub id: PaneId,
    pub title: String,
    pub focused: bool,
    pub scroll_offset: usize,
    pub screen: Screen,
    pub agent: Option<String>,
    /// Changes whenever the recognized foreground-agent identity changes.
    /// A prompt carries this value back to the daemon as an identity guard.
    #[serde(default)]
    pub agent_generation: u64,
    /// Increments when this pane enters Working or Blocked. Prompt waiters use
    /// it to observe a fast lifecycle that completes between polling ticks.
    #[serde(default)]
    pub activity_revision: u64,
    pub state: AgentStateKind,
    pub state_reason: String,
    /// Seconds the current state has held (see daemon state_since tracking).
    #[serde(default)]
    pub state_age_secs: u64,
    /// Live working directory of the pane's foreground process, when known.
    /// #11 shows the basename; the full path travels on the wire.
    pub cwd: Option<PathBuf>,
}

/// A daemon-owned tree with terminal-independent pane contents. Clients choose pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutSnapshot {
    pub active_workspace: WorkspaceId,
    pub active_tab: TabId,
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub tree: LayoutTree,
    pub panes: Vec<PaneSnapshot>,
    pub zoomed: bool,
    /// True when this session was rebuilt from a persisted state file and no
    /// client has attached (`Hello`) since. Older daemons omit it (#9).
    #[serde(default)]
    pub restored: bool,
}

/// Attribute bits carried by a `Run`. Kept as bit flags so a styled row stays
/// small on the wire.
pub const ATTR_BOLD: u8 = 1;
pub const ATTR_ITALIC: u8 = 2;
pub const ATTR_UNDERLINE: u8 = 4;
pub const ATTR_DIM: u8 = 8;
pub const ATTR_INVERSE: u8 = 16;

/// A terminal cell color. `Indexed(0..16)` is mapped through the client theme's
/// `[ansi]` palette; higher indices use the standard xterm 256-color cube.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CellColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// A horizontal stretch of cells on one row that share fg, bg, and attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub text: String,
    pub fg: CellColor,
    pub bg: CellColor,
    pub attrs: u8,
}

/// One pane's visible terminal state. `contents` stays plain text for copy mode
/// and `pane read`; `rows` carries the styled cells the client draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Screen {
    pub contents: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    /// One entry per visible screen row, left to right.
    pub rows: Vec<Vec<Run>>,
    pub bracketed_paste: bool,
    pub mouse_reporting: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graphics: Vec<ImagePlacement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageData {
    pub id: u32,
    pub revision: u64,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    /// Validated base64 pixel/PNG data, bounded to 8 MiB decoded per image.
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePlacement {
    pub image: u32,
    pub revision: u64,
    pub placement: u32,
    pub row: i32,
    pub col: u16,
    pub cols: u16,
    pub rows: u16,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub z: i32,
}

/// Persisted-session file version understood by this build (#9).
pub const SESSION_FILE_VERSION: u32 = 1;

/// Settings shared by the client and daemon from `[session]` in config.toml.
/// Both features are opt-in because they can retain terminal data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettings {
    #[serde(default)]
    pub resume_agents: bool,
    #[serde(default)]
    pub pane_history: bool,
}

/// A persisted session layout. Lives in the proto crate because it travels on
/// the wire too (`layout export` / `layout apply`, #16). Unknown fields are
/// ignored and every field has a default so a partially written or older file
/// still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFile {
    pub version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub active_workspace: u64,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceFile {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Sidebar swatch color as `#rrggbb`, if the user set one (#19).
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub active_tab: u64,
    #[serde(default)]
    pub tabs: Vec<TabFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabFile {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub zoomed: bool,
    #[serde(default)]
    pub focused: u64,
    /// Pane tree; its leaf ids reference the `panes` list below.
    pub tree: LayoutTree,
    #[serde(default)]
    pub panes: Vec<PaneFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneFile {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// The command this pane was spawned with, if any (used for `resume_agents`).
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Hook-reported native conversation identity, when the agent supports it.
    #[serde(default)]
    pub native_session: Option<NativeSession>,
}

impl SessionFile {
    /// A file is usable only if it describes one unambiguous layout: at least
    /// one workspace/tab/pane, every tree leaf backed by a pane entry, every
    /// pane entry reachable from its tab's tree, and no id reused anywhere in
    /// the file. Ids must be unique across the whole file because `layout
    /// apply` adopts them verbatim — a duplicate would silently merge two
    /// panes. Focused / active ids are still tolerated (callers fall back).
    pub fn validate(&self) -> Result<()> {
        use std::collections::HashSet;

        if self.version != SESSION_FILE_VERSION {
            anyhow::bail!("unsupported session file version {}", self.version);
        }
        if self.workspaces.is_empty() {
            anyhow::bail!("session file has no workspaces");
        }
        let mut workspace_ids = HashSet::new();
        let mut tab_ids = HashSet::new();
        let mut pane_ids = HashSet::new();
        for workspace in &self.workspaces {
            if !workspace_ids.insert(workspace.id) {
                anyhow::bail!("duplicate workspace id {}", workspace.id);
            }
            if workspace.tabs.is_empty() {
                anyhow::bail!("workspace {} has no tabs", workspace.id);
            }
            for tab in &workspace.tabs {
                if !tab_ids.insert(tab.id) {
                    anyhow::bail!("duplicate tab id {}", tab.id);
                }
                if tab.panes.is_empty() {
                    anyhow::bail!("tab {} has no panes", tab.id);
                }
                let mut known = HashSet::new();
                for pane in &tab.panes {
                    if !pane_ids.insert(pane.id) {
                        anyhow::bail!("duplicate pane id {}", pane.id);
                    }
                    known.insert(pane.id);
                }
                let mut leaves = Vec::new();
                tree_leaves(&tab.tree, &mut leaves);
                if leaves.is_empty() {
                    anyhow::bail!("tab {} has an empty tree", tab.id);
                }
                let reachable: HashSet<u64> = leaves.iter().copied().collect();
                if reachable.len() != leaves.len() {
                    anyhow::bail!("tab {} names a pane twice in its tree", tab.id);
                }
                for leaf in &leaves {
                    if !known.contains(leaf) {
                        anyhow::bail!("tab {} tree references unknown pane {leaf}", tab.id);
                    }
                }
                // An entry no tree names would be spawned and never drawn.
                for pane in &tab.panes {
                    if !reachable.contains(&pane.id) {
                        anyhow::bail!("tab {} has pane {} outside its tree", tab.id, pane.id);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Collect the pane ids referenced by a tree's leaves.
pub fn tree_leaves(tree: &LayoutTree, output: &mut Vec<u64>) {
    match tree {
        LayoutTree::Leaf { pane } => output.push(pane.0),
        LayoutTree::Split { first, second, .. } => {
            tree_leaves(first, output);
            tree_leaves(second, output);
        }
    }
}

/// Socket schema version reported by `Query(QueryKind::Schema)`. Bump it when a
/// message changes shape in a way an older client cannot ignore (#23).
pub const SCHEMA_VERSION: u32 = 1;

/// Every `ClientMessage` variant name, hand-maintained so the schema query is a
/// stable contract. `client_message_name` keeps it honest at compile time and
/// the schema test keeps the counts in sync.
pub const CLIENT_MESSAGE_NAMES: &[&str] = &[
    "Query",
    "Subscribe",
    "ApplyLayout",
    "Hello",
    "Input",
    "Resize",
    "SetCompactView",
    "SplitRight",
    "SplitDown",
    "ClosePane",
    "CloseTab",
    "CloseWorkspace",
    "FocusPane",
    "FocusPaneId",
    "SendToPane",
    "PasteImage",
    "PromptAgent",
    "RenamePaneId",
    "KillSession",
    "RenameSession",
    "NewTab",
    "NextTab",
    "PrevTab",
    "SelectTab",
    "SelectTabIndex",
    "MoveTab",
    "MovePaneToTab",
    "SwapPane",
    "BreakPane",
    "EqualizeLayout",
    "FocusPaneCycle",
    "SelectWorkspaceDelta",
    "RenameTabId",
    "RenameWorkspaceId",
    "NewWorkspace",
    "NewPane",
    "SelectWorkspace",
    "RenamePane",
    "RenameTab",
    "RenameWorkspace",
    "ResizePane",
    "ScrollPane",
    "ReadPane",
    "ZoomPane",
    "AgentState",
    "SetWorkspaceColor",
    "NewWorktreeWorkspace",
    "RemoveWorktreeWorkspace",
    "ReloadManifests",
];

/// Every `ServerMessage` variant name (see [`CLIENT_MESSAGE_NAMES`]).
pub const SERVER_MESSAGE_NAMES: &[&str] = &[
    "ImagePasted",
    "Welcome",
    "Layout",
    "Notification",
    "PaneText",
    "Pane",
    "Image",
    "Version",
    "Event",
    "Session",
    "Schema",
    "Manifests",
    "Error",
    "Shutdown",
];

/// Variant name of a client message. The exhaustive match makes a new variant a
/// compile error until it is named here (and, via the test, in the list above).
pub fn client_message_name(message: &ClientMessage) -> &'static str {
    match message {
        ClientMessage::Query(_) => "Query",
        ClientMessage::Subscribe => "Subscribe",
        ClientMessage::ApplyLayout(_) => "ApplyLayout",
        ClientMessage::Hello { .. } => "Hello",
        ClientMessage::Input { .. } => "Input",
        ClientMessage::Resize { .. } => "Resize",
        ClientMessage::SetCompactView { .. } => "SetCompactView",
        ClientMessage::SplitRight => "SplitRight",
        ClientMessage::SplitDown => "SplitDown",
        ClientMessage::ClosePane => "ClosePane",
        ClientMessage::CloseTab { .. } => "CloseTab",
        ClientMessage::CloseWorkspace { .. } => "CloseWorkspace",
        ClientMessage::FocusPane { .. } => "FocusPane",
        ClientMessage::FocusPaneId { .. } => "FocusPaneId",
        ClientMessage::SendToPane { .. } => "SendToPane",
        ClientMessage::PasteImage { .. } => "PasteImage",
        ClientMessage::PromptAgent { .. } => "PromptAgent",
        ClientMessage::RenamePaneId { .. } => "RenamePaneId",
        ClientMessage::KillSession => "KillSession",
        ClientMessage::RenameSession { .. } => "RenameSession",
        ClientMessage::NewTab => "NewTab",
        ClientMessage::NextTab => "NextTab",
        ClientMessage::PrevTab => "PrevTab",
        ClientMessage::SelectTab { .. } => "SelectTab",
        ClientMessage::SelectTabIndex { .. } => "SelectTabIndex",
        ClientMessage::MoveTab { .. } => "MoveTab",
        ClientMessage::MovePaneToTab { .. } => "MovePaneToTab",
        ClientMessage::SwapPane { .. } => "SwapPane",
        ClientMessage::BreakPane => "BreakPane",
        ClientMessage::EqualizeLayout => "EqualizeLayout",
        ClientMessage::FocusPaneCycle { .. } => "FocusPaneCycle",
        ClientMessage::SelectWorkspaceDelta { .. } => "SelectWorkspaceDelta",
        ClientMessage::RenameTabId { .. } => "RenameTabId",
        ClientMessage::RenameWorkspaceId { .. } => "RenameWorkspaceId",
        ClientMessage::NewWorkspace { .. } => "NewWorkspace",
        ClientMessage::NewPane { .. } => "NewPane",
        ClientMessage::SelectWorkspace { .. } => "SelectWorkspace",
        ClientMessage::RenamePane { .. } => "RenamePane",
        ClientMessage::RenameTab { .. } => "RenameTab",
        ClientMessage::RenameWorkspace { .. } => "RenameWorkspace",
        ClientMessage::ResizePane { .. } => "ResizePane",
        ClientMessage::ScrollPane { .. } => "ScrollPane",
        ClientMessage::ReadPane { .. } => "ReadPane",
        ClientMessage::ZoomPane => "ZoomPane",
        ClientMessage::AgentState { .. } => "AgentState",
        ClientMessage::SetWorkspaceColor { .. } => "SetWorkspaceColor",
        ClientMessage::NewWorktreeWorkspace { .. } => "NewWorktreeWorkspace",
        ClientMessage::RemoveWorktreeWorkspace { .. } => "RemoveWorktreeWorkspace",
        ClientMessage::ReloadManifests => "ReloadManifests",
    }
}

/// Variant name of a server message (see [`client_message_name`]).
pub fn server_message_name(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::ImagePasted { .. } => "ImagePasted",
        ServerMessage::Welcome { .. } => "Welcome",
        ServerMessage::Layout(_) => "Layout",
        ServerMessage::Notification(_) => "Notification",
        ServerMessage::PaneText { .. } => "PaneText",
        ServerMessage::Pane(_) => "Pane",
        ServerMessage::Image { .. } => "Image",
        ServerMessage::Version { .. } => "Version",
        ServerMessage::Event(_) => "Event",
        ServerMessage::Session(_) => "Session",
        ServerMessage::Schema { .. } => "Schema",
        ServerMessage::Manifests(_) => "Manifests",
        ServerMessage::Error { .. } => "Error",
        ServerMessage::Shutdown => "Shutdown",
    }
}

/// The schema reply this build answers `Query(QueryKind::Schema)` with.
pub fn schema_message() -> ServerMessage {
    ServerMessage::Schema {
        version: SCHEMA_VERSION,
        client_messages: CLIENT_MESSAGE_NAMES.iter().map(|s| s.to_string()).collect(),
        server_messages: SERVER_MESSAGE_NAMES.iter().map(|s| s.to_string()).collect(),
    }
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec(message)?;
    encoded.push(b'\n');
    Ok(encoded)
}

pub fn decode<T: DeserializeOwned>(line: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_newline_delimited_json() {
        let message = ServerMessage::Layout(LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(2),
            workspaces: vec![WorkspaceInfo {
                id: WorkspaceId(1),
                name: "main".into(),
                active: true,
                state: AgentStateKind::Idle,
                root: Some(PathBuf::from("/tmp/repo")),
                color: None,
                branch: Some("main".into()),
                parent: None,
                tabs: vec![SidebarTabInfo {
                    id: TabId(2),
                    name: "shell".into(),
                    state: AgentStateKind::Idle,
                    agents: vec![AgentInfo {
                        pane: PaneId(3),
                        name: "zsh".into(),
                        state: AgentStateKind::Idle,
                        state_age_secs: 12,
                    }],
                }],
            }],
            tabs: vec![TabInfo {
                id: TabId(2),
                name: "shell".into(),
                active: true,
                state: AgentStateKind::Idle,
            }],
            tree: LayoutTree::Leaf { pane: PaneId(3) },
            panes: vec![PaneSnapshot {
                id: PaneId(3),
                title: "zsh".into(),
                focused: true,
                scroll_offset: 0,
                screen: Screen {
                    contents: "hi".into(),
                    cursor_row: 0,
                    cursor_col: 2,
                    cursor_visible: true,
                    rows: vec![vec![Run {
                        text: "hi".into(),
                        fg: CellColor::Indexed(2),
                        bg: CellColor::Rgb(1, 2, 3),
                        attrs: ATTR_BOLD | ATTR_UNDERLINE,
                    }]],
                    bracketed_paste: true,
                    mouse_reporting: false,
                    graphics: Vec::new(),
                },
                agent: None,
                agent_generation: 0,
                activity_revision: 0,
                state: AgentStateKind::Idle,
                state_reason: "no agent process".into(),
                state_age_secs: 12,
                cwd: Some(PathBuf::from("/tmp/repo")),
            }],
            zoomed: false,
            restored: false,
        });

        let encoded = encode(&message).expect("message encodes");
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(
            decode::<ServerMessage>(&encoded).expect("message decodes"),
            message
        );
    }

    #[test]
    fn old_hello_without_version_decodes_as_zero() {
        // A pre-#23 client sends `Hello` without the `version` field; it must
        // decode as version 0 so the daemon can report a clean mismatch.
        let hello: ClientMessage =
            decode(br#"{"Hello":{"cols":80,"rows":24}}"#).expect("legacy hello decodes");
        assert_eq!(
            hello,
            ClientMessage::Hello {
                cols: 80,
                rows: 24,
                version: 0,
            }
        );
    }

    #[test]
    fn version_query_and_reply_round_trip() {
        let query = ClientMessage::Query(QueryKind::Version);
        assert_eq!(
            decode::<ClientMessage>(&encode(&query).unwrap()).unwrap(),
            query
        );
        let reply = ServerMessage::Version {
            version: PROTOCOL_VERSION,
        };
        assert_eq!(
            decode::<ServerMessage>(&encode(&reply).unwrap()).unwrap(),
            reply
        );
    }

    /// One value per `ClientMessage` variant. `client_message_name` is
    /// exhaustive, so a new variant breaks the build there; this list plus the
    /// count assertion below keeps the schema list in step with it.
    fn every_client_message() -> Vec<ClientMessage> {
        let pane = PaneId(1);
        let tab = TabId(2);
        let workspace = WorkspaceId(3);
        vec![
            ClientMessage::Query(QueryKind::Layout),
            ClientMessage::Subscribe,
            ClientMessage::ApplyLayout(SessionFile {
                version: SESSION_FILE_VERSION,
                name: "demo".into(),
                active_workspace: 3,
                workspaces: Vec::new(),
            }),
            ClientMessage::Hello {
                cols: 80,
                rows: 24,
                version: PROTOCOL_VERSION,
            },
            ClientMessage::Input { bytes: vec![1] },
            ClientMessage::Resize { cols: 80, rows: 24 },
            ClientMessage::SetCompactView { enabled: true },
            ClientMessage::SplitRight,
            ClientMessage::SplitDown,
            ClientMessage::ClosePane,
            ClientMessage::CloseTab { id: tab },
            ClientMessage::CloseWorkspace { id: workspace },
            ClientMessage::FocusPane {
                direction: Direction::Up,
            },
            ClientMessage::FocusPaneId { id: pane },
            ClientMessage::SendToPane {
                id: pane,
                bytes: vec![1],
            },
            ClientMessage::PasteImage {
                pane,
                data: "test".into(),
            },
            ClientMessage::PromptAgent {
                pane,
                expected_agent: "Codex".into(),
                expected_generation: 1,
                bytes: vec![1],
            },
            ClientMessage::RenamePaneId {
                id: pane,
                name: "a".into(),
            },
            ClientMessage::KillSession,
            ClientMessage::RenameSession { name: "a".into() },
            ClientMessage::NewTab,
            ClientMessage::NextTab,
            ClientMessage::PrevTab,
            ClientMessage::SelectTab { id: tab },
            ClientMessage::SelectTabIndex { index: 1 },
            ClientMessage::MoveTab { delta: 1 },
            ClientMessage::MovePaneToTab { pane, tab },
            ClientMessage::SwapPane {
                direction: Direction::Left,
            },
            ClientMessage::BreakPane,
            ClientMessage::EqualizeLayout,
            ClientMessage::FocusPaneCycle { forward: true },
            ClientMessage::SelectWorkspaceDelta { delta: 1 },
            ClientMessage::RenameTabId {
                id: tab,
                name: "a".into(),
            },
            ClientMessage::RenameWorkspaceId {
                id: workspace,
                name: "a".into(),
            },
            ClientMessage::NewWorkspace {
                name: "a".into(),
                root: None,
            },
            ClientMessage::NewPane {
                workspace: None,
                tab: None,
                split: None,
                command: None,
                name: None,
                context: None,
            },
            ClientMessage::SelectWorkspace { id: workspace },
            ClientMessage::RenamePane { name: "a".into() },
            ClientMessage::RenameTab { name: "a".into() },
            ClientMessage::RenameWorkspace { name: "a".into() },
            ClientMessage::ResizePane {
                direction: Direction::Up,
                cells: 1,
            },
            ClientMessage::ScrollPane { id: pane, delta: 1 },
            ClientMessage::ReadPane {
                id: pane,
                scrollback: true,
                lines: Some(5),
            },
            ClientMessage::ZoomPane,
            ClientMessage::AgentState {
                pane,
                state: AgentStateKind::Idle,
                source: "test".into(),
                native_session: None,
            },
            ClientMessage::SetWorkspaceColor {
                id: workspace,
                color: Some("#e7a33b".into()),
            },
            ClientMessage::NewWorktreeWorkspace {
                repo_root: PathBuf::from("/tmp/repo"),
                branch: "feat-a".into(),
                from: Some("main".into()),
            },
            ClientMessage::RemoveWorktreeWorkspace {
                id: workspace,
                keep: false,
            },
            ClientMessage::ReloadManifests,
        ]
    }

    fn every_server_message() -> Vec<ServerMessage> {
        let notification = Notification {
            pane: PaneId(1),
            workspace: WorkspaceId(3),
            tab: TabId(2),
            agent: "codex".into(),
            state: AgentStateKind::Blocked,
            seq: 1,
        };
        vec![
            ServerMessage::ImagePasted {
                pane: PaneId(1),
                path: "/tmp/image.png".into(),
            },
            ServerMessage::Welcome {
                session: "demo".into(),
                version: PROTOCOL_VERSION,
            },
            ServerMessage::Version {
                version: PROTOCOL_VERSION,
            },
            ServerMessage::Layout(LayoutSnapshot {
                active_workspace: WorkspaceId(3),
                active_tab: TabId(2),
                workspaces: Vec::new(),
                tabs: Vec::new(),
                tree: LayoutTree::Leaf { pane: PaneId(1) },
                panes: Vec::new(),
                zoomed: false,
                restored: false,
            }),
            ServerMessage::Notification(notification.clone()),
            ServerMessage::PaneText {
                id: PaneId(1),
                text: "hi".into(),
                scrollback_lines: 1,
            },
            ServerMessage::Pane(PaneSnapshot {
                id: PaneId(1),
                title: "zsh".into(),
                focused: true,
                scroll_offset: 0,
                screen: Screen::default(),
                agent: None,
                agent_generation: 0,
                activity_revision: 0,
                state: AgentStateKind::Idle,
                state_reason: "no agent process".into(),
                state_age_secs: 0,
                cwd: None,
            }),
            ServerMessage::Image {
                pane: PaneId(1),
                image: ImageData {
                    id: 7,
                    revision: 1,
                    format: 24,
                    width: 1,
                    height: 1,
                    data: "AAAA".into(),
                },
            },
            ServerMessage::Event(Event::Notification(notification)),
            ServerMessage::Session(SessionFile {
                version: SESSION_FILE_VERSION,
                name: "demo".into(),
                active_workspace: 3,
                workspaces: Vec::new(),
            }),
            schema_message(),
            ServerMessage::Manifests(vec![ManifestInfo {
                name: "codex".into(),
                display: "Codex".into(),
                process: vec!["codex".into()],
                title: vec!["Codex".into()],
                rules: 2,
                source: "builtin".into(),
            }]),
            ServerMessage::Error {
                message: "boom".into(),
            },
            ServerMessage::Shutdown,
        ]
    }

    #[test]
    fn schema_lists_every_message_exactly_once() {
        let client = every_client_message();
        assert_eq!(client.len(), CLIENT_MESSAGE_NAMES.len());
        for message in &client {
            let name = client_message_name(message);
            assert!(
                CLIENT_MESSAGE_NAMES.contains(&name),
                "{name} missing from CLIENT_MESSAGE_NAMES"
            );
        }
        let server = every_server_message();
        assert_eq!(server.len(), SERVER_MESSAGE_NAMES.len());
        for message in &server {
            let name = server_message_name(message);
            assert!(
                SERVER_MESSAGE_NAMES.contains(&name),
                "{name} missing from SERVER_MESSAGE_NAMES"
            );
        }
    }

    #[test]
    fn every_message_round_trips_through_json() {
        for message in every_client_message() {
            let encoded = encode(&message).expect("encode");
            assert_eq!(decode::<ClientMessage>(&encoded).expect("decode"), message);
        }
        for message in every_server_message() {
            let encoded = encode(&message).expect("encode");
            assert_eq!(decode::<ServerMessage>(&encoded).expect("decode"), message);
        }
    }
    /// A two-workspace file with a split tab, used by the validation tests.
    fn sample_session_file() -> SessionFile {
        SessionFile {
            version: SESSION_FILE_VERSION,
            name: "demo".into(),
            active_workspace: 1,
            workspaces: vec![WorkspaceFile {
                id: 1,
                name: "one".into(),
                root: None,
                color: None,
                active_tab: 2,
                tabs: vec![TabFile {
                    id: 2,
                    name: "agents".into(),
                    zoomed: false,
                    focused: 3,
                    tree: LayoutTree::Split {
                        axis: SplitAxis::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutTree::Leaf { pane: PaneId(3) }),
                        second: Box::new(LayoutTree::Leaf { pane: PaneId(4) }),
                    },
                    panes: vec![
                        PaneFile {
                            id: 3,
                            title: "codex".into(),
                            cwd: None,
                            command: None,
                            native_session: None,
                        },
                        PaneFile {
                            id: 4,
                            title: "shell".into(),
                            cwd: None,
                            command: None,
                            native_session: None,
                        },
                    ],
                }],
            }],
        }
    }

    #[test]
    fn validate_rejects_duplicate_ids_anywhere_in_the_file() {
        sample_session_file().validate().expect("sample is valid");

        // Two workspaces sharing an id would collapse into one on apply.
        let mut file = sample_session_file();
        let mut second = file.workspaces[0].clone();
        second.tabs[0].id = 20;
        second.tabs[0].panes[0].id = 30;
        second.tabs[0].panes[1].id = 40;
        second.tabs[0].focused = 30;
        second.tabs[0].tree = LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutTree::Leaf { pane: PaneId(30) }),
            second: Box::new(LayoutTree::Leaf { pane: PaneId(40) }),
        };
        file.workspaces.push(second);
        assert!(file.validate().is_err(), "duplicate workspace id accepted");

        // Same for a repeated pane id in two different tabs.
        let mut file = sample_session_file();
        let mut second = file.workspaces[0].clone();
        second.id = 10;
        second.tabs[0].id = 20;
        file.workspaces.push(second);
        let error = file.validate().expect_err("duplicate pane id accepted");
        assert!(error.to_string().contains("duplicate pane id"), "{error}");
    }

    #[test]
    fn old_pane_file_without_native_session_decodes() {
        let value = serde_json::json!({
            "id": 7,
            "title": "codex",
            "cwd": "/tmp",
            "command": ["codex"]
        });
        let pane: PaneFile = serde_json::from_value(value).expect("old pane decodes");
        assert_eq!(pane.id, 7);
        assert!(pane.native_session.is_none());
    }

    #[test]
    fn validate_rejects_panes_outside_the_tree() {
        let mut file = sample_session_file();
        // Drop the second leaf but keep its pane entry: it would be spawned and
        // never drawn.
        file.workspaces[0].tabs[0].tree = LayoutTree::Leaf { pane: PaneId(3) };
        let error = file.validate().expect_err("orphan pane accepted");
        assert!(error.to_string().contains("outside its tree"), "{error}");
    }
}
