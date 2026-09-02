use std::collections::HashMap;

use kodade_cli_proto::{
    AgentStateKind, CellColor, LayoutSnapshot, LayoutTree, PaneId, Run, Screen, TabId, WorkspaceId,
    ATTR_BOLD, ATTR_DIM, ATTR_INVERSE, ATTR_ITALIC, ATTR_UNDERLINE,
};
use ratatui::{
    layout::{Constraint, Direction as LayoutDirection, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    config::Theme,
    input::{tab_spans, TabSpan},
    mode::{CopyMode, Menu, MenuAction},
    overlay::{render_overlay, Overlay},
};

pub const TAB_PREFIX: &str = " Ködade · ";
pub const SIDEBAR_WIDTH: u16 = 24;
/// Non-selectable heading rows drawn above the sidebar list (currently the
/// lowercase `workspaces` label). Both render and hit-test offset by this.
pub const SIDEBAR_HEADER_ROWS: u16 = 1;

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
    /// The `prefix W` name/path prompt shares the rename text buffer.
    pub new_workspace: bool,
    pub name: &'a str,
    pub navigate: Option<usize>,
    pub copy: Option<&'a CopyMode>,
    pub menu: Option<&'a Menu>,
    /// Persistent resize mode (#14).
    pub resize: bool,
    /// Pending yes/no prompt text (#14).
    pub confirm: Option<&'a str>,
    /// Settings menu (#20); the help overlay and pickers reuse the same widget.
    pub settings: Option<&'a Overlay>,
    pub note: Option<&'a str>,
}

pub fn render(frame: &mut Frame, layout: &LayoutSnapshot, ui: &Ui, theme: &Theme) {
    let Ui {
        sidebar,
        prefix,
        rename,
        new_workspace,
        name,
        navigate,
        copy,
        menu,
        resize,
        confirm,
        settings,
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
    frame.render_widget(
        Paragraph::new(Line::from(tab_bar_spans(workspace, &layout.tabs, theme)))
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
            // Copy mode holds its own frozen Screen; both paths use the same
            // run renderer so styling is identical.
            let screen = copy
                .filter(|copy| copy.pane == pane.id)
                .map(|copy| &copy.screen)
                .unwrap_or(&pane.screen);
            frame.render_widget(
                Paragraph::new(pane_lines(screen, theme))
                    .style(Style::default().fg(theme.text).bg(theme.bg))
                    .block(
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
            // The block cursor only makes sense on the live focused pane.
            if pane.focused && copy.is_none() && pane.scroll_offset == 0 {
                render_cursor(frame, &pane.screen, *rect, theme);
            }
        }
    }
    let status = if let Some(confirm) = confirm {
        format!(" {confirm}")
    } else if rename {
        format!(" rename pane: {name}")
    } else if resize {
        " resize · hjkl 1 · HJKL 5 · esc".into()
    } else if new_workspace {
        format!(" new workspace: {name}")
    } else if copy.is_some() {
        " copy mode · v select · y copy · esc exit".into()
    } else if navigate.is_some() {
        " navigate · j/k move · enter activate · esc exit".into()
    } else if prefix {
        " prefix: % \" b hjkl c n p s w W x z d r · 1-9 X T R D o O ; ! = alt+hjkl alt+r".into()
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
    if let Some(settings) = settings {
        render_overlay(frame, frame.area(), settings, theme);
    }
}

/// Convert a pane `Screen` into styled ratatui lines. Shared by the live pane
/// draw and copy mode so both look the same.
pub fn pane_lines<'a>(screen: &'a Screen, theme: &Theme) -> Vec<Line<'a>> {
    screen
        .rows
        .iter()
        .map(|runs| {
            Line::from(
                runs.iter()
                    .map(|run| run_span(run, theme))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn run_span<'a>(run: &'a Run, theme: &Theme) -> Span<'a> {
    let mut style = Style::default()
        .fg(fg_color(run.fg, theme))
        .bg(bg_color(run.bg, theme));
    let mut modifiers = Modifier::empty();
    if run.attrs & ATTR_BOLD != 0 {
        modifiers |= Modifier::BOLD;
    }
    if run.attrs & ATTR_ITALIC != 0 {
        modifiers |= Modifier::ITALIC;
    }
    if run.attrs & ATTR_UNDERLINE != 0 {
        modifiers |= Modifier::UNDERLINED;
    }
    if run.attrs & ATTR_DIM != 0 {
        modifiers |= Modifier::DIM;
    }
    if run.attrs & ATTR_INVERSE != 0 {
        modifiers |= Modifier::REVERSED;
    }
    if !modifiers.is_empty() {
        style = style.add_modifier(modifiers);
    }
    Span::styled(run.text.as_str(), style)
}

/// Indexed colors 0–15 come from the theme `[ansi]` palette (#8); 16–255 use
/// the terminal's own 256-color cube.
fn cell_color(color: CellColor, theme: &Theme) -> Option<Color> {
    match color {
        CellColor::Default => None,
        CellColor::Indexed(index) if (index as usize) < theme.ansi.len() => {
            Some(theme.ansi[index as usize])
        }
        CellColor::Indexed(index) => Some(Color::Indexed(index)),
        CellColor::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn fg_color(color: CellColor, theme: &Theme) -> Color {
    cell_color(color, theme).unwrap_or(theme.text)
}

fn bg_color(color: CellColor, theme: &Theme) -> Color {
    cell_color(color, theme).unwrap_or(theme.bg)
}

/// Draw a block cursor inside the pane border, keeping whatever character sits
/// under it visible in the pane background color.
fn render_cursor(frame: &mut Frame, screen: &Screen, rect: Rect, theme: &Theme) {
    if !screen.cursor_visible {
        return;
    }
    let x = rect.x.saturating_add(1).saturating_add(screen.cursor_col);
    let y = rect.y.saturating_add(1).saturating_add(screen.cursor_row);
    // Inner area only: the border occupies the outer ring.
    if x + 1 >= rect.x + rect.width || y + 1 >= rect.y + rect.height {
        return;
    }
    let under = cursor_char(screen).unwrap_or_else(|| " ".to_string());
    let width = Span::raw(&under).width().max(1) as u16;
    frame.render_widget(
        Paragraph::new(under).style(Style::default().fg(theme.bg).bg(theme.cursor)),
        Rect::new(x, y, width.min(rect.x + rect.width - 1 - x), 1),
    );
}

/// The character currently under the cursor, walking runs by display width so
/// wide characters count for two columns.
fn cursor_char(screen: &Screen) -> Option<String> {
    let runs = screen.rows.get(screen.cursor_row as usize)?;
    let mut column = 0u16;
    for run in runs {
        for grapheme in run.text.chars() {
            let width = Span::raw(grapheme.to_string()).width().max(1) as u16;
            if screen.cursor_col < column + width {
                return Some(grapheme.to_string());
            }
            column += width;
        }
    }
    None
}

/// Label for one tab, matching `input::tab_spans` geometry exactly so mouse
/// hit-testing lines up with what is rendered.
pub fn tab_label(name: &str, active: bool, state: AgentStateKind) -> String {
    let dot = state_dot(state);
    if active {
        format!("{dot}[{name}]")
    } else {
        format!(" {dot}{name} ")
    }
}

/// Build the tab-bar line: accent wordmark, workspace name, then the tabs with
/// the active one in `tab_active_fg/bg`. Column layout mirrors `tab_spans_for`.
fn tab_bar_spans<'a>(
    workspace: &'a str,
    tabs: &'a [kodade_cli_proto::TabInfo],
    theme: &Theme,
) -> Vec<Span<'a>> {
    let base = Style::default().bg(theme.tabbar_bg);
    let mut spans = vec![
        Span::styled(TAB_PREFIX, base.fg(theme.accent)),
        Span::styled(workspace, base.fg(theme.text)),
        Span::styled("  ", base),
    ];
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ", base));
        }
        let label = tab_label(&tab.name, tab.active, tab.state);
        let style = if tab.active {
            Style::default()
                .fg(theme.tab_active_fg)
                .bg(theme.tab_active_bg)
        } else {
            base.fg(theme.text)
        };
        spans.push(Span::styled(label, style));
    }
    spans
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
        // Show the root basename after the name when it differs (#19 restructures
        // this into its own styled column later; for now it lives in the label).
        let root = workspace
            .root
            .as_deref()
            .and_then(std::path::Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|base| *base != workspace.name);
        let name = match root {
            Some(base) => format!("{}  {}", workspace.name, base),
            None => workspace.name.clone(),
        };
        rows.push(SidebarRow {
            target: SidebarTarget::Workspace(workspace.id),
            label: format!("{} {}", if workspace.active { "▾" } else { "▸" }, name),
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
                    // Active states carry a short age (e.g. `Codex 4m`); #19 restructures later.
                    label: match agent.state {
                        AgentStateKind::Blocked
                        | AgentStateKind::Working
                        | AgentStateKind::Done => {
                            format!("    {} {}", agent.name, format_age(agent.state_age_secs))
                        }
                        _ => format!("    {}", agent.name),
                    },
                    state: agent.state,
                }));
            }
        }
    }
    rows
}

/// Map a screen row (sidebar starts at y=0) to a list entry, skipping the
/// `workspaces` heading row(s). Clicks on a heading return `None`.
pub fn sidebar_row_at(rows: &[SidebarRow], row: u16) -> Option<&SidebarRow> {
    let index = row.checked_sub(SIDEBAR_HEADER_ROWS)?;
    rows.get(index as usize)
}

/// Compact state age: seconds under a minute, minutes under an hour, else hours.
pub fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn render_sidebar(
    frame: &mut Frame,
    layout: &LayoutSnapshot,
    area: Rect,
    navigate: Option<usize>,
    theme: &Theme,
) {
    // Fill the sidebar background, then draw the lowercase dim heading (#8).
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.sidebar_bg)),
        area,
    );
    frame.render_widget(
        Paragraph::new("workspaces").style(Style::default().fg(theme.dim).bg(theme.sidebar_bg)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    for (index, row) in sidebar_rows(layout).iter().enumerate() {
        let y = area
            .y
            .saturating_add(SIDEBAR_HEADER_ROWS)
            .saturating_add(index as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        let text = truncate(
            &format!("{} {}", row.label, sidebar_dot(row.state)),
            area.width,
        );
        let style = if navigate == Some(index) {
            Style::default().fg(theme.sidebar_bg).bg(theme.accent)
        } else {
            Style::default()
                .fg(state_color(theme, row.state))
                .bg(theme.sidebar_bg)
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
            MenuAction::BreakToTab => "Break to tab",
            MenuAction::Equalize => "Equalize",
            MenuAction::MoveLeft => "Move left",
            MenuAction::MoveRight => "Move right",
        })
        .collect::<Vec<_>>();
    let width = 14.min(area.width.saturating_sub(menu.x));
    for (index, label) in labels.iter().enumerate() {
        let y = menu.y.saturating_add(index as u16);
        if y >= area.height {
            break;
        }
        let style = if index == menu.selected {
            Style::default().fg(theme.menu_bg).bg(theme.accent)
        } else {
            Style::default().fg(theme.menu_fg).bg(theme.menu_bg)
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
                    root: Some("/Users/keith/src/active".into()),
                    tabs: vec![SidebarTabInfo {
                        id: TabId(2),
                        name: "agents".into(),
                        state: AgentStateKind::Blocked,
                        agents: vec![AgentInfo {
                            pane: PaneId(3),
                            name: "Codex".into(),
                            state: AgentStateKind::Blocked,
                            state_age_secs: 245,
                        }],
                    }],
                },
                WorkspaceInfo {
                    id: WorkspaceId(4),
                    name: "other".into(),
                    active: false,
                    state: AgentStateKind::Done,
                    root: None,
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
        // Blocked agent rows carry a compact age label.
        assert_eq!(rows[2].label, "    Codex 4m");
        assert_eq!(rows[3].label, "▸ other");
        // Screen row 0 is the `workspaces` heading; the list starts at row 1.
        assert_eq!(sidebar_row_at(&rows, 0), None);
        assert_eq!(
            sidebar_row_at(&rows, 1).map(|row| &row.target),
            Some(&SidebarTarget::Workspace(WorkspaceId(1)))
        );
        assert_eq!(
            sidebar_row_at(&rows, 3).map(|row| &row.target),
            Some(&SidebarTarget::Pane(PaneId(3)))
        );
        assert_eq!(sidebar_row_at(&rows, 5), None);
    }

    #[test]
    fn workspace_row_appends_a_differing_root_basename() {
        let mut layout = snapshot();
        layout.workspaces[0].name = "app".into();
        layout.workspaces[0].root = Some("/Users/keith/src/webapp".into());
        let rows = sidebar_rows(&layout);
        assert_eq!(rows[0].label, "▾ app  webapp");
        // A basename equal to the name is not repeated.
        layout.workspaces[0].root = Some("/Users/keith/src/app".into());
        assert_eq!(sidebar_rows(&layout)[0].label, "▾ app");
    }

    #[test]
    fn tab_bar_spans_match_hit_test_span_widths() {
        use kodade_cli_proto::TabInfo;
        let tabs = vec![
            TabInfo {
                id: TabId(1),
                name: "shell".into(),
                active: true,
                state: AgentStateKind::Idle,
            },
            TabInfo {
                id: TabId(2),
                name: "logs".into(),
                active: false,
                state: AgentStateKind::Working,
            },
        ];
        let workspace = "active";
        let mut layout = snapshot();
        layout.tabs = tabs.clone();
        // Rendered wordmark + workspace + 2 spaces before the first tab.
        let origin = 0;
        let start = origin + (TAB_PREFIX.chars().count() + workspace.chars().count() + 2) as u16;
        let spans = tab_spans_for(&layout, origin);
        // Each hit-test span must be exactly as wide as the rendered label, and
        // the labels must be the ones tab_bar_spans draws.
        let mut column = start;
        for (span, tab) in spans.iter().zip(&tabs) {
            let label = tab_label(&tab.name, tab.active, tab.state);
            assert_eq!(span.start, column);
            assert_eq!(span.end - span.start, label.chars().count() as u16);
            column = span.end + 1; // single-space separator between tabs
        }
    }

    fn run(text: &str, fg: CellColor, bg: CellColor, attrs: u8) -> Run {
        Run {
            text: text.into(),
            fg,
            bg,
            attrs,
        }
    }

    #[test]
    fn runs_map_colors_through_the_theme_and_attributes_to_modifiers() {
        let theme = Theme::kodade_dark();
        let screen = Screen {
            rows: vec![vec![
                run("a", CellColor::Indexed(2), CellColor::Default, ATTR_BOLD),
                run(
                    "b",
                    CellColor::Indexed(200),
                    CellColor::Rgb(1, 2, 3),
                    ATTR_ITALIC | ATTR_UNDERLINE | ATTR_DIM | ATTR_INVERSE,
                ),
                run("c", CellColor::Default, CellColor::Default, 0),
            ]],
            ..Screen::default()
        };
        let lines = pane_lines(&screen, &theme);
        let spans = &lines[0].spans;
        // 0–15 come from the theme palette; 16+ stay terminal-indexed.
        assert_eq!(spans[0].style.fg, Some(theme.ansi[2]));
        assert_eq!(spans[0].style.bg, Some(theme.bg));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].style.fg, Some(Color::Indexed(200)));
        assert_eq!(spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(
            spans[1].style.add_modifier,
            Modifier::ITALIC | Modifier::UNDERLINED | Modifier::DIM | Modifier::REVERSED
        );
        // Default fg/bg fall back to the theme's text and pane background.
        assert_eq!(spans[2].style.fg, Some(theme.text));
        assert_eq!(spans[2].style.bg, Some(theme.bg));
        assert_eq!(spans[2].style.add_modifier, Modifier::empty());
    }

    #[test]
    fn cursor_char_counts_wide_cells_as_two_columns() {
        let screen = Screen {
            cursor_row: 0,
            cursor_col: 2,
            rows: vec![vec![run("宽x", CellColor::Default, CellColor::Default, 0)]],
            ..Screen::default()
        };
        assert_eq!(cursor_char(&screen).as_deref(), Some("x"));
        let past_end = Screen {
            cursor_col: 9,
            ..screen
        };
        assert_eq!(cursor_char(&past_end), None);
    }

    #[test]
    fn focused_pane_draws_styled_cells_and_a_block_cursor() {
        use kodade_cli_proto::PaneSnapshot;
        use ratatui::{backend::TestBackend, Terminal};

        let theme = Theme::kodade_dark();
        let mut layout = snapshot();
        layout.panes = vec![PaneSnapshot {
            id: PaneId(3),
            title: "zsh".into(),
            focused: true,
            scroll_offset: 0,
            screen: Screen {
                contents: "ok".into(),
                cursor_row: 0,
                cursor_col: 1,
                cursor_visible: true,
                rows: vec![vec![run(
                    "ok",
                    CellColor::Indexed(1),
                    CellColor::Default,
                    0,
                )]],
                bracketed_paste: false,
                mouse_reporting: false,
            },
            agent: None,
            state: AgentStateKind::Idle,
            state_reason: String::new(),
            state_age_secs: 0,
            cwd: None,
        }];
        let ui = Ui {
            sidebar: false,
            prefix: false,
            rename: false,
            new_workspace: false,
            name: "",
            navigate: None,
            copy: None,
            menu: None,
            resize: false,
            confirm: None,
            settings: None,
            note: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &layout, &ui, &theme))
            .expect("frame renders");
        let buffer = terminal.backend().buffer();
        // Pane content starts one row below the tab bar and one cell inside the
        // border; `o` is themed red and `k` sits under the block cursor.
        let first = &buffer[(2, 2)];
        assert_eq!(first.symbol(), "o");
        assert_eq!(first.fg, theme.ansi[1]);
        let cursor = &buffer[(3, 2)];
        assert_eq!(cursor.symbol(), "k");
        assert_eq!(cursor.bg, theme.cursor);
    }

    #[test]
    fn format_age_uses_compact_units() {
        assert_eq!(format_age(4), "4s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(245), "4m");
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(7300), "2h");
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
