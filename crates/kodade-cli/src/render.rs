use std::collections::HashMap;

use kodade_cli_proto::{AgentStateKind, LayoutSnapshot, LayoutTree, PaneId, TabId, WorkspaceId};
use ratatui::{
    layout::{Constraint, Direction as LayoutDirection, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    config::Theme,
    input::{tab_spans, TabSpan},
    mode::{CopyMode, Menu, MenuAction},
};

pub const TAB_PREFIX: &str = " Ködade · ";
pub const SIDEBAR_WIDTH: u16 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarTarget {
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRow {
    pub target: SidebarTarget,
    pub label: String,
    pub state: AgentStateKind,
}

/// Per-frame UI state that is not part of the daemon layout snapshot.
pub struct Ui<'a> {
    pub sidebar: bool,
    pub prefix: bool,
    pub rename: bool,
    pub name: &'a str,
    pub navigate: Option<usize>,
    pub copy: Option<&'a CopyMode>,
    pub menu: Option<&'a Menu>,
    pub note: Option<&'a str>,
}

pub fn render(frame: &mut Frame, layout: &LayoutSnapshot, ui: &Ui, theme: &Theme) {
    let Ui {
        sidebar,
        prefix,
        rename,
        name,
        navigate,
        copy,
        menu,
        note,
    } = *ui;
    let areas = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width(sidebar)),
            Constraint::Min(1),
        ])
        .split(frame.area());
    if sidebar {
        render_sidebar(frame, layout, areas[0], navigate, theme);
    } else {
        frame.render_widget(
            Paragraph::new("▸").style(Style::default().fg(theme.dim)),
            areas[0],
        );
    }
    let areas = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(areas[1]);
    let workspace = layout
        .workspaces
        .iter()
        .find(|item| item.active)
        .map(|item| item.name.as_str())
        .unwrap_or("workspace");
    let tabs = layout
        .tabs
        .iter()
        .map(|tab| {
            let prefix = state_dot(tab.state);
            if tab.active {
                format!("{prefix}[{}]", tab.name)
            } else {
                format!(" {prefix}{} ", tab.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    frame.render_widget(
        Paragraph::new(format!("{TAB_PREFIX}{workspace}  {tabs}"))
            .style(Style::default().fg(theme.text).bg(theme.tabbar_bg)),
        areas[0],
    );

    let mut rects = HashMap::new();
    rects_for(&layout.tree, areas[1], &mut rects);
    for pane in &layout.panes {
        if let Some(rect) = rects.get(&pane.id) {
            let title = if pane.scroll_offset > 0 {
                format!("{} [scroll]", pane_title(pane))
            } else {
                pane_title(pane)
            };
            let contents = copy
                .filter(|copy| copy.pane == pane.id)
                .map(|copy| copy.screen.contents.as_str())
                .unwrap_or(&pane.screen.contents);
            frame.render_widget(
                Paragraph::new(contents).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(border_color(
                            theme,
                            pane.state,
                            pane.focused,
                        ))),
                ),
                *rect,
            );
        }
    }
    let status = if rename {
        format!(" rename pane: {name}")
    } else if copy.is_some() {
        " copy mode · v select · y copy · esc exit".into()
    } else if navigate.is_some() {
        " navigate · j/k move · enter activate · esc exit".into()
    } else if prefix {
        " prefix: % \" b hjkl c n p w W x z d r".into()
    } else {
        format!(" session · {workspace}")
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(theme.dim).bg(theme.status_bg)),
        areas[2],
    );
    if let Some(note) = note {
        frame.render_widget(
            Paragraph::new(note).style(Style::default().fg(theme.done).bg(theme.status_bg)),
            areas[2],
        );
    }
    if let Some(menu) = menu {
        render_menu(frame, menu, frame.area(), theme);
    }
}

pub fn sidebar_width(visible: bool) -> u16 {
    if visible {
        SIDEBAR_WIDTH
    } else {
        1
    }
}

pub fn content_area(area: Rect, sidebar: bool) -> Rect {
    Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width(sidebar)),
            Constraint::Min(1),
        ])
        .split(area)[1]
}

pub fn sidebar_rows(layout: &LayoutSnapshot) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    for workspace in &layout.workspaces {
        rows.push(SidebarRow {
            target: SidebarTarget::Workspace(workspace.id),
            label: format!(
                "{} {}",
                if workspace.active { "▾" } else { "▸" },
                workspace.name
            ),
            state: workspace.state,
        });
        if workspace.active {
            for tab in &workspace.tabs {
                rows.push(SidebarRow {
                    target: SidebarTarget::Tab(tab.id),
                    label: format!("  {}", tab.name),
                    state: tab.state,
                });
                rows.extend(tab.agents.iter().map(|agent| SidebarRow {
                    target: SidebarTarget::Pane(agent.pane),
                    label: format!("    {}", agent.name),
                    state: agent.state,
                }));
            }
        }
    }
    rows
}

pub fn sidebar_row_at(rows: &[SidebarRow], row: u16) -> Option<&SidebarRow> {
    rows.get(row as usize)
}

fn render_sidebar(
    frame: &mut Frame,
    layout: &LayoutSnapshot,
    area: Rect,
    navigate: Option<usize>,
    theme: &Theme,
) {
    for (index, row) in sidebar_rows(layout).iter().enumerate() {
        let y = area.y.saturating_add(index as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        let text = truncate(
            &format!("{} {}", row.label, sidebar_dot(row.state)),
            area.width,
        );
        let style = if navigate == Some(index) {
            Style::default().fg(theme.tabbar_bg).bg(theme.accent)
        } else {
            Style::default().fg(state_color(theme, row.state))
        };
        frame.render_widget(
            Paragraph::new(text).style(style),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

fn render_menu(frame: &mut Frame, menu: &Menu, area: Rect, theme: &Theme) {
    let labels = menu
        .actions()
        .iter()
        .map(|action| match action {
            MenuAction::SplitRight => "Split right",
            MenuAction::SplitDown => "Split down",
            MenuAction::Rename => "Rename",
            MenuAction::Zoom => "Zoom",
            MenuAction::Close => "Close",
        })
        .collect::<Vec<_>>();
    let width = 14.min(area.width.saturating_sub(menu.x));
    for (index, label) in labels.iter().enumerate() {
        let y = menu.y.saturating_add(index as u16);
        if y >= area.height {
            break;
        }
        let style = if index == menu.selected {
            Style::default().fg(theme.tabbar_bg).bg(theme.accent)
        } else {
            Style::default().fg(theme.text).bg(theme.status_bg)
        };
        frame.render_widget(
            Paragraph::new(*label).style(style),
            Rect::new(menu.x, y, width, 1),
        );
    }
}

fn truncate(text: &str, width: u16) -> String {
    text.chars().take(width as usize).collect()
}

fn sidebar_dot(state: AgentStateKind) -> &'static str {
    match state {
        AgentStateKind::Working => "◐",
        AgentStateKind::Blocked | AgentStateKind::Done => "●",
        AgentStateKind::Idle | AgentStateKind::Unknown => "·",
    }
}

fn state_color(theme: &Theme, state: AgentStateKind) -> Color {
    match state {
        AgentStateKind::Blocked => theme.blocked,
        AgentStateKind::Working => theme.working,
        AgentStateKind::Done => theme.done,
        AgentStateKind::Idle | AgentStateKind::Unknown => theme.idle,
    }
}

fn pane_title(pane: &kodade_cli_proto::PaneSnapshot) -> String {
    pane.agent
        .as_ref()
        .map(|agent| format!("{agent} — {}", state_name(pane.state)))
        .unwrap_or_else(|| pane.title.clone())
}

fn border_color(theme: &Theme, state: AgentStateKind, focused: bool) -> Color {
    if state == AgentStateKind::Blocked {
        theme.blocked
    } else if focused {
        theme.accent
    } else {
        theme.border
    }
}

fn state_dot(state: AgentStateKind) -> &'static str {
    match state {
        AgentStateKind::Blocked | AgentStateKind::Working => "● ",
        _ => "",
    }
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

pub fn tab_spans_for(layout: &LayoutSnapshot, origin_x: u16) -> Vec<TabSpan> {
    let workspace_len = layout
        .workspaces
        .iter()
        .find(|item| item.active)
        .map(|item| item.name.chars().count())
        .unwrap_or("workspace".len());
    tab_spans(
        origin_x.saturating_add((TAB_PREFIX.chars().count() + workspace_len + 2) as u16),
        &layout.tabs,
    )
}

pub fn pane_rects_for(layout: &LayoutSnapshot, area: Rect) -> Vec<(PaneId, Rect)> {
    let areas = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let mut rects = HashMap::new();
    rects_for(&layout.tree, areas[1], &mut rects);
    let mut pairs: Vec<_> = rects.into_iter().collect();
    pairs.sort_by_key(|(id, _)| id.0);
    pairs
}

fn rects_for(tree: &LayoutTree, rect: Rect, out: &mut HashMap<PaneId, Rect>) {
    match tree {
        LayoutTree::Leaf { pane } => {
            out.insert(*pane, rect);
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let direction = match axis {
                kodade_cli_proto::SplitAxis::Horizontal => LayoutDirection::Horizontal,
                kodade_cli_proto::SplitAxis::Vertical => LayoutDirection::Vertical,
            };
            let span = if direction == LayoutDirection::Horizontal {
                rect.width
            } else {
                rect.height
            };
            let areas = Layout::default()
                .direction(direction)
                .constraints([
                    Constraint::Length(((span as f32 * ratio) as u16).max(1)),
                    Constraint::Min(1),
                ])
                .split(rect);
            rects_for(first, areas[0], out);
            rects_for(second, areas[1], out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kodade_cli_proto::{
        AgentInfo, LayoutSnapshot, SidebarTabInfo, TabId, WorkspaceId, WorkspaceInfo,
    };

    fn snapshot() -> LayoutSnapshot {
        LayoutSnapshot {
            active_workspace: WorkspaceId(1),
            active_tab: TabId(2),
            workspaces: vec![
                WorkspaceInfo {
                    id: WorkspaceId(1),
                    name: "active".into(),
                    active: true,
                    state: AgentStateKind::Blocked,
                    tabs: vec![SidebarTabInfo {
                        id: TabId(2),
                        name: "agents".into(),
                        state: AgentStateKind::Blocked,
                        agents: vec![AgentInfo {
                            pane: PaneId(3),
                            name: "Codex".into(),
                            state: AgentStateKind::Blocked,
                        }],
                    }],
                },
                WorkspaceInfo {
                    id: WorkspaceId(4),
                    name: "other".into(),
                    active: false,
                    state: AgentStateKind::Done,
                    tabs: vec![SidebarTabInfo {
                        id: TabId(5),
                        name: "hidden".into(),
                        state: AgentStateKind::Done,
                        agents: vec![],
                    }],
                },
            ],
            tabs: vec![],
            tree: LayoutTree::Leaf { pane: PaneId(3) },
            panes: vec![],
            zoomed: false,
        }
    }

    #[test]
    fn sidebar_rows_expand_only_the_active_workspace_and_hit_test_targets() {
        let rows = sidebar_rows(&snapshot());
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].label, "▾ active");
        assert_eq!(rows[1].target, SidebarTarget::Tab(TabId(2)));
        assert_eq!(rows[2].target, SidebarTarget::Pane(PaneId(3)));
        assert_eq!(rows[3].label, "▸ other");
        assert_eq!(
            sidebar_row_at(&rows, 2).map(|row| &row.target),
            Some(&SidebarTarget::Pane(PaneId(3)))
        );
        assert_eq!(sidebar_row_at(&rows, 4), None);
    }

    #[test]
    fn content_area_leaves_a_sidebar_or_gutter() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            content_area(area, true),
            Rect::new(SIDEBAR_WIDTH, 0, 56, 24)
        );
        assert_eq!(content_area(area, false), Rect::new(1, 0, 79, 24));
    }
}
