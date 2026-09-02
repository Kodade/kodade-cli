//! Shared Ködade CLI socket protocol.
//!
//! Each JSON message is UTF-8 and terminated by one newline. Message payloads
//! that contain byte streams use serde's JSON byte-array representation.

use std::path::PathBuf;

use anyhow::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

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
    },
    /// Set (or clear) a workspace's sidebar swatch color, as a `#rrggbb` hex
    /// string. `None` clears it back to the auto-hashed fallback (#19).
    SetWorkspaceColor {
        id: WorkspaceId,
        color: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryKind {
    Layout,
    /// Cheap version probe: the daemon replies with `ServerMessage::Version` and
    /// nothing else, so `--remote` can check compatibility before attaching (#23).
    Version,
    /// The persisted-layout view of the session (`layout export`).
    Session,
    /// The protocol schema: version plus the message names this daemon knows.
    Schema,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
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
    /// Reply to `Query(QueryKind::Session)` — the persisted layout.
    Session(SessionFile),
    /// Reply to `Query(QueryKind::Schema)`.
    Schema {
        version: u32,
        client_messages: Vec<String>,
        server_messages: Vec<String>,
    },
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
}

/// Persisted-session file version understood by this build (#9).
pub const SESSION_FILE_VERSION: u32 = 1;

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
}

impl SessionFile {
    /// A file is usable only if every tab's tree leaves have a matching pane
    /// entry and there is at least one workspace/tab/pane. Focused / active ids
    /// are tolerated (callers fall back), but a tree that names a missing pane
    /// can't be rebuilt, so it counts as corrupt.
    pub fn validate(&self) -> Result<()> {
        if self.version != SESSION_FILE_VERSION {
            anyhow::bail!("unsupported session file version {}", self.version);
        }
        if self.workspaces.is_empty() {
            anyhow::bail!("session file has no workspaces");
        }
        for workspace in &self.workspaces {
            if workspace.tabs.is_empty() {
                anyhow::bail!("workspace {} has no tabs", workspace.id);
            }
            for tab in &workspace.tabs {
                if tab.panes.is_empty() {
                    anyhow::bail!("tab {} has no panes", tab.id);
                }
                let known: std::collections::HashSet<u64> =
                    tab.panes.iter().map(|pane| pane.id).collect();
                let mut leaves = Vec::new();
                tree_leaves(&tab.tree, &mut leaves);
                if leaves.is_empty() {
                    anyhow::bail!("tab {} has an empty tree", tab.id);
                }
                for leaf in leaves {
                    if !known.contains(&leaf) {
                        anyhow::bail!("tab {} tree references unknown pane {leaf}", tab.id);
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
    "SplitRight",
    "SplitDown",
    "ClosePane",
    "CloseTab",
    "CloseWorkspace",
    "FocusPane",
    "FocusPaneId",
    "SendToPane",
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
];

/// Every `ServerMessage` variant name (see [`CLIENT_MESSAGE_NAMES`]).
pub const SERVER_MESSAGE_NAMES: &[&str] = &[
    "Welcome",
    "Layout",
    "Notification",
    "PaneText",
    "Version",
    "Event",
    "Session",
    "Schema",
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
        ClientMessage::SplitRight => "SplitRight",
        ClientMessage::SplitDown => "SplitDown",
        ClientMessage::ClosePane => "ClosePane",
        ClientMessage::CloseTab { .. } => "CloseTab",
        ClientMessage::CloseWorkspace { .. } => "CloseWorkspace",
        ClientMessage::FocusPane { .. } => "FocusPane",
        ClientMessage::FocusPaneId { .. } => "FocusPaneId",
        ClientMessage::SendToPane { .. } => "SendToPane",
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
    }
}

/// Variant name of a server message (see [`client_message_name`]).
pub fn server_message_name(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::Welcome { .. } => "Welcome",
        ServerMessage::Layout(_) => "Layout",
        ServerMessage::Notification(_) => "Notification",
        ServerMessage::PaneText { .. } => "PaneText",
        ServerMessage::Version { .. } => "Version",
        ServerMessage::Event(_) => "Event",
        ServerMessage::Session(_) => "Session",
        ServerMessage::Schema { .. } => "Schema",
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
                },
                agent: None,
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
            },
            ClientMessage::SetWorkspaceColor {
                id: workspace,
                color: Some("#e7a33b".into()),
            },
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
            ServerMessage::Event(Event::Notification(notification)),
            ServerMessage::Session(SessionFile {
                version: SESSION_FILE_VERSION,
                name: "demo".into(),
                active_workspace: 3,
                workspaces: Vec::new(),
            }),
            schema_message(),
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
}
