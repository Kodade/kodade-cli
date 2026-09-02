//! Mouse text selection, link detection, and mouse passthrough encoding (#12).
//!
//! Everything here is pure: `App` owns the live `Selection` and feeds it mouse
//! coordinates that are already pane-relative (border subtracted), `render`
//! asks `contains` which cells to paint with `theme.selection`.
//!
//! Coordinates are `(row, col)` in the pane's own grid, char-indexed against
//! `Screen::contents` the same way keyboard copy mode is (`mode::selected_text`).

use crossterm::event::{MouseButton, MouseEventKind};
use kodade_cli_proto::{PaneId, Screen};

/// Characters that make up a "word" for double-click and link extraction.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '~' | '-')
}

/// What a drag selects: a character range, whole words, or whole lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Char,
    Word,
    Line,
}

/// A live mouse selection in one pane.
///
/// `anchor` is where the press landed and `head` follows the pointer; either
/// may come first. `range` caches the mode-expanded, ordered bounds so the
/// renderer can hit-test a cell without re-scanning the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub pane: PaneId,
    pub anchor: (usize, usize),
    pub head: (usize, usize),
    pub mode: SelectionMode,
    /// Ordered `(start, end)`, end exclusive, after word/line expansion.
    range: ((usize, usize), (usize, usize)),
}

impl Selection {
    /// Starts a selection at one cell.
    pub fn new(pane: PaneId, at: (usize, usize), mode: SelectionMode, screen: &Screen) -> Self {
        let mut selection = Self {
            pane,
            anchor: at,
            head: at,
            mode,
            range: (at, at),
        };
        selection.resolve(screen);
        selection
    }

    /// Moves the drag end and re-expands the range.
    pub fn set_head(&mut self, at: (usize, usize), screen: &Screen) {
        self.head = at;
        self.resolve(screen);
    }

    /// Ordered anchor/head, before mode expansion.
    pub fn normalize(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// True when the user pressed and released on the same cell without
    /// dragging — that is a plain click, not a selection.
    pub fn is_click(&self) -> bool {
        self.mode == SelectionMode::Char && self.anchor == self.head
    }

    /// Whether a pane cell falls inside the selection (for highlighting).
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let (start, end) = self.range;
        (row, col) >= start && (row, col) < end
    }

    /// The selected text, joined with newlines like `mode::selected_text`.
    pub fn text(&self, screen: &Screen) -> String {
        let lines = crate::mode::grid(&screen.contents);
        let ((start_row, start_col), (end_row, end_col)) = self.range;
        let last = lines.len().saturating_sub(1);
        (start_row.min(last)..=end_row.min(last))
            .map(|row| {
                let line = lines[row];
                let len = line.chars().count();
                let from = if row == start_row { start_col } else { 0 };
                let to = if row == end_row {
                    end_col.min(len)
                } else {
                    len
                };
                line.chars()
                    .skip(from)
                    .take(to.saturating_sub(from))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Recomputes the cached range: order the ends, then widen by mode. The end
    // column is exclusive, so char mode includes the cell under the pointer.
    fn resolve(&mut self, screen: &Screen) {
        let lines = crate::mode::grid(&screen.contents);
        let line_len = |row: usize| lines.get(row).map_or(0, |line| line.chars().count());
        let ((start_row, start_col), (end_row, end_col)) = self.normalize();
        self.range = match self.mode {
            SelectionMode::Char => (
                (start_row, start_col.min(line_len(start_row))),
                (end_row, (end_col + 1).min(line_len(end_row))),
            ),
            SelectionMode::Word => (
                (start_row, word_start(lines.get(start_row), start_col)),
                (end_row, word_end(lines.get(end_row), end_col)),
            ),
            SelectionMode::Line => ((start_row, 0), (end_row, line_len(end_row))),
        };
    }
}

/// First column of the word containing `col`, or `col` when it is not a word cell.
fn word_start(line: Option<&&str>, col: usize) -> usize {
    let Some(chars) = line.map(|line| line.chars().collect::<Vec<_>>()) else {
        return col;
    };
    if !chars.get(col).copied().is_some_and(is_word) {
        return col;
    }
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    start
}

/// One past the last column of the word containing `col`.
fn word_end(line: Option<&&str>, col: usize) -> usize {
    let Some(chars) = line.map(|line| line.chars().collect::<Vec<_>>()) else {
        return col + 1;
    };
    if !chars.get(col).copied().is_some_and(is_word) {
        return (col + 1).min(chars.len());
    }
    let mut end = col;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    end
}

/// Extracts the `http(s)://` token around a cell: widen to whitespace on both
/// sides, then trim the brackets and punctuation people wrap URLs in.
pub fn link_at(screen: &Screen, row: usize, col: usize) -> Option<String> {
    let lines = crate::mode::grid(&screen.contents);
    let chars = lines.get(row)?.chars().collect::<Vec<_>>();
    if col >= chars.len() {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    let token = chars[start..end]
        .iter()
        .collect::<String>()
        .trim_start_matches(['(', '<', '"', '\''])
        .trim_end_matches([')', '>', '.', ',', ';', '"', '\''])
        .to_string();
    (token.starts_with("http://") || token.starts_with("https://")).then_some(token)
}

/// Encodes a mouse event as SGR (1006) bytes for a pane app that turned mouse
/// reporting on. `col`/`row` are pane-relative and zero-based here; SGR is
/// one-based, so both get a `+1`.
///
/// Button numbers: 0 left, 1 middle, 2 right, +32 while dragging, 64/65 wheel.
/// Modifier bits: 4 shift, 8 alt, 16 ctrl.
pub fn sgr_mouse(
    kind: MouseEventKind,
    modifiers: crossterm::event::KeyModifiers,
    col: u16,
    row: u16,
) -> Vec<u8> {
    use crossterm::event::KeyModifiers;
    let button = |button: MouseButton| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut code, release) = match kind {
        MouseEventKind::Down(pressed) => (button(pressed), false),
        MouseEventKind::Up(pressed) => (button(pressed), true),
        MouseEventKind::Drag(pressed) => (button(pressed) + 32, false),
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let final_byte = if release { 'm' } else { 'M' };
    format!("\x1b[<{code};{};{}{final_byte}", col + 1, row + 1).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(contents: &str) -> Screen {
        Screen {
            contents: contents.into(),
            ..Screen::default()
        }
    }

    #[test]
    fn char_selection_spans_lines_and_includes_the_head_cell() {
        let screen = screen("hello world\nsecond line");
        let mut selection = Selection::new(PaneId(1), (0, 6), SelectionMode::Char, &screen);
        selection.set_head((1, 2), &screen);
        assert_eq!(selection.text(&screen), "world\nsec");
        assert!(selection.contains(0, 6));
        assert!(!selection.contains(0, 5));
        assert!(selection.contains(1, 2));
        assert!(!selection.contains(1, 3));
    }

    #[test]
    fn reversed_drag_selects_the_same_text() {
        let screen = screen("hello world\nsecond line");
        let mut forward = Selection::new(PaneId(1), (0, 6), SelectionMode::Char, &screen);
        forward.set_head((1, 2), &screen);
        let mut backward = Selection::new(PaneId(1), (1, 2), SelectionMode::Char, &screen);
        backward.set_head((0, 6), &screen);
        assert_eq!(forward.text(&screen), backward.text(&screen));
    }

    #[test]
    fn word_selection_expands_both_ends() {
        let screen = screen("run ./build.sh now");
        // Double-click in the middle of `./build.sh` grabs the whole token.
        let selection = Selection::new(PaneId(1), (0, 7), SelectionMode::Word, &screen);
        assert_eq!(selection.text(&screen), "./build.sh");
        // Dragging on to the next word keeps whole words at both ends.
        let mut dragged = selection.clone();
        dragged.set_head((0, 16), &screen);
        assert_eq!(dragged.text(&screen), "./build.sh now");
        // A click on whitespace selects just that cell.
        let space = Selection::new(PaneId(1), (0, 3), SelectionMode::Word, &screen);
        assert_eq!(space.text(&screen), " ");
    }

    #[test]
    fn line_selection_takes_whole_lines() {
        let screen = screen("first\nsecond\nthird");
        let mut selection = Selection::new(PaneId(1), (0, 3), SelectionMode::Line, &screen);
        assert_eq!(selection.text(&screen), "first");
        selection.set_head((1, 1), &screen);
        assert_eq!(selection.text(&screen), "first\nsecond");
        assert!(selection.contains(1, 5));
        assert!(!selection.contains(2, 0));
    }

    #[test]
    fn a_press_and_release_on_one_cell_is_a_click() {
        let screen = screen("abc");
        let mut selection = Selection::new(PaneId(1), (0, 1), SelectionMode::Char, &screen);
        assert!(selection.is_click());
        selection.set_head((0, 2), &screen);
        assert!(!selection.is_click());
    }

    #[test]
    fn links_are_extracted_and_trimmed() {
        let screen = screen("see (https://example.com/a_b). done\nno link here");
        assert_eq!(
            link_at(&screen, 0, 12).as_deref(),
            Some("https://example.com/a_b")
        );
        assert_eq!(link_at(&screen, 1, 3), None);
        assert_eq!(link_at(&screen, 0, 200), None);
    }

    #[test]
    fn sgr_encoding_matches_the_1006_protocol() {
        use crossterm::event::KeyModifiers;
        assert_eq!(
            sgr_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                0,
                0
            ),
            b"\x1b[<0;1;1M".to_vec()
        );
        assert_eq!(
            sgr_mouse(
                MouseEventKind::Up(MouseButton::Right),
                KeyModifiers::NONE,
                4,
                9
            ),
            b"\x1b[<2;5;10m".to_vec()
        );
        assert_eq!(
            sgr_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                KeyModifiers::CONTROL,
                1,
                1
            ),
            b"\x1b[<48;2;2M".to_vec()
        );
        assert_eq!(
            sgr_mouse(MouseEventKind::ScrollUp, KeyModifiers::NONE, 2, 3),
            b"\x1b[<64;3;4M".to_vec()
        );
    }
}
