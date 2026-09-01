//! Shared Ködade CLI socket protocol.
//!
//! Each JSON message is UTF-8 and terminated by one newline. Message payloads
//! that contain byte streams use serde's JSON byte-array representation.

use anyhow::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    Query(QueryKind),
    Hello {
        cols: u16,
        rows: u16,
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
    NewWorkspace {
        name: String,
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
    ZoomPane,
    AgentState {
        pane: PaneId,
        state: AgentStateKind,
        source: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryKind {
    Layout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome { session: String },
    Layout(LayoutSnapshot),
    Error { message: String },
    Shutdown,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Screen {
    pub contents: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
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
                tabs: vec![SidebarTabInfo {
                    id: TabId(2),
                    name: "shell".into(),
                    state: AgentStateKind::Idle,
                    agents: vec![AgentInfo {
                        pane: PaneId(3),
                        name: "zsh".into(),
                        state: AgentStateKind::Idle,
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
                screen: Screen::default(),
                agent: None,
                state: AgentStateKind::Idle,
                state_reason: "no agent process".into(),
            }],
            zoomed: false,
        });

        let encoded = encode(&message).expect("message encodes");
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(
            decode::<ServerMessage>(&encoded).expect("message decodes"),
            message
        );
    }
}
