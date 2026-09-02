//! Fuzzy pickers for the workspace switcher (`prefix w`) and the goto palette
//! (`prefix g`).
//!
//! Everything here is pure so it can be unit-tested without a terminal. The
//! picker builds a plain [`Overlay`] with a filter line and reuses the shared
//! overlay key handling; only the row styling (a state-colored dot plus a dim
//! detail) is drawn through `overlay::render_picker`.

use kodade_cli_proto::{AgentStateKind, LayoutSnapshot, PaneId, TabId, WorkspaceId};

use crate::overlay::{Overlay, OverlayRow, OverlayTarget};

/// What activating a picker row does. `Tab` carries only the tab id; the app
/// resolves and selects its workspace first (a tab can live in an inactive one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickTarget {
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

impl PickTarget {
    // The overlay's own target enum, so mouse clicks and `enter` share a path.
    fn overlay_target(&self) -> OverlayTarget {
        match self {
            PickTarget::Workspace(id) => OverlayTarget::Workspace(*id),
            PickTarget::Tab(id) => OverlayTarget::Tab(*id),
            PickTarget::Pane(id) => OverlayTarget::Pane(*id),
        }
    }
}

/// One searchable entry: a display `label`, a dim `detail`, the `state` that
/// colors its dot, and the `target` it activates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub state: AgentStateKind,
    pub target: PickTarget,
}

/// Every workspace as a picker row: name, its rollup state, a `N tabs` count,
/// and the root directory basename when it differs from the name.
pub fn workspace_items(layout: &LayoutSnapshot) -> Vec<PickerItem> {
    layout
        .workspaces
        .iter()
        .map(|workspace| {
            let tabs = workspace.tabs.len();
            let mut detail = format!("{tabs} {}", if tabs == 1 { "tab" } else { "tabs" });
            if let Some(base) = root_basename(workspace) {
                detail = format!("{detail} · {base}");
            }
            PickerItem {
                label: workspace.name.clone(),
                detail,
                state: workspace.state,
                target: PickTarget::Workspace(workspace.id),
            }
        })
        .collect()
}

/// Every workspace, tab, and agent pane as a flat list. Tabs and panes carry a
/// `workspace › tab › Agent` breadcrumb so a search hits the deepest match.
pub fn goto_items(layout: &LayoutSnapshot) -> Vec<PickerItem> {
    let mut items = Vec::new();
    for workspace in &layout.workspaces {
        items.push(PickerItem {
            label: workspace.name.clone(),
            detail: "workspace".into(),
            state: workspace.state,
            target: PickTarget::Workspace(workspace.id),
        });
        for tab in &workspace.tabs {
            let path = format!("{} › {}", workspace.name, tab.name);
            items.push(PickerItem {
                label: path.clone(),
                detail: "tab".into(),
                state: tab.state,
                target: PickTarget::Tab(tab.id),
            });
            for agent in &tab.agents {
                items.push(PickerItem {
                    label: format!("{path} › {}", agent.name),
                    detail: state_name(agent.state).into(),
                    state: agent.state,
                    target: PickTarget::Pane(agent.pane),
                });
            }
        }
    }
    items
}

// The workspace root's basename, when set and different from the name.
fn root_basename(workspace: &kodade_cli_proto::WorkspaceInfo) -> Option<String> {
    workspace
        .root
        .as_deref()
        .and_then(std::path::Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|base| *base != workspace.name)
        .map(str::to_string)
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

/// Case-insensitive subsequence score, or `None` when `query` is not a
/// subsequence of `text`. An empty query matches everything with score 0.
/// Every matched char scores 1, a char at a word start scores +3, and a char
/// contiguous with the previous match scores +2 — so tighter, word-aligned
/// matches sort ahead of scattered ones.
pub fn fuzzy_score(query: &str, text: &str) -> Option<u32> {
    let needle: Vec<char> = query.to_lowercase().chars().collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0u32;
    let mut next = 0usize;
    let mut prev_matched = false;
    for (index, &ch) in hay.iter().enumerate() {
        if next < needle.len() && ch == needle[next] {
            score += 1;
            let word_start =
                index == 0 || matches!(hay[index - 1], ' ' | '›' | '·' | '/' | '-' | '_' | '.');
            if word_start {
                score += 3;
            }
            if prev_matched {
                score += 2;
            }
            prev_matched = true;
            next += 1;
        } else {
            prev_matched = false;
        }
    }
    (next == needle.len()).then_some(score)
}

/// Items matching `query`, sorted by (blocked first, score desc, original
/// order). An empty query keeps every item in original order, blocked first.
pub fn filter(items: &[PickerItem], query: &str) -> Vec<PickerItem> {
    let mut scored: Vec<(usize, u32, &PickerItem)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| fuzzy_score(query, &item.label).map(|s| (index, s, item)))
        .collect();
    scored.sort_by(|(ia, sa, a), (ib, sb, b)| {
        let a_blocked = a.state == AgentStateKind::Blocked;
        let b_blocked = b.state == AgentStateKind::Blocked;
        b_blocked.cmp(&a_blocked).then(sb.cmp(sa)).then(ia.cmp(ib))
    });
    scored
        .into_iter()
        .map(|(_, _, item)| item.clone())
        .collect()
}

/// A picker overlay plus the item list behind it, so filtering can rebuild the
/// visible rows and map a selection back to its target and state.
pub struct Picker {
    all: Vec<PickerItem>,
    /// Items behind the current rows, aligned with `overlay.rows`.
    visible: Vec<PickerItem>,
    pub overlay: Overlay,
}

impl Picker {
    /// Builds a picker titled `title` over `items`, filter box open and empty.
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        let visible = filter(&items, "");
        let rows = visible.iter().map(row_of).collect();
        let mut overlay = Overlay::new(title, rows);
        overlay.filter = Some(String::new());
        Self {
            all: items,
            visible,
            overlay,
        }
    }

    /// Rebuilds the visible rows from the filter text, keeping the selection in
    /// range.
    pub fn apply_filter(&mut self) {
        let query = self.overlay.filter.clone().unwrap_or_default();
        self.visible = filter(&self.all, &query);
        self.overlay.rows = self.visible.iter().map(row_of).collect();
        let last = self.overlay.rows.len().saturating_sub(1);
        if self.overlay.selected > last {
            self.overlay.selected = last;
        }
    }

    /// The target behind the highlighted row, if any.
    pub fn current_target(&self) -> Option<PickTarget> {
        self.visible
            .get(self.overlay.selected)
            .map(|item| item.target.clone())
    }

    /// State of the row at `index`, for coloring its dot at render time.
    pub fn state_at(&self, index: usize) -> AgentStateKind {
        self.visible
            .get(index)
            .map(|item| item.state)
            .unwrap_or(AgentStateKind::Unknown)
    }
}

// Turns an item into an overlay row: name in the label, detail in the hint.
fn row_of(item: &PickerItem) -> OverlayRow {
    OverlayRow::new(
        item.label.clone(),
        item.detail.clone(),
        item.target.overlay_target(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{AgentInfo, LayoutTree, SidebarTabInfo, TabInfo, WorkspaceInfo};

    fn agent(name: &str, pane: u64, state: AgentStateKind) -> AgentInfo {
        AgentInfo {
            pane: PaneId(pane),
            name: name.into(),
            state,
            state_age_secs: 0,
        }
    }

    fn snapshot() -> LayoutSnapshot {
        let workspaces = vec![
            WorkspaceInfo {
                id: WorkspaceId(1),
                name: "alpha".into(),
                active: true,
                state: AgentStateKind::Idle,
                root: Some("/home/keith/repos/alpha-src".into()),
                color: None,
                branch: None,
                parent: None,
                tabs: vec![SidebarTabInfo {
                    id: TabId(1),
                    name: "edit".into(),
                    state: AgentStateKind::Working,
                    agents: vec![
                        agent("Claude", 10, AgentStateKind::Working),
                        agent("Codex", 11, AgentStateKind::Idle),
                    ],
                }],
            },
            WorkspaceInfo {
                id: WorkspaceId(2),
                name: "beta".into(),
                active: false,
                state: AgentStateKind::Blocked,
                root: None,
                color: None,
                branch: None,
                parent: None,
                tabs: vec![
                    SidebarTabInfo {
                        id: TabId(2),
                        name: "run".into(),
                        state: AgentStateKind::Blocked,
                        agents: vec![agent("Grok", 20, AgentStateKind::Blocked)],
                    },
                    SidebarTabInfo {
                        id: TabId(3),
                        name: "logs".into(),
                        state: AgentStateKind::Idle,
                        agents: vec![],
                    },
                ],
            },
        ];
        LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(1),
            workspaces,
            tabs: vec![TabInfo {
                id: TabId(1),
                name: "edit".into(),
                active: true,
                state: AgentStateKind::Working,
            }],
            tree: LayoutTree::Leaf { pane: PaneId(10) },
            panes: vec![],
            zoomed: false,
            restored: false,
        }
    }

    #[test]
    fn workspace_items_carry_counts_and_root_basename() {
        let items = workspace_items(&snapshot());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "alpha");
        assert_eq!(items[0].detail, "1 tab · alpha-src");
        // beta has no root and two tabs.
        assert_eq!(items[1].detail, "2 tabs");
        assert_eq!(items[1].state, AgentStateKind::Blocked);
    }

    #[test]
    fn goto_items_flatten_every_level_with_breadcrumbs() {
        let items = goto_items(&snapshot());
        // 2 workspaces + 3 tabs + 3 agents.
        assert_eq!(items.len(), 8);
        assert!(items.iter().any(|item| item.label == "alpha › edit › Codex"
            && item.target == PickTarget::Pane(PaneId(11))));
        assert!(items
            .iter()
            .any(|item| item.label == "beta › run" && item.target == PickTarget::Tab(TabId(2))));
    }

    #[test]
    fn cod_lands_the_codex_pane_first() {
        let items = goto_items(&snapshot());
        let ranked = filter(&items, "cod");
        assert_eq!(ranked[0].target, PickTarget::Pane(PaneId(11)));
        assert!(ranked[0].label.contains("Codex"));
    }

    #[test]
    fn blocked_entries_sort_to_the_top() {
        let items = goto_items(&snapshot());
        // Empty query keeps original order but floats blocked entries up.
        let ranked = filter(&items, "");
        assert_eq!(ranked.len(), items.len());
        assert_eq!(ranked[0].state, AgentStateKind::Blocked);
        // The two blocked rows (beta workspace + run tab + Grok pane) lead.
        let blocked = ranked
            .iter()
            .take_while(|item| item.state == AgentStateKind::Blocked)
            .count();
        assert_eq!(blocked, 3);
    }

    #[test]
    fn empty_query_keeps_original_order_within_a_group() {
        let items = workspace_items(&snapshot());
        let ranked = filter(&items, "");
        // beta is blocked so it leads; alpha (idle) follows, order preserved.
        assert_eq!(ranked[0].label, "beta");
        assert_eq!(ranked[1].label, "alpha");
    }

    #[test]
    fn fuzzy_score_rewards_word_starts_and_runs() {
        // A contiguous, word-start match beats a scattered one.
        let tight = fuzzy_score("cl", "Claude").unwrap();
        let loose = fuzzy_score("cl", "canonical").unwrap();
        assert!(tight > loose);
        // Non-subsequence returns None.
        assert!(fuzzy_score("zzz", "Claude").is_none());
        // Empty query always matches.
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn picker_filter_rebuilds_rows_and_maps_selection() {
        let mut picker = Picker::new("goto", goto_items(&snapshot()));
        picker.overlay.filter = Some("cod".into());
        picker.apply_filter();
        assert!(!picker.overlay.rows.is_empty());
        picker.overlay.selected = 0;
        assert_eq!(picker.current_target(), Some(PickTarget::Pane(PaneId(11))));
    }
}
