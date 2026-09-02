//! Client-side agent notifications (#10).
//!
//! `Notifier` turns each `ServerMessage::Notification` from the daemon into a
//! list of `Effect`s (toast, bell, system OSC, sound) and keeps an unread stack
//! that `prefix N` walks back through. Effect computation is pure so it can be
//! unit-tested; `app.rs` performs the side effects (drawing, writing escapes,
//! spawning the sound command).

use kodade_cli_proto::{AgentStateKind, LayoutSnapshot, Notification};

use crate::config::{self, NotifyToast};

/// One thing to do in response to a notification. `app.rs` interprets these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Show this text in the status bar for a few seconds.
    Toast(String),
    /// Ring the terminal bell (`\x07`).
    Bell,
    /// Ask the host terminal to raise a desktop notification (OSC 777 + OSC 9).
    Osc777 { title: String, body: String },
    /// Run this command via `sh -c`, detached.
    Sound(String),
}

/// Holds the notification config and the unread stack (most recent last).
pub struct Notifier {
    enabled: bool,
    on: Vec<AgentStateKind>,
    toast: NotifyToast,
    bell: bool,
    sound: String,
    only_when_unfocused: bool,
    /// Chord label shown in the toast, e.g. `N` (see #14 collision note).
    jump_hint: String,
    unread: Vec<Notification>,
}

impl Notifier {
    pub fn new(config: &config::Config, jump_hint: String) -> Self {
        Self {
            enabled: config.notify,
            on: config.notify_on.clone(),
            toast: config.notify_toast,
            bell: config.notify_bell,
            sound: config.notify_sound.clone(),
            only_when_unfocused: config.notify_only_when_unfocused,
            jump_hint,
            unread: Vec::new(),
        }
    }

    /// Computes the effects for one notification and records it as unread.
    /// Returns an empty list (and records nothing) when the notification is
    /// filtered out by config or the pane is already on screen and focused.
    pub fn handle(&mut self, notification: &Notification, layout: &LayoutSnapshot) -> Vec<Effect> {
        if !self.enabled || !self.on.contains(&notification.state) {
            return Vec::new();
        }
        if self.only_when_unfocused && pane_is_on_screen(layout, notification) {
            return Vec::new();
        }
        // Keep a single entry per pane, freshest on top of the stack.
        self.unread.retain(|item| item.pane != notification.pane);
        self.unread.push(notification.clone());

        let text = toast_text(notification, layout, &self.jump_hint);
        let mut effects = Vec::new();
        match self.toast {
            NotifyToast::Status => effects.push(Effect::Toast(text)),
            NotifyToast::System => effects.push(Effect::Osc777 {
                title: "Ködade".to_string(),
                body: text,
            }),
            NotifyToast::Off => {}
        }
        if self.bell {
            effects.push(Effect::Bell);
        }
        if !self.sound.is_empty() {
            effects.push(Effect::Sound(self.sound.clone()));
        }
        effects
    }

    /// Pops the most recent unread notification for `prefix N`, marking it read.
    pub fn pop_unread(&mut self) -> Option<Notification> {
        self.unread.pop()
    }
}

/// True when the notification's pane is the focused pane of the active tab and
/// workspace in this snapshot — i.e. the user is already looking at it.
fn pane_is_on_screen(layout: &LayoutSnapshot, notification: &Notification) -> bool {
    layout.active_workspace == notification.workspace
        && layout.active_tab == notification.tab
        && layout
            .panes
            .iter()
            .any(|pane| pane.id == notification.pane && pane.focused)
}

/// `● codex blocked in kodade-cli/agents · prefix N to jump`.
fn toast_text(notification: &Notification, layout: &LayoutSnapshot, jump_hint: &str) -> String {
    let (workspace, tab) = names(layout, notification);
    format!(
        "● {} {} in {}/{} · prefix {} to jump",
        notification.agent,
        state_name(notification.state),
        workspace,
        tab,
        jump_hint,
    )
}

/// Resolves the workspace and tab display names from the snapshot, falling back
/// to numeric ids when the notification outran a layout that no longer has them.
fn names(layout: &LayoutSnapshot, notification: &Notification) -> (String, String) {
    let workspace = layout
        .workspaces
        .iter()
        .find(|workspace| workspace.id == notification.workspace);
    let workspace_name = workspace
        .map(|workspace| workspace.name.clone())
        .unwrap_or_else(|| format!("ws{}", notification.workspace.0));
    let tab_name = workspace
        .and_then(|workspace| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.id == notification.tab)
                .map(|tab| tab.name.clone())
        })
        .unwrap_or_else(|| format!("tab{}", notification.tab.0));
    (workspace_name, tab_name)
}

fn state_name(state: AgentStateKind) -> &'static str {
    match state {
        AgentStateKind::Blocked => "blocked",
        AgentStateKind::Working => "working",
        AgentStateKind::Done => "done",
        AgentStateKind::Idle => "idle",
        AgentStateKind::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{
        AgentInfo, LayoutTree, PaneId, PaneSnapshot, SidebarTabInfo, TabId, TabInfo, WorkspaceId,
        WorkspaceInfo,
    };

    fn layout(focused_pane: PaneId) -> LayoutSnapshot {
        LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(2),
            workspaces: vec![WorkspaceInfo {
                id: WorkspaceId(1),
                name: "kodade-cli".into(),
                active: true,
                state: AgentStateKind::Blocked,
                root: None,
                color: None,
                tabs: vec![SidebarTabInfo {
                    id: TabId(2),
                    name: "agents".into(),
                    state: AgentStateKind::Blocked,
                    agents: vec![AgentInfo {
                        pane: PaneId(3),
                        name: "codex".into(),
                        state: AgentStateKind::Blocked,
                        state_age_secs: 0,
                    }],
                }],
            }],
            tabs: vec![TabInfo {
                id: TabId(2),
                name: "agents".into(),
                active: true,
                state: AgentStateKind::Blocked,
            }],
            tree: LayoutTree::Leaf { pane: PaneId(3) },
            panes: vec![PaneSnapshot {
                id: PaneId(3),
                title: "codex".into(),
                focused: PaneId(3) == focused_pane,
                scroll_offset: 0,
                screen: Default::default(),
                agent: Some("codex".into()),
                state: AgentStateKind::Blocked,
                state_reason: String::new(),
                state_age_secs: 0,
                cwd: None,
            }],
            zoomed: false,
            restored: false,
        }
    }

    fn notification(state: AgentStateKind) -> Notification {
        Notification {
            pane: PaneId(3),
            workspace: WorkspaceId(1),
            tab: TabId(2),
            agent: "codex".into(),
            state,
            seq: 1,
        }
    }

    fn notifier() -> Notifier {
        Notifier {
            enabled: true,
            on: vec![AgentStateKind::Blocked, AgentStateKind::Done],
            toast: NotifyToast::Status,
            bell: true,
            sound: String::new(),
            only_when_unfocused: true,
            jump_hint: "N".into(),
            unread: Vec::new(),
        }
    }

    #[test]
    fn status_toast_and_bell_when_unfocused() {
        let mut notifier = notifier();
        // Focused pane is a different one, so the target pane is off screen.
        let effects = notifier.handle(&notification(AgentStateKind::Blocked), &layout(PaneId(9)));
        assert_eq!(
            effects,
            vec![
                Effect::Toast(
                    "● codex blocked in kodade-cli/agents · prefix N to jump".to_string()
                ),
                Effect::Bell,
            ]
        );
        assert!(notifier.pop_unread().is_some());
    }

    #[test]
    fn only_when_unfocused_skips_the_focused_pane() {
        let mut notifier = notifier();
        let effects = notifier.handle(&notification(AgentStateKind::Blocked), &layout(PaneId(3)));
        assert!(effects.is_empty());
        assert!(notifier.pop_unread().is_none());
    }

    #[test]
    fn system_toast_emits_osc_and_sound() {
        let mut notifier = notifier();
        notifier.toast = NotifyToast::System;
        notifier.sound = "afplay ding.aiff".into();
        let effects = notifier.handle(&notification(AgentStateKind::Done), &layout(PaneId(9)));
        assert_eq!(
            effects,
            vec![
                Effect::Osc777 {
                    title: "Ködade".to_string(),
                    body: "● codex done in kodade-cli/agents · prefix N to jump".to_string(),
                },
                Effect::Bell,
                Effect::Sound("afplay ding.aiff".to_string()),
            ]
        );
    }

    #[test]
    fn state_not_in_on_is_ignored() {
        let mut notifier = notifier();
        notifier.on = vec![AgentStateKind::Done];
        assert!(notifier
            .handle(&notification(AgentStateKind::Blocked), &layout(PaneId(9)))
            .is_empty());
    }

    #[test]
    fn disabled_notifier_does_nothing() {
        let mut notifier = notifier();
        notifier.enabled = false;
        assert!(notifier
            .handle(&notification(AgentStateKind::Blocked), &layout(PaneId(9)))
            .is_empty());
    }

    #[test]
    fn repeated_pane_keeps_one_unread_entry() {
        let mut notifier = notifier();
        let _ = notifier.handle(&notification(AgentStateKind::Blocked), &layout(PaneId(9)));
        let _ = notifier.handle(&notification(AgentStateKind::Done), &layout(PaneId(9)));
        assert_eq!(
            notifier.pop_unread().map(|n| n.state),
            Some(AgentStateKind::Done)
        );
        assert!(notifier.pop_unread().is_none());
    }
}
