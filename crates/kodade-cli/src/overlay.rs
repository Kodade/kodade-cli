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
    style::Style,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub title: String,
    /// `Some` turns the overlay into a filter box; typed characters append.
    pub filter: Option<String>,
    pub rows: Vec<OverlayRow>,
    pub selected: usize,
    pub scroll: usize,
}

impl Overlay {
    pub fn new(title: impl Into<String>, rows: Vec<OverlayRow>) -> Self {
        Self {
            title: title.into(),
            filter: None,
            rows,
            selected: 0,
            scroll: 0,
        }
    }

    /// Currently highlighted row, if any.
    pub fn current(&self) -> Option<&OverlayRow> {
        self.rows.get(self.selected)
    }

    // Moves the selection and keeps `scroll` following it.
    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = match delta {
            d if d < 0 => self.selected.saturating_sub(d.unsigned_abs()),
            d => (self.selected + d as usize).min(last),
        };
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
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
        KeyCode::Char('q') if !filtering => OverlayEvent::Cancel,
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

/// Draws the overlay centered in `area`.
pub fn render_overlay(frame: &mut Frame, area: Rect, overlay: &Overlay, theme: &Theme) {
    let rows = overlay.rows.len() as u16;
    let width = area.width.saturating_sub(4).clamp(8, 72);
    let height = area
        .height
        .saturating_sub(2)
        .min(rows.saturating_add(4))
        .max(3);

    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", overlay.title))
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().fg(theme.menu_fg).bg(theme.menu_bg)),
        rect,
    );
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut y = inner.y;
    if let Some(filter) = &overlay.filter {
        frame.render_widget(
            Paragraph::new(format!("> {filter}")).style(Style::default().fg(theme.accent)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
    let list_height = inner.height.saturating_sub(y - inner.y) as usize;
    if list_height == 0 {
        return;
    }
    // `scroll` is a hint; the window always keeps the selection visible.
    let mut start = overlay.scroll.min(overlay.selected);
    if overlay.selected >= start + list_height {
        start = overlay.selected + 1 - list_height;
    }
    for (offset, row) in overlay
        .rows
        .iter()
        .skip(start)
        .take(list_height)
        .enumerate()
    {
        let index = start + offset;
        let style = if index == overlay.selected {
            // Selected row is inverted against the menu colors.
            Style::default().fg(theme.menu_bg).bg(theme.accent)
        } else {
            Style::default().fg(theme.menu_fg).bg(theme.menu_bg)
        };
        let hint_width = row.hint.chars().count().min(inner.width as usize);
        let label_width = inner.width as usize - hint_width;
        let label = clip(&row.label, label_width);
        let pad = label_width - label.chars().count();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(label),
                Span::raw(" ".repeat(pad)),
                Span::raw(clip(&row.hint, hint_width)),
            ]))
            .style(style),
            Rect::new(
                inner.x,
                inner.y + (y - inner.y) + offset as u16,
                inner.width,
                1,
            ),
        );
    }
}

// Truncates to a cell width without panicking on multi-byte characters.
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
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
