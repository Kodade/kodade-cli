//! Remember acknowledged view state and restore it without replaying user input.

use kodade_cli_proto::{ClientMessage, LayoutSnapshot, PaneId, TabId, WorkspaceId};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct View {
    tabs: BTreeMap<u64, TabId>,
    focused: BTreeMap<u64, PaneId>,
    scroll: BTreeMap<u64, usize>,
    active: Option<PaneId>,
}

impl View {
    pub fn observe(&mut self, layout: &LayoutSnapshot) {
        self.tabs.retain(|id, tab| {
            layout
                .workspaces
                .iter()
                .any(|w| w.id.0 == *id && w.tabs.iter().any(|t| t.id == *tab))
        });
        self.focused.retain(|id, pane| {
            layout
                .workspaces
                .iter()
                .flat_map(|w| &w.tabs)
                .any(|t| t.id.0 == *id && t.agents.iter().any(|a| a.pane == *pane))
        });
        let live: std::collections::HashSet<_> = layout
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.agents)
            .map(|a| a.pane.0)
            .collect();
        self.scroll.retain(|id, _| live.contains(id));
        self.tabs
            .insert(layout.active_workspace.0, layout.active_tab);
        for pane in &layout.panes {
            self.scroll.insert(pane.id.0, pane.scroll_offset);
            if pane.focused {
                self.active = Some(pane.id);
                self.focused.insert(layout.active_tab.0, pane.id);
            }
        }
    }

    pub fn restore(&self) -> Vec<ClientMessage> {
        let mut commands = Vec::new();
        for &id in self.focused.values() {
            commands.push(ClientMessage::FocusPaneId { id });
        }
        for (&workspace, &id) in &self.tabs {
            commands.push(ClientMessage::SelectWorkspace {
                id: WorkspaceId(workspace),
            });
            commands.push(ClientMessage::SelectTab { id });
        }
        for (&id, &offset) in &self.scroll {
            if offset > 0 {
                commands.push(ClientMessage::ScrollPane {
                    id: PaneId(id),
                    delta: offset.min(i16::MAX as usize) as i16,
                });
            }
        }
        if let Some(id) = self.active {
            commands.push(ClientMessage::FocusPaneId { id });
        }
        commands
    }
}
