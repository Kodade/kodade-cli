use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use kodade_cli_proto::{
    AgentInfo, AgentStateKind, CellColor, LayoutSnapshot, LayoutTree, PaneId, Run, Screen, TabId,
    TabInfo, WorkspaceId, WorkspaceInfo, ATTR_BOLD, ATTR_DIM, ATTR_INVERSE, ATTR_ITALIC,
    ATTR_UNDERLINE,
};
use ratatui::{
    layout::{Constraint, Direction as LayoutDirection, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    config::{Config, StatusWidget, Theme},
    input::{tab_spans, TabSpan},
    mode::{menu_origin_x, CopyMode, Menu, MenuAction, MENU_WIDTH},
    overlay::{self, render_overlay, Overlay},
    selection::Selection,
};

/// Longest a single tab name is shown before ellipsis; the whole bar then
/// scrolls horizontally to keep the active tab visible.
const MAX_TAB_NAME: usize = 24;

pub const TAB_PREFIX: &str = " Ködade · ";

/// Which of the three sidebar shapes is showing (#19). `prefix b` cycles
/// full → compact → hidden → full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarMode {
    /// The full list of workspaces, tabs, panes, and the agents panel.
    Full,
    /// A 3-column rail of workspace state dots.
    Compact,
    /// A 1-column gutter (as in v0.1).
    Hidden,
}

impl SidebarMode {
    /// `prefix b` cycle: full → compact → hidden → full.
    pub fn next(self) -> Self {
        match self {
            Self::Full => Self::Compact,
            Self::Compact => Self::Hidden,
            Self::Hidden => Self::Full,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarTarget {
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

/// What a sidebar row represents. Headings carry no target so hit-testing and
/// navigate skip them naturally (#19, replaces `SIDEBAR_HEADER_ROWS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarKind {
    Heading,
    Workspace,
    Tab,
    Pane,
    Agent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SidebarRow {
    pub kind: SidebarKind,
    /// `None` for heading rows, which are neither clickable nor navigable.
    pub target: Option<SidebarTarget>,
    pub label: String,
    pub state: AgentStateKind,
    /// Rows under a non-active expanded workspace draw dimmer (#19).
    pub dim: bool,
    /// Swatch color for a workspace row (explicit or auto-hashed).
    pub color: Option<Color>,
}

/// The sidebar's two stacked sections: the scrolling workspaces list and the
/// agents panel below it. Concatenated, they form the flat navigate list.
#[derive(Debug, Clone, PartialEq)]
pub struct SidebarModel {
    pub workspaces: Vec<SidebarRow>,
    pub agents: Vec<SidebarRow>,
}

impl SidebarModel {
    /// Flat rows (workspaces then agents), the index space navigate/click use.
    pub fn flat(&self) -> Vec<&SidebarRow> {
        self.workspaces.iter().chain(self.agents.iter()).collect()
    }

    /// Owned clones of the flat rows, for callers that need to index by value.
    pub fn into_flat(self) -> Vec<SidebarRow> {
        let mut rows = self.workspaces;
        rows.extend(self.agents);
        rows
    }
}

/// Per-frame UI state that is not part of the daemon layout snapshot.
pub struct Ui<'a> {
    pub sidebar_mode: SidebarMode,
    /// Effective sidebar width for the current mode + config.
    pub sidebar_width: u16,
    /// Workspaces the user has collapsed in the sidebar list (#19).
    pub collapsed: &'a HashSet<WorkspaceId>,
    /// Whether the agents panel is enabled in config (#19).
    pub agents_panel: bool,
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
    /// Status-bar note text and the color it is drawn in (#10 toasts use the
    /// agent-state color; ordinary notes use `done`).
    pub note: Option<(&'a str, Color)>,
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
    /// Prefix-mode chord hint, generated from the live bindings (#6).
    pub prefix_hint: &'a str,
    /// First-attach `ctrl+b ? for help` hint, shown until help is opened (#6).
    pub first_attach_hint: Option<&'a str>,
    /// Help overlay (`prefix ?`), drawn over everything else (#6).
    pub help: Option<&'a Overlay>,
    /// Workspace / goto picker (`prefix w` / `prefix g`), drawn like help (#17).
    pub picker: Option<&'a crate::picker::Picker>,
}

pub fn render(frame: &mut Frame, layout: &LayoutSnapshot, ui: &Ui, theme: &Theme) {
    let Ui {
        sidebar_mode,
        sidebar_width,
        collapsed,
        agents_panel,
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
        prefix_hint,
        first_attach_hint,
        help,
        picker,
    } = *ui;
    let areas = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Length(sidebar_width), Constraint::Min(1)])
        .split(frame.area());
    match sidebar_mode {
        SidebarMode::Full => render_sidebar(
            frame,
            layout,
            areas[0],
            navigate,
            collapsed,
            agents_panel,
            theme,
        ),
        SidebarMode::Compact => render_sidebar_rail(frame, layout, areas[0], theme),
        SidebarMode::Hidden => frame.render_widget(
            Paragraph::new("▸").style(Style::default().fg(theme.dim)),
            areas[0],
        ),
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
            let block = pane_block(pane, *rect, theme);
            // The copy-mode pane draws its own plain-text scrollback viewport
            // (selection + search highlighted); every other pane draws the
            // styled live screen (#7) with any live mouse selection (#12).
            if let Some(cm) = copy.filter(|copy| copy.pane == pane.id) {
                let inner = block.inner(*rect);
                frame.render_widget(block, *rect);
                render_copy(frame, cm, inner, theme);
            } else {
                // A mouse selection highlights only in the pane it started in.
                let selection = selection.filter(|selection| selection.pane == pane.id);
                frame.render_widget(
                    Paragraph::new(pane_lines(&pane.screen, theme, selection))
                        .style(Style::default().fg(theme.text).bg(theme.bg))
                        .block(block),
                    *rect,
                );
                // The block cursor only makes sense on the live focused pane.
                if pane.focused && copy.is_none() && pane.scroll_offset == 0 {
                    render_cursor(frame, &pane.screen, *rect, theme);
                }
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
    } else if let Some(cm) = copy {
        if let Some(prompt) = &cm.prompt {
            // Live search entry: echo the sigil and the query being typed.
            let sigil = if prompt.forward { '/' } else { '?' };
            format!(" {sigil}{}", prompt.input)
        } else {
            format!(
                " copy · {}/{} · / search · v V select · y copy · e editor · esc",
                cm.cursor.row + 1,
                cm.line_count()
            )
        }
    } else if let Some(index) = navigate {
        // Show the selected row's full (un-truncated) label so a clipped
        // sidebar row is still legible (#19).
        let model = sidebar_rows(layout, collapsed, agents_panel);
        match model.flat().get(index).map(|row| row.label.trim()) {
            Some(label) if !label.is_empty() => {
                format!(" navigate · {label} · enter · * expand · esc")
            }
            _ => " navigate · j/k move · enter activate · * expand · esc exit".into(),
        }
    } else if prefix {
        // Generated from the live bindings so remaps show through (#6).
        format!(" prefix: {prefix_hint} · ? help")
    } else {
        let tab = active_tab_name(layout);
        let core = if sidebar_hint {
            format!("▸ prefix b · sidebar · {session} · {workspace} · {tab}")
        } else {
            format!("{session} · {workspace} · {tab}")
        };
        // First-attach nudge toward the help overlay, cleared once it is opened.
        match first_attach_hint {
            Some(hint) => format!(" {hint} · {core}"),
            None => format!(" {core}"),
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
    if let Some((note, color)) = note {
        frame.render_widget(
            Paragraph::new(note).style(Style::default().fg(color).bg(theme.status_bg)),
            areas[2],
        );
    }
    if let Some(menu) = menu {
        render_menu(frame, menu, frame.area(), theme);
    }
    if let Some(settings) = settings {
        render_overlay(frame, frame.area(), settings, theme);
    }
    // The help overlay draws last so it sits above every other surface.
    if let Some(help) = help {
        render_overlay(frame, frame.area(), help, theme);
    }
    // The picker draws over everything, with a state-colored dot per row.
    if let Some(picker) = picker {
        overlay::render_picker(frame, frame.area(), &picker.overlay, theme, |index| {
            let state = picker.state_at(index);
            (picker_dot(state), state_color(theme, state))
        });
    }
}

// The leading dot glyph for a picker row, matching the sidebar's state marks.
fn picker_dot(state: AgentStateKind) -> &'static str {
    match state {
        AgentStateKind::Working => "◐ ",
        AgentStateKind::Blocked | AgentStateKind::Done => "● ",
        AgentStateKind::Idle | AgentStateKind::Unknown => "· ",
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

/// Draw the copy-mode viewport: plain history lines from `top`, with the
/// selection in `theme.selection`, search matches in `theme.accent`, and the
/// cursor cell reversed. Copy mode is intentionally unstyled otherwise — the
/// frozen cell colors of the live screen are not reproduced here.
fn render_copy(frame: &mut Frame, cm: &CopyMode, area: Rect, theme: &Theme) {
    let height = area.height as usize;
    let rows: Vec<Line> = (0..height)
        .map(|i| {
            let row = cm.top + i;
            if row < cm.line_count() {
                copy_line(cm, row, theme)
            } else {
                Line::default()
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(rows).style(Style::default().fg(theme.text).bg(theme.bg)),
        area,
    );
}

/// One copy-mode line as styled spans, grouping runs of equal style. Priority
/// is cursor > selection > search match > base text.
fn copy_line<'a>(cm: &'a CopyMode, row: usize, theme: &Theme) -> Line<'a> {
    let line = cm.line(row);
    let selection = cm.selection_span(row);
    let matches = cm.search_spans(row);
    let cursor_col = (cm.cursor.row == row).then_some(cm.cursor.col);

    let base = Style::default().fg(theme.text).bg(theme.bg);
    let sel_style = Style::default().fg(theme.text).bg(theme.selection);
    let match_style = Style::default().fg(theme.bg).bg(theme.accent);
    let cursor_style = Style::default().fg(theme.bg).bg(theme.cursor);

    let mut spans: Vec<Span> = Vec::new();
    let mut text = String::new();
    let mut style = base;
    for (col, ch) in line.chars().enumerate() {
        let mut cell = base;
        if matches.iter().any(|(s, e)| col >= *s && col < *e) {
            cell = match_style;
        }
        if selection.is_some_and(|(s, e)| col >= s && col < e) {
            cell = sel_style;
        }
        if cursor_col == Some(col) {
            cell = cursor_style;
        }
        if cell != style && !text.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut text), style));
        }
        style = cell;
        text.push(ch);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    // A cursor resting at or past end-of-line gets a trailing highlighted cell.
    if cursor_col == Some(line.chars().count()) {
        spans.push(Span::styled(" ", cursor_style));
    }
    Line::from(spans)
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

/// Effective sidebar width for the current mode and config (#19). Full clamps
/// the configured width; compact is a 3-column rail; hidden is a 1-column gutter.
pub fn sidebar_width(mode: SidebarMode, config: &Config) -> u16 {
    use crate::config::{SIDEBAR_WIDTH_MAX, SIDEBAR_WIDTH_MIN};
    match mode {
        SidebarMode::Full => config
            .sidebar_width
            .clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX),
        SidebarMode::Compact => 3,
        SidebarMode::Hidden => 1,
    }
}

pub fn content_area(area: Rect, mode: SidebarMode, config: &Config) -> Rect {
    Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width(mode, config)),
            Constraint::Min(1),
        ])
        .split(area)[1]
}

/// A heading row (lowercase dim, no target).
fn heading_row(label: &str) -> SidebarRow {
    SidebarRow {
        kind: SidebarKind::Heading,
        target: None,
        label: label.to_string(),
        state: AgentStateKind::Unknown,
        dim: false,
        color: None,
    }
}

/// Build the sidebar model: a `workspaces` heading plus one row per workspace,
/// expanding tabs and agents for every workspace not in `collapsed`; then an
/// `agents` panel section when `agents_panel` is set and any agents exist (#19).
pub fn sidebar_rows(
    layout: &LayoutSnapshot,
    collapsed: &HashSet<WorkspaceId>,
    agents_panel: bool,
) -> SidebarModel {
    let mut workspaces = vec![heading_row("workspaces")];
    for workspace in &layout.workspaces {
        let expanded = !collapsed.contains(&workspace.id);
        // Rows under a non-active but expanded workspace draw dimmer.
        let dim = !workspace.active;
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
        let caret = if expanded { "▾" } else { "▸" };
        workspaces.push(SidebarRow {
            kind: SidebarKind::Workspace,
            target: Some(SidebarTarget::Workspace(workspace.id)),
            label: format!("{caret} {name}"),
            state: workspace.state,
            dim,
            color: Some(workspace_color(workspace)),
        });
        if expanded {
            for tab in &workspace.tabs {
                workspaces.push(SidebarRow {
                    kind: SidebarKind::Tab,
                    target: Some(SidebarTarget::Tab(tab.id)),
                    label: format!("  {}", tab.name),
                    state: tab.state,
                    dim,
                    color: None,
                });
                workspaces.extend(tab.agents.iter().map(|agent| SidebarRow {
                    kind: SidebarKind::Pane,
                    target: Some(SidebarTarget::Pane(agent.pane)),
                    label: agent_label("    ", agent),
                    state: agent.state,
                    dim,
                    color: None,
                }));
            }
        }
    }
    let agents = if agents_panel {
        agents_panel_rows(layout)
    } else {
        Vec::new()
    };
    SidebarModel { workspaces, agents }
}

/// A `● Name · workspace/tab age` label for an agent, with the given indent.
fn agent_label(indent: &str, agent: &AgentInfo) -> String {
    match agent.state {
        AgentStateKind::Blocked | AgentStateKind::Working | AgentStateKind::Done => {
            format!(
                "{indent}{} {}",
                agent.name,
                format_age(agent.state_age_secs)
            )
        }
        _ => format!("{indent}{}", agent.name),
    }
}

/// The agents-panel section: a heading plus every agent pane in the session,
/// sorted by urgency (blocked, working, done, idle) then age (#19).
fn agents_panel_rows(layout: &LayoutSnapshot) -> Vec<SidebarRow> {
    let mut agents: Vec<(
        &WorkspaceInfo,
        &kodade_cli_proto::SidebarTabInfo,
        &AgentInfo,
    )> = layout
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace
                .tabs
                .iter()
                .flat_map(move |tab| tab.agents.iter().map(move |agent| (workspace, tab, agent)))
        })
        .collect();
    if agents.is_empty() {
        return Vec::new();
    }
    agents.sort_by_key(|(_, _, agent)| (urgency(agent.state), u64::MAX - agent.state_age_secs));
    let mut rows = vec![heading_row("agents")];
    rows.extend(agents.into_iter().map(|(workspace, tab, agent)| {
        let dot = sidebar_dot(agent.state);
        let where_ = format!("{}/{}", workspace.name, tab.name);
        let label = match agent.state {
            AgentStateKind::Blocked | AgentStateKind::Working | AgentStateKind::Done => format!(
                "{dot} {} · {where_} {}",
                agent.name,
                format_age(agent.state_age_secs)
            ),
            _ => format!("{dot} {} · {where_}", agent.name),
        };
        SidebarRow {
            kind: SidebarKind::Agent,
            target: Some(SidebarTarget::Pane(agent.pane)),
            label,
            state: agent.state,
            dim: false,
            color: None,
        }
    }));
    rows
}

/// Urgency rank for the agents panel sort: blocked first, idle/unknown last.
fn urgency(state: AgentStateKind) -> u8 {
    match state {
        AgentStateKind::Blocked => 0,
        AgentStateKind::Working => 1,
        AgentStateKind::Done => 2,
        AgentStateKind::Idle => 3,
        AgentStateKind::Unknown => 4,
    }
}

/// How the sidebar height splits between the scrolling workspaces list and the
/// agents panel (min of the agent rows and 40% of the height), plus the
/// workspace scroll offset that keeps the selected navigate row visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarLayout {
    pub ws_offset: usize,
    pub ws_height: u16,
    /// First row (relative to the sidebar top) of the agents panel.
    pub agents_y: u16,
    pub agents_height: u16,
    /// First agents-section row drawn, so a selected agent past the panel
    /// height scrolls into view (#19).
    pub agents_offset: usize,
}

pub fn sidebar_layout(height: u16, model: &SidebarModel, selected: Option<usize>) -> SidebarLayout {
    let agents_len = model.agents.len() as u16;
    // Agents panel takes min(agents+heading, 40% of the sidebar height).
    let agents_height = if agents_len == 0 {
        0
    } else {
        agents_len.min(height.saturating_mul(2) / 5)
    };
    let ws_height = height.saturating_sub(agents_height);
    let ws_len = model.workspaces.len();
    // Scroll each section so its selected row stays visible. When the selection
    // sits in the agents panel, hold the workspaces list at the bottom of its
    // range (its "last" offset) rather than snapping back to the top.
    let ws_offset = match selected {
        Some(index) if index < ws_len && ws_height > 0 => {
            index.saturating_sub(ws_height as usize - 1)
        }
        Some(index) if index >= ws_len && ws_height > 0 => {
            ws_len.saturating_sub(ws_height as usize)
        }
        _ => 0,
    };
    let agents_offset = match selected {
        Some(index) if index >= ws_len && agents_height > 0 => {
            (index - ws_len).saturating_sub(agents_height as usize - 1)
        }
        _ => 0,
    };
    SidebarLayout {
        ws_offset,
        ws_height,
        agents_y: ws_height,
        agents_height,
        agents_offset,
    }
}

/// Map a sidebar screen row (relative to the sidebar top) to a flat row index
/// and the row itself, skipping headings. Used by clicks and right-clicks.
pub fn sidebar_row_at<'a>(
    model: &'a SidebarModel,
    layout: &SidebarLayout,
    rel_row: u16,
) -> Option<(usize, &'a SidebarRow)> {
    if rel_row < layout.ws_height {
        let index = layout.ws_offset + rel_row as usize;
        let row = model.workspaces.get(index)?;
        row.target.as_ref()?;
        Some((index, row))
    } else {
        let within = rel_row.checked_sub(layout.agents_y)? as usize;
        if within >= layout.agents_height as usize {
            return None;
        }
        let index = layout.agents_offset + within;
        let row = model.agents.get(index)?;
        row.target.as_ref()?;
        Some((model.workspaces.len() + index, row))
    }
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

/// A workspace's swatch color: an explicit `#rrggbb`, or the auto-hashed
/// fallback keyed on its name (#19).
fn workspace_color(workspace: &WorkspaceInfo) -> Color {
    workspace
        .color
        .as_deref()
        .and_then(|hex| crate::config::parse_hex_color(hex).ok())
        .unwrap_or_else(|| auto_color(&workspace.name))
}

/// Deterministic swatch color for a name: FNV-1a hash picks from the theme ANSI
/// palette indices 1–6 and 9–14 (skipping black/white/grey) (#19).
pub fn auto_color(name: &str) -> Color {
    const PALETTE: [u8; 12] = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14];
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    Color::Indexed(PALETTE[(hash as usize) % PALETTE.len()])
}

fn render_sidebar(
    frame: &mut Frame,
    layout: &LayoutSnapshot,
    area: Rect,
    navigate: Option<usize>,
    collapsed: &HashSet<WorkspaceId>,
    agents_panel: bool,
    theme: &Theme,
) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.sidebar_bg)),
        area,
    );
    let model = sidebar_rows(layout, collapsed, agents_panel);
    let place = sidebar_layout(area.height, &model, navigate);
    // Workspaces list (scrolled to keep the selected row visible).
    for screen in 0..place.ws_height {
        let index = place.ws_offset + screen as usize;
        let Some(row) = model.workspaces.get(index) else {
            break;
        };
        let selected = navigate == Some(index);
        draw_sidebar_row(frame, area, area.y + screen, row, selected, theme);
    }
    // Agents panel, pinned below the workspaces list (scrolled to its selection).
    for screen in 0..place.agents_height {
        let index = place.agents_offset + screen as usize;
        let Some(row) = model.agents.get(index) else {
            break;
        };
        let flat = model.workspaces.len() + index;
        let selected = navigate == Some(flat);
        draw_sidebar_row(
            frame,
            area,
            area.y + place.agents_y + screen,
            row,
            selected,
            theme,
        );
    }
}

/// Draw one sidebar row at absolute screen `y`, with an optional leading color
/// swatch for workspace rows and the trailing state dot.
fn draw_sidebar_row(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    row: &SidebarRow,
    selected: bool,
    theme: &Theme,
) {
    if y >= area.y.saturating_add(area.height) {
        return;
    }
    let base = if selected {
        Style::default().fg(theme.sidebar_bg).bg(theme.accent)
    } else {
        let mut fg = state_color(theme, row.state);
        if row.kind == SidebarKind::Heading {
            fg = theme.dim;
        }
        let style = Style::default().fg(fg).bg(theme.sidebar_bg);
        if row.dim {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    };
    // A workspace row reserves a 1-cell swatch before its label. The cell is
    // always reserved (even when selected) so the label never shifts; on a
    // selected row the swatch sits under the accent background.
    if let Some(color) = row.color {
        let swatch = if selected {
            base
        } else {
            Style::default().fg(color).bg(theme.sidebar_bg)
        };
        frame.render_widget(
            Paragraph::new("█").style(swatch),
            Rect::new(area.x, y, 1, 1),
        );
        let inner = Rect::new(area.x + 1, y, area.width.saturating_sub(1), 1);
        frame.render_widget(
            Paragraph::new(sidebar_row_text(row, inner.width as usize)).style(base),
            inner,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(sidebar_row_text(row, area.width as usize)).style(base),
        Rect::new(area.x, y, area.width, 1),
    );
}

/// Render a row's label plus its trailing state dot into `width` columns,
/// reserving the dot's cell so truncation drops label text, never the dot (#19).
fn sidebar_row_text(row: &SidebarRow, width: usize) -> String {
    let dot = sidebar_dot(row.state);
    // Leading space + the (single-column) dot.
    let reserved = dot.chars().count() + 1;
    if width <= reserved {
        return truncate_ellipsis(&format!("{} {}", row.label, dot), width);
    }
    let label = truncate_ellipsis(&row.label, width - reserved);
    format!("{label} {dot}")
}

/// One state dot per workspace, colored by rollup state; the active workspace's
/// dot draws on the accent. Row `i` (from the sidebar top) is workspace `i`.
pub fn render_sidebar_rail(frame: &mut Frame, layout: &LayoutSnapshot, area: Rect, theme: &Theme) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.sidebar_bg)),
        area,
    );
    for (index, workspace) in layout.workspaces.iter().enumerate() {
        let y = area.y + index as u16;
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        let style = if workspace.active {
            Style::default().fg(theme.sidebar_bg).bg(theme.accent)
        } else {
            Style::default()
                .fg(state_color(theme, workspace.state))
                .bg(theme.sidebar_bg)
        };
        frame.render_widget(
            Paragraph::new(format!(" {} ", sidebar_dot(workspace.state))).style(style),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

/// Hit-test the compact rail: a click on row `rel_row` (from the sidebar top)
/// selects that workspace, if one is drawn there.
pub fn rail_workspace_at(layout: &LayoutSnapshot, rel_row: u16) -> Option<WorkspaceId> {
    layout
        .workspaces
        .get(rel_row as usize)
        .map(|workspace| workspace.id)
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
            MenuAction::Help => "Help…",
            MenuAction::Color => "Color…",
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
    use std::collections::HashSet;

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
                    color: None,
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
                    color: None,
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

    fn no_collapse() -> HashSet<WorkspaceId> {
        HashSet::new()
    }

    #[test]
    fn sidebar_rows_expand_workspaces_and_hit_test_targets() {
        let model = sidebar_rows(&snapshot(), &no_collapse(), false);
        // heading, active ws, its tab, its pane, other ws + tab (also expanded).
        assert_eq!(model.workspaces.len(), 6);
        assert_eq!(model.workspaces[0].kind, SidebarKind::Heading);
        assert_eq!(model.workspaces[0].target, None);
        assert_eq!(model.workspaces[1].label, "▾ active");
        assert_eq!(
            model.workspaces[2].target,
            Some(SidebarTarget::Tab(TabId(2)))
        );
        assert_eq!(
            model.workspaces[3].target,
            Some(SidebarTarget::Pane(PaneId(3)))
        );
        // Blocked agent rows carry a compact age label.
        assert_eq!(model.workspaces[3].label, "    Codex 4m");
        assert_eq!(model.workspaces[4].label, "▾ other");
        // Non-active workspace children are dimmer.
        assert!(model.workspaces[5].dim);

        let place = sidebar_layout(24, &model, None);
        // Row 0 is the `workspaces` heading and is not a hit.
        assert_eq!(sidebar_row_at(&model, &place, 0), None);
        assert_eq!(
            sidebar_row_at(&model, &place, 1).map(|(_, row)| row.target.clone()),
            Some(Some(SidebarTarget::Workspace(WorkspaceId(1))))
        );
        assert_eq!(
            sidebar_row_at(&model, &place, 3).map(|(_, row)| row.target.clone()),
            Some(Some(SidebarTarget::Pane(PaneId(3))))
        );
    }

    #[test]
    fn collapsed_workspace_hides_its_children() {
        let mut collapsed = HashSet::new();
        collapsed.insert(WorkspaceId(1));
        let model = sidebar_rows(&snapshot(), &collapsed, false);
        // heading, collapsed active ws (caret ▸, no children), other ws + tab.
        assert_eq!(model.workspaces[1].label, "▸ active");
        assert_eq!(
            model.workspaces[2].target,
            Some(SidebarTarget::Workspace(WorkspaceId(4)))
        );
    }

    #[test]
    fn agents_panel_sorts_by_urgency() {
        let mut layout = snapshot();
        // Give the second workspace a working agent so the panel has two entries.
        layout.workspaces[1].tabs[0].agents = vec![AgentInfo {
            pane: PaneId(9),
            name: "Claude".into(),
            state: AgentStateKind::Working,
            state_age_secs: 30,
        }];
        let model = sidebar_rows(&layout, &no_collapse(), true);
        assert_eq!(model.agents[0].kind, SidebarKind::Heading);
        // Blocked outranks working.
        assert_eq!(model.agents[1].target, Some(SidebarTarget::Pane(PaneId(3))));
        assert_eq!(model.agents[2].target, Some(SidebarTarget::Pane(PaneId(9))));
    }

    #[test]
    fn rail_hit_test_maps_rows_to_workspaces() {
        let layout = snapshot();
        assert_eq!(rail_workspace_at(&layout, 0), Some(WorkspaceId(1)));
        assert_eq!(rail_workspace_at(&layout, 1), Some(WorkspaceId(4)));
        assert_eq!(rail_workspace_at(&layout, 2), None);
    }

    #[test]
    fn agents_panel_takes_at_most_40_percent_and_workspaces_scroll() {
        let mut layout = snapshot();
        // Enough agents that the panel wants more than 40% of a short sidebar.
        layout.workspaces[1].tabs[0].agents = (0..10)
            .map(|i| AgentInfo {
                pane: PaneId(100 + i),
                name: format!("a{i}"),
                state: AgentStateKind::Working,
                state_age_secs: i,
            })
            .collect();
        let model = sidebar_rows(&layout, &no_collapse(), true);
        let place = sidebar_layout(20, &model, None);
        // 40% of 20 = 8 rows for the agents panel; the rest is the list.
        assert_eq!(place.agents_height, 8);
        assert_eq!(place.ws_height, 12);
        // Selecting a workspace row past the visible list scrolls it into view.
        let last_ws = model.workspaces.len() - 1;
        let deep = sidebar_layout(6, &model, Some(last_ws));
        assert_eq!(deep.ws_offset, last_ws + 1 - deep.ws_height as usize);
    }

    #[test]
    fn agents_panel_scrolls_and_hit_tests_its_selection() {
        let mut layout = snapshot();
        // Ten agents in the second workspace so the panel overflows.
        layout.workspaces[1].tabs[0].agents = (0..10)
            .map(|i| AgentInfo {
                pane: PaneId(100 + i),
                name: format!("a{i}"),
                state: AgentStateKind::Working,
                state_age_secs: i,
            })
            .collect();
        let model = sidebar_rows(&layout, &no_collapse(), true);
        let ws_len = model.workspaces.len();
        // Select the last agent row (flat index past the panel height).
        let last_agent = ws_len + model.agents.len() - 1;
        let place = sidebar_layout(20, &model, Some(last_agent));
        assert_eq!(place.agents_height, 8);
        // The agents section scrolled so the selected row is the last drawn one.
        assert_eq!(
            place.agents_offset,
            model.agents.len() - place.agents_height as usize
        );
        // The workspaces list holds at the bottom of its range, not snapped to 0.
        assert_eq!(place.ws_offset, ws_len - place.ws_height as usize);
        // The selected agent row is drawn on the panel's last screen line and is
        // hit-testable there (it was previously off-panel and unreachable).
        let last_screen = place.agents_y + place.agents_height - 1;
        let (flat, row) = sidebar_row_at(&model, &place, last_screen).expect("row drawn");
        assert_eq!(flat, last_agent);
        assert_eq!(row.target, model.agents.last().unwrap().target);
    }

    #[test]
    fn sidebar_row_text_reserves_the_state_dot() {
        let row = SidebarRow {
            kind: SidebarKind::Pane,
            target: Some(SidebarTarget::Pane(PaneId(1))),
            label: "a-very-long-agent-name-here".into(),
            state: AgentStateKind::Blocked,
            dim: false,
            color: None,
        };
        let dot = sidebar_dot(AgentStateKind::Blocked);
        let text = sidebar_row_text(&row, 12);
        // Fits the width and always ends with the state dot (never truncated off).
        assert_eq!(text.chars().count(), 12);
        assert!(text.ends_with(dot), "{text:?} should keep the dot");
        assert!(text.contains('…'), "long label should be ellipsized");
    }

    #[test]
    fn auto_color_is_stable_per_name() {
        assert_eq!(auto_color("kodade-cli"), auto_color("kodade-cli"));
        // Every color comes from the 1–6 / 9–14 palette bands.
        for name in ["a", "workspace", "agents", "main", "x"] {
            match auto_color(name) {
                Color::Indexed(i) => assert!((1..=6).contains(&i) || (9..=14).contains(&i)),
                other => panic!("unexpected color {other:?}"),
            }
        }
    }

    #[test]
    fn workspace_row_appends_a_differing_root_basename() {
        let mut layout = snapshot();
        layout.workspaces[0].name = "app".into();
        layout.workspaces[0].root = Some("/Users/keith/src/webapp".into());
        let model = sidebar_rows(&layout, &no_collapse(), false);
        assert_eq!(model.workspaces[1].label, "▾ app  webapp");
        // A basename equal to the name is not repeated.
        layout.workspaces[0].root = Some("/Users/keith/src/app".into());
        assert_eq!(
            sidebar_rows(&layout, &no_collapse(), false).workspaces[1].label,
            "▾ app"
        );
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
        let collapsed = no_collapse();
        let ui = Ui {
            sidebar_mode: SidebarMode::Hidden,
            sidebar_width: 1,
            collapsed: &collapsed,
            agents_panel: true,
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
            prefix_hint: "",
            first_attach_hint: None,
            help: None,
            picker: None,
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
        let config = Config::default();
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            content_area(area, SidebarMode::Full, &config),
            Rect::new(config.sidebar_width, 0, 80 - config.sidebar_width, 24)
        );
        assert_eq!(
            content_area(area, SidebarMode::Hidden, &config),
            Rect::new(1, 0, 79, 24)
        );
        // Compact is a 3-column rail.
        assert_eq!(sidebar_width(SidebarMode::Compact, &config), 3);
        // #24: content width + sidebar/gutter width must cover the whole row so
        // `pane_cols` never drifts by the collapsed 1-col gutter.
        for mode in [SidebarMode::Full, SidebarMode::Compact, SidebarMode::Hidden] {
            assert_eq!(
                content_area(area, mode, &config).width + sidebar_width(mode, &config),
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
        let config = Config::default();
        let width = sidebar_width(SidebarMode::Full, &config);
        let collapsed = no_collapse();
        let ui = Ui {
            sidebar_mode: SidebarMode::Full,
            sidebar_width: width,
            collapsed: &collapsed,
            agents_panel: false,
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
            prefix_hint: "",
            first_attach_hint: None,
            help: None,
            picker: None,
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
        assert_eq!(buffer[(width, 1)].fg, theme.blocked);
        // Tab bar row (y=0) carries the blocked tab label.
        assert!(has_blocked_fg(0, width..80));
        // Sidebar workspace row (y=1, below the heading) is tinted blocked.
        assert!(has_blocked_fg(1, 0..width));
    }
}
