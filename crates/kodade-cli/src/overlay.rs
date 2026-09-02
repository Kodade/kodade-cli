//! The shared centered-overlay widget.
//!
//! One list box with a title, an optional filter line, scrollable rows, and a
//! selection. The settings menu (#20) uses it today; the help overlay (#6) and
//! the workspace / goto pickers (#17) build on the same struct. Everything here
//! is pure so it can be unit-tested without a terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kodade_cli_proto::{PaneId, TabId, WorkspaceId};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::config::Theme;

/// What a row activates. `Index` points back into the owner's own list; the
/// layout variants are for the help overlay (#6) and the pickers (#17).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OverlayTarget {
    None,
    Index(usize),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayRow {
    pub label: String,
    /// Right-aligned dim text: a value, a key chord, or a state.
    pub hint: String,
    pub target: OverlayTarget,
}

impl OverlayRow {
    pub fn new(label: impl Into<String>, hint: impl Into<String>, target: OverlayTarget) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
            target,
        }
    }
}

/// The overlay carries no scroll state: the visible window is derived from
/// `selected` at render time (`window_start`), so key handling stays pure and
/// the selection is always on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub title: String,
    /// `Some` turns the overlay into a filter box; typed characters append.
    pub filter: Option<String>,
    pub rows: Vec<OverlayRow>,
    pub selected: usize,
}

impl Overlay {
    pub fn new(title: impl Into<String>, rows: Vec<OverlayRow>) -> Self {
        Self {
            title: title.into(),
            filter: None,
            rows,
            selected: 0,
        }
    }

    /// Currently highlighted row, if any.
    pub fn current(&self) -> Option<&OverlayRow> {
        self.rows.get(self.selected)
    }

    // Moves the selection, clamped to the row list.
    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = match delta {
            d if d < 0 => self.selected.saturating_sub(d.unsigned_abs()),
            d => (self.selected + d as usize).min(last),
        };
    }
}

/// Result of feeding a key to the overlay; the owner decides what `Select`
/// and `Filtered` mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayEvent {
    Select,
    Cancel,
    Moved,
    Filtered,
    None,
}

/// Handles one key for an overlay. Movement works with arrows and ctrl+n /
/// ctrl+p always; `j` / `k` only when the overlay has no filter, so filter
/// boxes stay typable.
pub fn overlay_key(overlay: &mut Overlay, key: KeyEvent) -> OverlayEvent {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let filtering = overlay.filter.is_some();
    match key.code {
        KeyCode::Esc => OverlayEvent::Cancel,
        KeyCode::Enter => OverlayEvent::Select,
        KeyCode::Up => {
            overlay.move_by(-1);
            OverlayEvent::Moved
        }
        KeyCode::Down => {
            overlay.move_by(1);
            OverlayEvent::Moved
        }
        KeyCode::Char('p') if ctrl => {
            overlay.move_by(-1);
            OverlayEvent::Moved
        }
        KeyCode::Char('n') if ctrl => {
            overlay.move_by(1);
            OverlayEvent::Moved
        }
        KeyCode::Char('k') if !filtering => {
            overlay.move_by(-1);
            OverlayEvent::Moved
        }
        KeyCode::Char('j') if !filtering => {
            overlay.move_by(1);
            OverlayEvent::Moved
        }
        KeyCode::Char('q') if !filtering && !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            OverlayEvent::Cancel
        }
        KeyCode::Backspace => match &mut overlay.filter {
            Some(filter) => {
                filter.pop();
                OverlayEvent::Filtered
            }
            None => OverlayEvent::None,
        },
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            match &mut overlay.filter {
                Some(filter) => {
                    filter.push(c);
                    OverlayEvent::Filtered
                }
                None => OverlayEvent::None,
            }
        }
        _ => OverlayEvent::None,
    }
}

/// The centered box the overlay occupies inside `area`.
pub fn overlay_rect(area: Rect, overlay: &Overlay) -> Rect {
    let rows = overlay.rows.len() as u16;
    let chrome = if overlay.filter.is_some() { 4 } else { 3 };
    let width = area.width.saturating_sub(4).clamp(8, 72);
    let height = area
        .height
        .saturating_sub(2)
        .min(rows.saturating_add(chrome))
        .max(3);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

// The row list inside the border, below the filter line when there is one.
fn list_area(rect: Rect, overlay: &Overlay) -> Rect {
    let filter = u16::from(overlay.filter.is_some());
    Rect::new(
        rect.x + 1,
        rect.y + 1 + filter,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2 + filter),
    )
}

/// First visible row for a viewport `height`; keeps `selected` on screen.
pub fn window_start(selected: usize, height: usize) -> usize {
    if height == 0 || selected < height {
        0
    } else {
        selected + 1 - height
    }
}

/// Row index under a click, or `None` when the click misses the row list.
pub fn row_at(area: Rect, overlay: &Overlay, column: u16, row: u16) -> Option<usize> {
    if !contains(area, overlay, column, row) {
        return None;
    }
    let list = list_area(overlay_rect(area, overlay), overlay);
    if row < list.y || row >= list.y + list.height {
        return None;
    }
    let offset = (row - list.y) as usize;
    let index = window_start(overlay.selected, list.height as usize) + offset;
    (index < overlay.rows.len()).then_some(index)
}

/// True when a click lands inside the overlay box (border included).
pub fn contains(area: Rect, overlay: &Overlay, column: u16, row: u16) -> bool {
    let rect = overlay_rect(area, overlay);
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Draws the overlay centered in `area`.
pub fn render_overlay(frame: &mut Frame, area: Rect, overlay: &Overlay, theme: &Theme) {
    let rect = overlay_rect(area, overlay);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", overlay.title))
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().fg(theme.menu_fg).bg(theme.menu_bg)),
        rect,
    );
    let inner_width = rect.width.saturating_sub(2);
    if inner_width == 0 || rect.height <= 2 {
        return;
    }
    if let Some(filter) = &overlay.filter {
        frame.render_widget(
            Paragraph::new(format!("> {filter}")).style(Style::default().fg(theme.accent)),
            Rect::new(rect.x + 1, rect.y + 1, inner_width, 1),
        );
    }
    let list = list_area(rect, overlay);
    let height = list.height as usize;
    if height == 0 {
        return;
    }
    let start = window_start(overlay.selected, height);
    for (offset, row) in overlay.rows.iter().skip(start).take(height).enumerate() {
        let index = start + offset;
        let style = if index == overlay.selected {
            // Selected row is inverted against the menu colors.
            Style::default().fg(theme.menu_bg).bg(theme.accent)
        } else {
            Style::default().fg(theme.menu_fg).bg(theme.menu_bg)
        };
        // Widths are display cells, so wide (CJK) labels cannot overflow the border.
        let (hint, hint_width) = clip(&row.hint, list.width as usize);
        let (label, label_width) = clip(&row.label, list.width as usize - hint_width);
        let pad = list.width as usize - hint_width - label_width;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(label),
                Span::raw(" ".repeat(pad)),
                Span::raw(hint),
            ]))
            .style(style),
            Rect::new(list.x, list.y + offset as u16, list.width, 1),
        );
    }
}

/// Additive picker renderer (#17): like `render_overlay`, but each row leads
/// with a state-colored dot and draws its hint dimmed. `dot(index)` returns the
/// glyph and color for the row at `index` in `overlay.rows`; a selected row is
/// inverted whole so it stays legible. This does not change `render_overlay`.
pub fn render_picker(
    frame: &mut Frame,
    area: Rect,
    overlay: &Overlay,
    theme: &Theme,
    dot: impl Fn(usize) -> (&'static str, Color),
) {
    let rect = overlay_rect(area, overlay);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", overlay.title))
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().fg(theme.menu_fg).bg(theme.menu_bg)),
        rect,
    );
    let inner_width = rect.width.saturating_sub(2);
    if inner_width == 0 || rect.height <= 2 {
        return;
    }
    if let Some(filter) = &overlay.filter {
        frame.render_widget(
            Paragraph::new(format!("> {filter}")).style(Style::default().fg(theme.accent)),
            Rect::new(rect.x + 1, rect.y + 1, inner_width, 1),
        );
    }
    let list = list_area(rect, overlay);
    let height = list.height as usize;
    if height == 0 {
        return;
    }
    let start = window_start(overlay.selected, height);
    for (offset, row) in overlay.rows.iter().skip(start).take(height).enumerate() {
        let index = start + offset;
        let selected = index == overlay.selected;
        let (glyph, dot_color) = dot(index);
        let dot_width = Span::raw(glyph).width();
        // Reserve the dot's cells first, then split the rest between label and hint.
        let avail = (list.width as usize).saturating_sub(dot_width);
        let (hint, hint_width) = clip(&row.hint, avail);
        let (label, label_width) = clip(&row.label, avail - hint_width);
        let pad = avail - hint_width - label_width;
        let (label_style, hint_style, dot_style) = if selected {
            let inverted = Style::default().fg(theme.menu_bg).bg(theme.accent);
            (inverted, inverted, inverted)
        } else {
            (
                Style::default().fg(theme.menu_fg).bg(theme.menu_bg),
                Style::default().fg(theme.dim).bg(theme.menu_bg),
                Style::default().fg(dot_color).bg(theme.menu_bg),
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(glyph, dot_style),
                Span::styled(label, label_style),
                Span::styled(" ".repeat(pad), label_style),
                Span::styled(hint, hint_style),
            ])),
            Rect::new(list.x, list.y + offset as u16, list.width, 1),
        );
    }
}

// Truncates to a display width, returning the text and the cells it uses.
fn clip(text: &str, width: usize) -> (String, usize) {
    let mut clipped = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cells = Span::raw(character.to_string()).width();
        if used + cells > width {
            break;
        }
        clipped.push(character);
        used += cells;
    }
    (clipped, used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn overlay() -> Overlay {
        Overlay::new(
            "settings",
            (0..5)
                .map(|index| {
                    OverlayRow::new(format!("row {index}"), "on", OverlayTarget::Index(index))
                })
                .collect(),
        )
    }

    #[test]
    fn movement_clamps_and_selects() {
        let mut overlay = overlay();
        assert_eq!(
            overlay_key(&mut overlay, KeyEvent::from(KeyCode::Up)),
            OverlayEvent::Moved
        );
        assert_eq!(overlay.selected, 0);
        for _ in 0..10 {
            overlay_key(&mut overlay, KeyEvent::from(KeyCode::Down));
        }
        assert_eq!(overlay.selected, 4);
        assert_eq!(
            overlay_key(&mut overlay, KeyEvent::from(KeyCode::Enter)),
            OverlayEvent::Select
        );
        assert_eq!(overlay.current().unwrap().target, OverlayTarget::Index(4));
        assert_eq!(
            overlay_key(&mut overlay, KeyEvent::from(KeyCode::Esc)),
            OverlayEvent::Cancel
        );
    }

    #[test]
    fn vi_keys_move_only_without_a_filter() {
        let mut overlay = overlay();
        overlay_key(&mut overlay, KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(overlay.selected, 1);
        assert_eq!(
            overlay_key(&mut overlay, KeyEvent::from(KeyCode::Char('q'))),
            OverlayEvent::Cancel
        );

        overlay.filter = Some(String::new());
        assert_eq!(
            overlay_key(&mut overlay, KeyEvent::from(KeyCode::Char('j'))),
            OverlayEvent::Filtered
        );
        assert_eq!(overlay.selected, 1);
        assert_eq!(overlay.filter.as_deref(), Some("j"));
        overlay_key(&mut overlay, KeyEvent::from(KeyCode::Char('q')));
        assert_eq!(overlay.filter.as_deref(), Some("jq"));
        assert_eq!(
            overlay_key(&mut overlay, KeyEvent::from(KeyCode::Backspace)),
            OverlayEvent::Filtered
        );
        assert_eq!(overlay.filter.as_deref(), Some("j"));
        // ctrl+n / ctrl+p keep working while filtering.
        assert_eq!(
            overlay_key(
                &mut overlay,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)
            ),
            OverlayEvent::Moved
        );
        assert_eq!(overlay.selected, 2);
    }

    #[test]
    fn window_follows_the_selection_and_clicks_hit_rows() {
        let mut overlay = Overlay::new(
            "long",
            (0..20)
                .map(|index| {
                    OverlayRow::new(format!("row {index}"), "", OverlayTarget::Index(index))
                })
                .collect(),
        );
        assert_eq!(window_start(0, 5), 0);
        assert_eq!(window_start(4, 5), 0);
        assert_eq!(window_start(9, 5), 5);
        // A click maps to the row drawn under it, scrolled window included.
        let area = Rect::new(0, 0, 40, 12);
        overlay.selected = 19;
        let rect = overlay_rect(area, &overlay);
        let list = list_area(rect, &overlay);
        let first = row_at(area, &overlay, rect.x + 2, list.y).expect("row under the click");
        assert_eq!(first, window_start(19, list.height as usize));
        // Outside the box, and on the border, there is no row.
        assert!(row_at(area, &overlay, 0, 0).is_none());
        assert!(row_at(area, &overlay, rect.x, rect.y).is_none());
        assert!(contains(area, &overlay, rect.x, rect.y));
        assert!(!contains(area, &overlay, 0, 0));
    }

    #[test]
    fn wide_characters_stay_inside_the_border() {
        let theme = Theme::kodade_dark();
        let overlay = Overlay::new(
            "テーマ",
            vec![OverlayRow::new(
                " 日本語のとても長いラベルです日本語のとても長いラベルです",
                "オン ",
                OverlayTarget::Index(0),
            )],
        );
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).expect("test terminal");
        terminal
            .draw(|frame| render_overlay(frame, frame.area(), &overlay, &theme))
            .expect("frame renders");
        let buffer = terminal.backend().buffer();
        let rect = overlay_rect(Rect::new(0, 0, 30, 8), &overlay);
        // The right border survives: the label was clipped by display width.
        let right = rect.x + rect.width - 1;
        let list = list_area(rect, &overlay);
        assert_eq!(buffer[(right, list.y)].symbol(), "│");
    }

    #[test]
    fn renders_a_centered_box_with_the_selection_visible() {
        let theme = Theme::kodade_dark();
        let overlay = {
            let mut overlay = overlay();
            overlay.selected = 4;
            overlay
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("test terminal");
        terminal
            .draw(|frame| render_overlay(frame, frame.area(), &overlay, &theme))
            .expect("frame renders");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("settings"));
        assert!(text.contains("row 4"));
        // The box is centered: the first row stays empty.
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), " ");
    }
}
