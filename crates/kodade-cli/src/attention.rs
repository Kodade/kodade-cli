//! Client-side attention inbox built from daemon notifications.

use crate::overlay::{Overlay, OverlayRow, OverlayTarget};
use kodade_cli_proto::{AgentStateKind, LayoutSnapshot, Notification, PaneId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    notifications: Vec<Notification>,
    pub overlay: Overlay,
}

impl Attention {
    pub fn new(notifications: &[Notification], layout: &LayoutSnapshot) -> Self {
        let mut notifications = notifications.to_vec();
        notifications.sort_by_key(|notice| {
            (
                notice.state != AgentStateKind::Blocked,
                std::cmp::Reverse(notice.seq),
            )
        });
        let rows = if notifications.is_empty() {
            vec![OverlayRow::new(
                " no unread agent alerts",
                "agents are clear",
                OverlayTarget::None,
            )]
        } else {
            notifications
                .iter()
                .map(|notice| row(notice, layout))
                .collect()
        };
        Self {
            notifications,
            overlay: Overlay::new(
                "attention · enter jumps · a acknowledges · esc closes",
                rows,
            ),
        }
    }

    pub fn current_pane(&self) -> Option<PaneId> {
        self.notifications
            .get(self.overlay.selected)
            .map(|notice| notice.pane)
    }

    /// Rebuild rows from the live snapshot without stealing the user's place
    /// in an inbox that may update many times per second.
    pub fn refresh(&mut self, notifications: &[Notification], layout: &LayoutSnapshot) {
        let selected = self.current_pane();
        *self = Self::new(notifications, layout);
        if let Some(pane) = selected {
            if let Some(index) = self
                .notifications
                .iter()
                .position(|notification| notification.pane == pane)
            {
                self.overlay.selected = index;
            }
        }
    }
}

fn row(notice: &Notification, layout: &LayoutSnapshot) -> OverlayRow {
    let workspace = layout
        .workspaces
        .iter()
        .find(|workspace| workspace.id == notice.workspace);
    let workspace_name = workspace
        .map(|w| w.name.as_str())
        .unwrap_or("closed workspace");
    let tab_name = workspace
        .and_then(|w| w.tabs.iter().find(|tab| tab.id == notice.tab))
        .map(|tab| tab.name.as_str())
        .unwrap_or("closed tab");
    let pane = layout.panes.iter().find(|pane| pane.id == notice.pane);
    let agent = workspace
        .and_then(|workspace| workspace.tabs.iter().find(|tab| tab.id == notice.tab))
        .and_then(|tab| tab.agents.iter().find(|agent| agent.pane == notice.pane));
    let reason = pane
        .map(|pane| pane.state_reason.as_str())
        .filter(|reason| !reason.is_empty())
        .map(str::to_string);
    let age = pane
        .map(|pane| pane.state_age_secs)
        .or_else(|| agent.map(|agent| agent.state_age_secs));
    let mut hint = age.map(age_text).unwrap_or_default();
    if let Some(reason) = reason {
        if !hint.is_empty() {
            hint.push_str(" · ");
        }
        hint.push_str(&reason);
    }
    OverlayRow::new(
        format!(
            "{} · {} · {}/{}",
            state_name(notice.state),
            notice.agent,
            workspace_name,
            tab_name
        ),
        hint,
        OverlayTarget::Pane(notice.pane),
    )
}

fn state_name(state: AgentStateKind) -> &'static str {
    match state {
        AgentStateKind::Blocked => "● blocked",
        AgentStateKind::Done => "✓ done",
        AgentStateKind::Working => "working",
        AgentStateKind::Idle => "idle",
        AgentStateKind::Unknown => "unknown",
    }
}

fn age_text(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{TabId, WorkspaceId};

    #[test]
    fn attention_sorts_blocked_before_completed_work() {
        let blocked = Notification {
            pane: PaneId(1),
            workspace: WorkspaceId(1),
            tab: TabId(1),
            agent: "Codex".into(),
            state: AgentStateKind::Blocked,
            seq: 1,
        };
        let done = Notification {
            pane: PaneId(2),
            workspace: WorkspaceId(1),
            tab: TabId(1),
            agent: "Claude".into(),
            state: AgentStateKind::Done,
            seq: 2,
        };
        let layout = LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(1),
            workspaces: vec![],
            tabs: vec![],
            panes: vec![],
            tree: kodade_cli_proto::LayoutTree::Leaf { pane: PaneId(1) },
            zoomed: false,
            restored: false,
        };
        let attention = Attention::new(&[done, blocked], &layout);
        assert!(attention.overlay.rows[0].label.contains("blocked"));
        assert_eq!(attention.current_pane(), Some(PaneId(1)));
    }

    #[test]
    fn attention_has_a_clear_empty_state() {
        let layout = LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(1),
            workspaces: vec![],
            tabs: vec![],
            panes: vec![],
            tree: kodade_cli_proto::LayoutTree::Leaf { pane: PaneId(1) },
            zoomed: false,
            restored: false,
        };
        let attention = Attention::new(&[], &layout);
        assert_eq!(
            attention.overlay.rows[0].label.trim(),
            "no unread agent alerts"
        );
        assert_eq!(attention.current_pane(), None);
    }

    #[test]
    fn refresh_keeps_the_selected_pane_when_rows_update() {
        let notifications = [
            Notification {
                pane: PaneId(1),
                workspace: WorkspaceId(1),
                tab: TabId(1),
                agent: "Codex".into(),
                state: AgentStateKind::Blocked,
                seq: 2,
            },
            Notification {
                pane: PaneId(2),
                workspace: WorkspaceId(1),
                tab: TabId(1),
                agent: "Claude".into(),
                state: AgentStateKind::Blocked,
                seq: 1,
            },
        ];
        let layout = LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(1),
            workspaces: vec![],
            tabs: vec![],
            panes: vec![],
            tree: kodade_cli_proto::LayoutTree::Leaf { pane: PaneId(1) },
            zoomed: false,
            restored: false,
        };
        let mut attention = Attention::new(&notifications, &layout);
        attention.overlay.selected = 1;
        assert_eq!(attention.current_pane(), Some(PaneId(2)));

        attention.refresh(&notifications, &layout);
        assert_eq!(attention.current_pane(), Some(PaneId(2)));
    }
}
