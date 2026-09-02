use std::collections::HashMap;
use std::sync::OnceLock;

use kodade_cli_proto::{
    AgentStateKind, CellColor, LayoutSnapshot, LayoutTree, PaneId, Run, Screen, TabId, TabInfo,
    WorkspaceId, ATTR_BOLD, ATTR_DIM, ATTR_INVERSE, ATTR_ITALIC, ATTR_UNDERLINE,
};
use ratatui::{
    layout::{Constraint, Direction as LayoutDirection, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    config::{StatusWidget, Theme},
    input::{tab_spans, TabSpan},
    mode::{menu_origin_x, CopyMode, Menu, MenuAction, MENU_WIDTH},
    overlay::{render_overlay, Overlay},
    selection::Selection,
};

/// Longest a single tab name is shown before ellipsis; the whole bar then
/// scrolls horizontally to keep the active tab visible.
const MAX_TAB_NAME: usize = 24;

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
    /// Real session name for the status bar left segment (#11).
    pub session: &'a str,
    /// Right-side status widgets to draw, in order (#11).
    pub status_right: &'a [StatusWidget],
    /// `prefix q` flash: draw big pane ids over each pane (#11).
    pub flash: bool,
    /// Show the `prefix b · sidebar` hint in the status bar (#24 gutter hint).
    pub sidebar_hint: bool,
    /// Live mouse selection, highlighted in its own pane (#12).
    pub selection: Option<&'a Selection>,
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
        session,
        status_right,
        flash,
        sidebar_hint,
        selection,
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
    render_tab_bar(frame, workspace, &layout.tabs, areas[0], theme);

    let mut rects = HashMap::new();
    rects_for(&layout.tree, areas[1], &mut rects);
    for pane in &layout.panes {
        if let Some(rect) = rects.get(&pane.id) {
            // Copy mode holds its own frozen Screen; both paths use the same
            // run renderer so styling is identical.
            let screen = copy
                .filter(|copy| copy.pane == pane.id)
                .map(|copy| &copy.screen)
                .unwrap_or(&pane.screen);
            // A mouse selection highlights only in the pane it started in.
            let selection = selection.filter(|selection| selection.pane == pane.id);
            frame.render_widget(
                Paragraph::new(pane_lines(screen, theme, selection))
                    .style(Style::default().fg(theme.text).bg(theme.bg))
                    .block(pane_block(pane, *rect, theme)),
                *rect,
            );
            // The block cursor only makes sense on the live focused pane.
            if pane.focused && copy.is_none() && pane.scroll_offset == 0 {
                render_cursor(frame, &pane.screen, *rect, theme);
            }
        }
    }
    // `prefix q` overlay: big pane ids centered in each pane for a beat.
    if flash {
        for pane in &layout.panes {
            if let Some(rect) = rects.get(&pane.id) {
                render_pane_flash(frame, pane.id, *rect, theme);
            }
        }
    }
    // Left status: a mode hint overrides, else session · workspace · tab.
    let left = if let Some(confirm) = confirm {
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
        " prefix: % \" b hjkl c m n p s w W x z d r q · 1-9 X T R D o O ; ! = alt+hjkl alt+r ctrl+r"
            .into()
    } else {
        let tab = active_tab_name(layout);
        if sidebar_hint {
            format!(" ▸ prefix b · sidebar · {session} · {workspace} · {tab}")
        } else {
            format!(" {session} · {workspace} · {tab}")
        }
    };
    frame.render_widget(
        Paragraph::new(left).style(Style::default().fg(theme.dim).bg(theme.status_bg)),
        areas[2],
    );
    // Right status widgets, drawn over the same row at the right edge.
    let widgets = status_right_spans(status_right, layout, theme);
    if !widgets.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(widgets).right_aligned())
                .style(Style::default().bg(theme.status_bg)),
            areas[2],
        );
    }
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

/// The active tab's name in the active workspace, for the status bar / title.
fn active_tab_name(layout: &LayoutSnapshot) -> &str {
    layout
        .workspaces
        .iter()
        .find(|workspace| workspace.active)
        .and_then(|workspace| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.id == layout.active_tab)
        })
        .map(|tab| tab.name.as_str())
        .unwrap_or("")
}

/// Build the border block for a pane: `#id name — state` on the left (id dim)
/// with a `[scroll]` marker, and the cwd basename right-aligned when wide enough.
fn pane_block<'a>(
    pane: &'a kodade_cli_proto::PaneSnapshot,
    rect: Rect,
    theme: &Theme,
) -> Block<'a> {
    let dim = Style::default().fg(theme.dim);
    let name = pane.agent.as_deref().unwrap_or(pane.title.as_str());
    let mut spans = vec![
        Span::styled(format!("#{} ", pane.id.0), dim),
        Span::styled(name.to_string(), Style::default().fg(theme.text)),
        Span::styled(format!(" — {}", state_name(pane.state)), dim),
    ];
    if pane.scroll_offset > 0 {
        spans.push(Span::styled(" [scroll]", Style::default().fg(theme.accent)));
    }
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title_top(Line::from(spans))
        .border_style(Style::default().fg(border_color(theme, pane.state, pane.focused)));
    // Show the cwd basename on the right of the top border on wide panes.
    if rect.width >= 30 {
        if let Some(base) = pane
            .cwd
            .as_deref()
            .and_then(std::path::Path::file_name)
            .and_then(|base| base.to_str())
        {
            block =
                block.title_top(Line::from(Span::styled(base.to_string(), dim)).right_aligned());
        }
    }
    block
}

/// Draw one big pane id centered in the pane, accent-on-bg, for the flash.
fn render_pane_flash(frame: &mut Frame, id: PaneId, rect: Rect, theme: &Theme) {
    if rect.width <= 2 || rect.height <= 2 {
        return;
    }
    let label = format!(" {} ", id.0);
    let width = (label.chars().count() as u16).min(rect.width);
    let x = rect.x + (rect.width.saturating_sub(width)) / 2;
    let y = rect.y + rect.height / 2;
    frame.render_widget(
        Paragraph::new(label).style(
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(x, y, width, 1),
    );
}

/// Right-side status widgets in config order, joined by two spaces plus a
/// trailing pad so nothing sits flush against the terminal edge.
fn status_right_spans(
    widgets: &[StatusWidget],
    layout: &LayoutSnapshot,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme.dim).bg(theme.status_bg);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push = |span: Span<'static>| {
        if !spans.is_empty() {
            spans.push(Span::styled("  ", dim));
        }
        spans.push(span);
    };
    for widget in widgets {
        match widget {
            StatusWidget::Zoom if layout.zoomed => push(Span::styled(
                "[zoom]",
                Style::default().fg(theme.accent).bg(theme.status_bg),
            )),
            StatusWidget::Zoom => {}
            StatusWidget::Blocked => {
                let count = blocked_count(layout);
                if count > 0 {
                    push(Span::styled(
                        format!("● {count} blocked"),
                        Style::default().fg(theme.blocked).bg(theme.status_bg),
                    ));
                }
            }
            StatusWidget::Hostname => push(Span::styled(hostname().to_string(), dim)),
            StatusWidget::Time => push(Span::styled(local_hh_mm(), dim)),
        }
    }
    if !spans.is_empty() {
        spans.push(Span::styled(" ", dim));
    }
    spans
}

/// Panes reporting `blocked` across every workspace.
fn blocked_count(layout: &LayoutSnapshot) -> usize {
    layout
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.agents)
        .filter(|agent| agent.state == AgentStateKind::Blocked)
        .count()
}

/// Local host name, read once via `gethostname` and cached.
fn hostname() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        let mut buf = [0_u8; 256];
        // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc != 0 {
            return String::new();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    })
}

/// Local `HH:MM` without pulling in a date crate.
fn local_hh_mm() -> String {
    // SAFETY: localtime_r writes into a zeroed tm we own; time() takes null.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// Convert a pane `Screen` into styled ratatui lines. Shared by the live pane
/// draw and copy mode so both look the same.
pub fn pane_lines<'a>(
    screen: &'a Screen,
    theme: &Theme,
    selection: Option<&Selection>,
) -> Vec<Line<'a>> {
    screen
        .rows
        .iter()
        .enumerate()
        .map(|(row, runs)| {
            let mut spans = Vec::with_capacity(runs.len());
            let mut column = 0usize;
            for run in runs {
                let width = run.text.chars().count();
                spans.extend(run_spans(run, theme, row, column, selection));
                column += width;
            }
            Line::from(spans)
        })
        .collect()
}

/// One run as spans, split where the mouse selection starts or ends so the
/// selected cells get `theme.selection` as their background (#12).
fn run_spans<'a>(
    run: &'a Run,
    theme: &Theme,
    row: usize,
    column: usize,
    selection: Option<&Selection>,
) -> Vec<Span<'a>> {
    let base = run_span(run, theme);
    let Some(selection) = selection else {
        return vec![base];
    };
    let highlight = base.style.bg(theme.selection);
    let mut spans = Vec::new();
    let mut chunk = String::new();
    let mut selected = false;
    for (offset, character) in run.text.chars().enumerate() {
        let now = selection.contains(row, column + offset);
        if now != selected && !chunk.is_empty() {
            let style = if selected { highlight } else { base.style };
            spans.push(Span::styled(std::mem::take(&mut chunk), style));
        }
        selected = now;
        chunk.push(character);
    }
    if !chunk.is_empty() {
        let style = if selected { highlight } else { base.style };
        spans.push(Span::styled(chunk, style));
    }
    spans
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
    let name = truncate_ellipsis(name, MAX_TAB_NAME);
    if active {
        format!("{dot}[{name}]")
    } else {
        format!(" {dot}{name} ")
    }
}

/// Draw the tab bar: a fixed wordmark + workspace name on the left, then the
/// tabs in a scrolling sub-area so the active tab stays visible on overflow.
fn render_tab_bar(frame: &mut Frame, workspace: &str, tabs: &[TabInfo], area: Rect, theme: &Theme) {
    let base = Style::default().bg(theme.tabbar_bg);
    // Fixed prefix fills the whole row (background) and draws the wordmark.
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(TAB_PREFIX, base.fg(theme.accent)),
            Span::styled(workspace.to_string(), base.fg(theme.text)),
            Span::styled("  ", base),
        ]))
        .style(Style::default().fg(theme.text).bg(theme.tabbar_bg)),
        area,
    );
    let start = tab_bar_start(workspace);
    if start >= area.width {
        return;
    }
    let tabs_rect = Rect::new(area.x + start, area.y, area.width - start, 1);
    let offset = tab_scroll_offset(tabs, tabs_rect.width);
    frame.render_widget(
        Paragraph::new(Line::from(tab_bar_spans_only(tabs, theme)))
            .style(base)
            .scroll((0, offset)),
        tabs_rect,
    );
}

/// Column where the first tab starts, relative to the tab-bar area origin.
fn tab_bar_start(workspace: &str) -> u16 {
    (TAB_PREFIX.chars().count() + workspace.chars().count() + 2) as u16
}

/// Tab labels only (no wordmark), positioned from column 0. `tab_spans(0, ..)`
/// mirrors this geometry so hit-testing lines up after the same scroll offset.
fn tab_bar_spans_only<'a>(tabs: &'a [TabInfo], theme: &Theme) -> Vec<Span<'a>> {
    let base = Style::default().bg(theme.tabbar_bg);
    let mut spans = Vec::new();
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ", base));
        }
        let label = tab_label(&tab.name, tab.active, tab.state);
        let mut style = if tab.active {
            Style::default()
                .fg(theme.tab_active_fg)
                .bg(theme.tab_active_bg)
        } else {
            base.fg(theme.text)
        };
        // Blocked tabs tint bold in the blocked color, matching the border/row.
        if tab.state == AgentStateKind::Blocked {
            style = style.fg(theme.blocked).add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(label, style));
    }
    spans
}

/// Horizontal scroll (in columns) so the active tab is fully visible when the
/// tabs overflow the available width. Shared by render and hit-testing.
fn tab_scroll_offset(tabs: &[TabInfo], available: u16) -> u16 {
    let rel = tab_spans(0, tabs);
    let total = rel.last().map(|span| span.end).unwrap_or(0);
    if total <= available {
        return 0;
    }
    let Some(index) = tabs.iter().position(|tab| tab.active) else {
        return 0;
    };
    let (start, end) = (rel[index].start, rel[index].end);
    let mut offset = 0;
    if end > available {
        offset = end - available;
    }
    if start < offset {
        offset = start;
    }
    offset
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
        let text = truncate_ellipsis(
            &format!("{} {}", row.label, sidebar_dot(row.state)),
            area.width as usize,
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
    // Flip the menu left of the click when it would clip the right edge (#24).
    let x = menu_origin_x(menu.x, area.width);
    let width = MENU_WIDTH.min(area.width.saturating_sub(x));
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
            Rect::new(x, y, width, 1),
        );
    }
}

/// Truncate `text` to `width` columns, appending `…` when it overflows. Counts
/// `char`s (not display width), which suits the labels this draws (#11).
pub fn truncate_ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
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

/// Hit-test spans for the tab bar, in absolute screen columns. `area` is the
/// content region (right of the sidebar). Applies the same scroll offset and
/// clipping as `render_tab_bar`, so clicks land on the rendered tabs.
pub fn tab_spans_for(layout: &LayoutSnapshot, area: Rect) -> Vec<TabSpan> {
    let workspace = layout
        .workspaces
        .iter()
        .find(|item| item.active)
        .map(|item| item.name.as_str())
        .unwrap_or("workspace");
    let start = tab_bar_start(workspace);
    if start >= area.width {
        return Vec::new();
    }
    let available = area.width - start;
    let origin = area.x + start;
    let offset = tab_scroll_offset(&layout.tabs, available);
    tab_spans(0, &layout.tabs)
        .into_iter()
        .zip(&layout.tabs)
        .filter_map(|(span, tab)| {
            let s = span.start as i32 - offset as i32;
            let e = span.end as i32 - offset as i32;
            // Fully scrolled off the left, or starts past the right edge.
            if e <= 0 || s >= available as i32 {
                return None;
            }
            let s = s.max(0) as u16;
            let e = (e as u16).min(available);
            Some(TabSpan {
                id: tab.id,
                start: origin + s,
                end: origin + e,
            })
        })
        .collect()
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
            restored: false,
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
        // Rendered wordmark + workspace + 2 spaces before the first tab. A wide
        // area means no overflow, so the scroll offset is zero.
        let area = Rect::new(0, 0, 200, 24);
        let start = area.x + (TAB_PREFIX.chars().count() + workspace.chars().count() + 2) as u16;
        let spans = tab_spans_for(&layout, area);
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
        let lines = pane_lines(&screen, &theme, None);
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
    fn selected_cells_take_the_theme_selection_background() {
        use crate::selection::{Selection, SelectionMode};
        let theme = Theme::kodade_dark();
        let screen = Screen {
            contents: "hello".into(),
            rows: vec![vec![run(
                "hello",
                CellColor::Default,
                CellColor::Default,
                0,
            )]],
            ..Screen::default()
        };
        let mut selection = Selection::new(PaneId(1), (0, 1), SelectionMode::Char, &screen);
        selection.set_head((0, 2), &screen);
        let lines = pane_lines(&screen, &theme, Some(&selection));
        let spans = &lines[0].spans;
        // The run splits into before / selected / after.
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "h");
        assert_eq!(spans[0].style.bg, Some(theme.bg));
        assert_eq!(spans[1].content.as_ref(), "el");
        assert_eq!(spans[1].style.bg, Some(theme.selection));
        assert_eq!(spans[2].content.as_ref(), "lo");
        assert_eq!(spans[2].style.bg, Some(theme.bg));
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
            session: "work",
            status_right: &[],
            flash: false,
            sidebar_hint: false,
            selection: None,
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
        // #24: content width + sidebar/gutter width must cover the whole row so
        // `pane_cols` never drifts by the collapsed 1-col gutter.
        for sidebar in [true, false] {
            assert_eq!(
                content_area(area, sidebar).width + sidebar_width(sidebar),
                area.width
            );
        }
    }

    #[test]
    fn truncate_ellipsis_appends_a_marker_only_on_overflow() {
        assert_eq!(truncate_ellipsis("short", 10), "short");
        assert_eq!(truncate_ellipsis("exact", 5), "exact");
        assert_eq!(truncate_ellipsis("overlong", 5), "over…");
        assert_eq!(truncate_ellipsis("x", 0), "");
    }

    fn tab(id: u64, name: &str, active: bool) -> kodade_cli_proto::TabInfo {
        kodade_cli_proto::TabInfo {
            id: TabId(id),
            name: name.into(),
            active,
            state: AgentStateKind::Idle,
        }
    }

    #[test]
    fn tab_bar_scrolls_to_keep_the_active_tab_visible() {
        let mut layout = snapshot();
        layout.tabs = (1..=8)
            .map(|i| tab(i, &format!("tab{i}"), i == 6))
            .collect();
        // Narrow content area forces overflow (8 tabs, ~6 cols each).
        let area = Rect::new(0, 0, 40, 24);
        let start = tab_bar_start("active");
        let available = area.width - start;
        let spans = tab_spans_for(&layout, area);
        let active = spans
            .iter()
            .find(|span| span.id == TabId(6))
            .expect("active tab is drawn");
        // The active tab lands fully inside the visible tab strip.
        assert!(active.start >= area.x + start);
        assert!(active.end <= area.x + start + available);
        let label = tab_label("tab6", true, AgentStateKind::Idle);
        assert_eq!(active.end - active.start, label.chars().count() as u16);
        // The first tab scrolled off the left edge (absent or left-clipped).
        let first = spans.iter().find(|span| span.id == TabId(1));
        assert!(first.is_none_or(|span| span.start == area.x + start));
    }

    #[test]
    fn blocked_state_tints_border_tab_and_workspace_row() {
        use kodade_cli_proto::PaneSnapshot;
        use ratatui::{backend::TestBackend, Terminal};

        let theme = Theme::kodade_dark();
        let mut layout = snapshot();
        // The active workspace, its tab, and the pane are all blocked.
        layout.tabs = vec![tab(2, "agents", true)];
        layout.tabs[0].state = AgentStateKind::Blocked;
        layout.panes = vec![PaneSnapshot {
            id: PaneId(3),
            title: "zsh".into(),
            focused: true,
            scroll_offset: 0,
            screen: Screen::default(),
            agent: Some("Codex".into()),
            state: AgentStateKind::Blocked,
            state_reason: String::new(),
            state_age_secs: 0,
            cwd: None,
        }];
        let ui = Ui {
            sidebar: true,
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
            session: "work",
            status_right: &[],
            flash: false,
            sidebar_hint: false,
            selection: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &layout, &ui, &theme))
            .expect("frame renders");
        let buffer = terminal.backend().buffer();
        let has_blocked_fg = |y: u16, xs: std::ops::Range<u16>| {
            xs.clone().any(|x| buffer[(x, y)].fg == theme.blocked)
        };
        // Pane border: top-left corner of the content area is drawn blocked.
        assert_eq!(buffer[(SIDEBAR_WIDTH, 1)].fg, theme.blocked);
        // Tab bar row (y=0) carries the blocked tab label.
        assert!(has_blocked_fg(0, SIDEBAR_WIDTH..80));
        // Sidebar workspace row (y=1, below the heading) is tinted blocked.
        assert!(has_blocked_fg(1, 0..SIDEBAR_WIDTH));
    }
}
