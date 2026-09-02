use std::time::Instant;

use kodade_cli_proto::{PaneId, TabId, WorkspaceId};

pub const OSC52_LIMIT: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Absolute line index into the copy buffer (scrollback + screen).
    pub row: usize,
    /// Character (not byte) column within that line.
    pub col: usize,
}

/// Selection shape: `v` char-wise, `V` line-wise, `ctrl+v` rectangular block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    Char,
    Line,
    Block,
}

/// An in-progress `/` or `?` search entry shown in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub forward: bool,
    pub input: String,
}

/// A committed search: the lowercased needle, the direction it was entered in,
/// and the line/column of every case-insensitive substring match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub query: String,
    pub forward: bool,
    pub matches: Vec<Cursor>,
}

/// Character class for vi-style word motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

/// Copy mode over a pane's full history. Holds the joined scrollback + screen as
/// plain lines and navigates them with a cursor in absolute line coordinates.
#[derive(Debug, Clone)]
pub struct CopyMode {
    pub pane: PaneId,
    pub lines: Vec<String>,
    /// First visible line index (viewport offset).
    pub top: usize,
    /// Visible height in rows; drives page / half-page / H-M-L motions.
    pub height: usize,
    pub cursor: Cursor,
    pub anchor: Option<Cursor>,
    pub select: SelectKind,
    pub search: Option<Search>,
    pub prompt: Option<Prompt>,
    /// Set by a lone `g`; the next `g` jumps to the top.
    pub pending_g: bool,
    /// Last full-history refresh, so the client can throttle refetches.
    pub refreshed_at: Instant,
}

impl CopyMode {
    pub fn new(pane: PaneId, lines: Vec<String>, height: usize) -> Self {
        let lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        let height = height.max(1);
        let last = lines.len() - 1;
        let top = lines.len().saturating_sub(height);
        Self {
            pane,
            lines,
            top,
            height,
            // Enter at the newest line, like tmux copy mode.
            cursor: Cursor { row: last, col: 0 },
            anchor: None,
            select: SelectKind::Char,
            search: None,
            prompt: None,
            pending_g: false,
            refreshed_at: Instant::now(),
        }
    }

    /// Replace the buffer on a throttled refresh, keeping cursor and viewport
    /// valid and recomputing search matches against the new text.
    pub fn refresh(&mut self, lines: Vec<String>, height: usize) {
        self.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.height = height.max(1);
        self.clamp();
        self.scroll_into_view();
        if let Some(search) = self.search.take() {
            self.set_search(&search.query, search.forward);
        }
        self.refreshed_at = Instant::now();
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub fn line(&self, row: usize) -> &str {
        self.lines.get(row).map_or("", String::as_str)
    }
    fn line_len(&self, row: usize) -> usize {
        self.line(row).chars().count()
    }
    pub fn cur_line(&self) -> &str {
        self.line(self.cursor.row)
    }

    fn clamp(&mut self) {
        self.cursor.row = self.cursor.row.min(self.line_count().saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.line_len(self.cursor.row));
    }

    fn scroll_into_view(&mut self) {
        if self.cursor.row < self.top {
            self.top = self.cursor.row;
        } else if self.cursor.row >= self.top + self.height {
            self.top = self.cursor.row + 1 - self.height;
        }
        let max_top = self.line_count().saturating_sub(self.height);
        self.top = self.top.min(max_top);
    }

    // Every motion clamps the cursor and keeps it in view.
    fn after_move(&mut self) {
        self.clamp();
        self.scroll_into_view();
    }

    pub fn move_rows(&mut self, delta: isize) {
        self.cursor.row = self
            .cursor
            .row
            .saturating_add_signed(delta)
            .min(self.line_count().saturating_sub(1));
        self.after_move();
    }
    pub fn move_cols(&mut self, delta: isize) {
        self.cursor.col = self
            .cursor
            .col
            .saturating_add_signed(delta)
            .min(self.line_len(self.cursor.row));
        self.after_move();
    }
    pub fn goto_line_start(&mut self) {
        self.cursor.col = 0;
        self.after_move();
    }
    pub fn goto_line_end(&mut self) {
        self.cursor.col = self.line_len(self.cursor.row);
        self.after_move();
    }
    pub fn goto_first_nonblank(&mut self) {
        self.cursor.col = self
            .cur_line()
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0);
        self.after_move();
    }
    pub fn goto_top(&mut self) {
        self.cursor = Cursor { row: 0, col: 0 };
        self.after_move();
    }
    pub fn goto_bottom(&mut self) {
        self.cursor = Cursor {
            row: self.line_count().saturating_sub(1),
            col: 0,
        };
        self.after_move();
    }
    pub fn half_page(&mut self, down: bool) {
        let delta = (self.height / 2).max(1) as isize;
        self.move_rows(if down { delta } else { -delta });
    }
    pub fn page(&mut self, down: bool) {
        let delta = self.height.max(1) as isize;
        self.move_rows(if down { delta } else { -delta });
    }
    pub fn viewport_top(&mut self) {
        self.cursor.row = self.top;
        self.after_move();
    }
    pub fn viewport_middle(&mut self) {
        let bottom = (self.top + self.height - 1).min(self.line_count().saturating_sub(1));
        self.cursor.row = (self.top + bottom) / 2;
        self.after_move();
    }
    pub fn viewport_bottom(&mut self) {
        self.cursor.row = (self.top + self.height - 1).min(self.line_count().saturating_sub(1));
        self.after_move();
    }
    /// `{` / `}`: jump to the previous / next blank line.
    pub fn paragraph(&mut self, forward: bool) {
        let row = if forward {
            ((self.cursor.row + 1)..self.line_count())
                .find(|&r| self.line(r).trim().is_empty())
                .unwrap_or(self.line_count() - 1)
        } else {
            (0..self.cursor.row)
                .rev()
                .find(|&r| self.line(r).trim().is_empty())
                .unwrap_or(0)
        };
        self.cursor = Cursor { row, col: 0 };
        self.after_move();
    }

    // --- word motions -------------------------------------------------------

    fn char_at(&self, p: Cursor) -> Option<char> {
        self.line(p.row).chars().nth(p.col)
    }
    fn class_at(&self, p: Cursor, big: bool) -> Class {
        match self.char_at(p) {
            // Past the last char is the line break, treated as whitespace.
            None => Class::Space,
            Some(c) if c.is_whitespace() => Class::Space,
            Some(_) if big => Class::Word,
            Some(c) if c.is_alphanumeric() || c == '_' => Class::Word,
            Some(_) => Class::Punct,
        }
    }
    fn advance(&self, p: Cursor) -> Option<Cursor> {
        if p.col < self.line_len(p.row) {
            Some(Cursor {
                row: p.row,
                col: p.col + 1,
            })
        } else if p.row + 1 < self.line_count() {
            Some(Cursor {
                row: p.row + 1,
                col: 0,
            })
        } else {
            None
        }
    }
    fn retreat(&self, p: Cursor) -> Option<Cursor> {
        if p.col > 0 {
            Some(Cursor {
                row: p.row,
                col: p.col - 1,
            })
        } else if p.row > 0 {
            Some(Cursor {
                row: p.row - 1,
                col: self.line_len(p.row - 1),
            })
        } else {
            None
        }
    }

    pub fn next_word(&mut self, big: bool) {
        let mut p = self.cursor;
        let start = self.class_at(p, big);
        if start != Class::Space {
            while self.class_at(p, big) == start {
                match self.advance(p) {
                    Some(n) => p = n,
                    None => break,
                }
            }
        }
        while self.class_at(p, big) == Class::Space {
            match self.advance(p) {
                Some(n) => p = n,
                None => break,
            }
        }
        self.cursor = p;
        self.after_move();
    }
    pub fn prev_word(&mut self, big: bool) {
        let mut p = self.cursor;
        p = self.retreat(p).unwrap_or(p);
        while self.class_at(p, big) == Class::Space {
            match self.retreat(p) {
                Some(n) => p = n,
                None => break,
            }
        }
        let cls = self.class_at(p, big);
        while let Some(q) = self.retreat(p) {
            if self.class_at(q, big) == cls {
                p = q;
            } else {
                break;
            }
        }
        self.cursor = p;
        self.after_move();
    }
    pub fn end_word(&mut self, big: bool) {
        let mut p = self.cursor;
        p = self.advance(p).unwrap_or(p);
        while self.class_at(p, big) == Class::Space {
            match self.advance(p) {
                Some(n) => p = n,
                None => break,
            }
        }
        let cls = self.class_at(p, big);
        while let Some(q) = self.advance(p) {
            if self.class_at(q, big) == cls {
                p = q;
            } else {
                break;
            }
        }
        self.cursor = p;
        self.after_move();
    }

    // --- search -------------------------------------------------------------

    /// Recompute case-insensitive substring matches for `query`.
    pub fn set_search(&mut self, query: &str, forward: bool) {
        if query.is_empty() {
            self.search = None;
            return;
        }
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        for (row, line) in self.lines.iter().enumerate() {
            let hay = line.to_lowercase();
            let mut byte = 0;
            while let Some(off) = hay[byte..].find(&needle) {
                let start = byte + off;
                let col = hay[..start].chars().count();
                matches.push(Cursor { row, col });
                byte = start + needle.len();
            }
        }
        self.search = Some(Search {
            query: needle,
            forward,
            matches,
        });
    }
    /// Jump to the nearest match in `forward` direction, wrapping around.
    pub fn search_jump(&mut self, forward: bool) {
        let Some(search) = &self.search else { return };
        if search.matches.is_empty() {
            return;
        }
        let cur = (self.cursor.row, self.cursor.col);
        let target = if forward {
            search
                .matches
                .iter()
                .find(|m| (m.row, m.col) > cur)
                .or_else(|| search.matches.first())
        } else {
            search
                .matches
                .iter()
                .rev()
                .find(|m| (m.row, m.col) < cur)
                .or_else(|| search.matches.last())
        };
        if let Some(target) = target.copied() {
            self.cursor = target;
            self.after_move();
        }
    }
    pub fn clear_search(&mut self) {
        self.search = None;
    }

    // --- selection ----------------------------------------------------------

    /// Selected columns `[start, end)` on `row`, or `None` when unselected.
    pub fn selection_span(&self, row: usize) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let (start, end) = ordered(anchor, self.cursor);
        if row < start.row || row > end.row {
            return None;
        }
        let len = self.line_len(row);
        Some(match self.select {
            SelectKind::Line => (0, len),
            SelectKind::Block => {
                let (c0, c1) = (start.col.min(end.col), start.col.max(end.col));
                (c0.min(len), c1.min(len))
            }
            SelectKind::Char => {
                let from = if row == start.row { start.col } else { 0 };
                let to = if row == end.row { end.col } else { len };
                (from.min(len), to.min(len))
            }
        })
    }

    /// Match column ranges on `row`, for highlighting.
    pub fn search_spans(&self, row: usize) -> Vec<(usize, usize)> {
        let Some(search) = &self.search else {
            return Vec::new();
        };
        let width = search.query.chars().count();
        search
            .matches
            .iter()
            .filter(|m| m.row == row)
            .map(|m| (m.col, m.col + width))
            .collect()
    }

    /// Text to yank: the selection if one is active, else the current line.
    pub fn yank_text(&self) -> String {
        let Some(anchor) = self.anchor else {
            return self.cur_line().to_string();
        };
        let (start, end) = ordered(anchor, self.cursor);
        match self.select {
            SelectKind::Line => (start.row..=end.row)
                .map(|r| self.line(r).to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            SelectKind::Block => {
                let (c0, c1) = (start.col.min(end.col), start.col.max(end.col));
                (start.row..=end.row)
                    .map(|r| {
                        self.line(r)
                            .chars()
                            .skip(c0)
                            .take(c1.saturating_sub(c0))
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            SelectKind::Char => (start.row..=end.row)
                .map(|r| {
                    let line = self.line(r);
                    let from = if r == start.row { start.col } else { 0 };
                    let to = if r == end.row {
                        end.col
                    } else {
                        line.chars().count()
                    };
                    line.chars()
                        .skip(from)
                        .take(to.saturating_sub(from))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// Split terminal `contents` into lines, guaranteeing at least one row. Shared
/// with mouse selection (#12) so screen coordinates map the same way.
pub fn grid(contents: &str) -> Vec<&str> {
    let lines = contents.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        vec![""]
    } else {
        lines
    }
}

/// Order two cursors so the first is at or before the second.
fn ordered(a: Cursor, b: Cursor) -> (Cursor, Cursor) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

pub fn osc52(text: &str) -> (String, bool) {
    let (text, truncated) = if text.len() > OSC52_LIMIT {
        (&text[..text.floor_char_boundary(OSC52_LIMIT)], true)
    } else {
        (text, false)
    };
    (
        format!("\x1b]52;c;{}\x07", base64(text.as_bytes())),
        truncated,
    )
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTarget {
    Pane(PaneId),
    Tab(TabId),
    Workspace(WorkspaceId),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    SplitRight,
    SplitDown,
    Rename,
    Zoom,
    Close,
    /// Pane → its own tab (#14).
    BreakToTab,
    /// Reset split ratios in the pane's tab (#14).
    Equalize,
    /// Reorder a tab (#14).
    MoveLeft,
    MoveRight,
    /// Open the help overlay (#6); present on every target.
    Help,
    /// Cycle a workspace's sidebar swatch through the preset colors (#19).
    Color,
}
#[derive(Debug, Clone)]
pub struct Menu {
    pub target: MenuTarget,
    pub x: u16,
    pub y: u16,
    pub selected: usize,
}
impl Menu {
    pub fn actions(&self) -> &'static [MenuAction] {
        match self.target {
            MenuTarget::Pane(_) => &[
                MenuAction::SplitRight,
                MenuAction::SplitDown,
                MenuAction::Rename,
                MenuAction::Zoom,
                MenuAction::BreakToTab,
                MenuAction::Equalize,
                MenuAction::Close,
                MenuAction::Help,
            ],
            MenuTarget::Tab(_) => &[
                MenuAction::Rename,
                MenuAction::MoveLeft,
                MenuAction::MoveRight,
                MenuAction::Close,
                MenuAction::Help,
            ],
            MenuTarget::Workspace(_) => &[
                MenuAction::Rename,
                MenuAction::Color,
                MenuAction::Close,
                MenuAction::Help,
            ],
        }
    }
    pub fn move_by(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.actions().len() - 1);
    }
    pub fn action(&self) -> MenuAction {
        self.actions()[self.selected]
    }
}

/// Fixed context-menu width; labels like `Split right` fit within it.
pub const MENU_WIDTH: u16 = 14;

/// Left edge of the menu, flipped left of the click when it would clip the
/// right screen edge (#24). Render and hit-test share this so they agree.
pub fn menu_origin_x(x: u16, area_width: u16) -> u16 {
    if x.saturating_add(MENU_WIDTH) > area_width {
        x.saturating_sub(MENU_WIDTH)
    } else {
        x
    }
}

pub fn menu_hit(menu: &Menu, column: u16, row: u16, area_width: u16) -> Option<usize> {
    let x = menu_origin_x(menu.x, area_width);
    (column >= x
        && column < x.saturating_add(MENU_WIDTH)
        && row >= menu.y
        && row < menu.y.saturating_add(menu.actions().len() as u16))
    .then(|| (row - menu.y) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn copy(lines: &[&str], height: usize) -> CopyMode {
        CopyMode::new(
            PaneId(1),
            lines.iter().map(|s| s.to_string()).collect(),
            height,
        )
    }

    #[test]
    fn char_and_line_selection_yank_the_right_text() {
        let mut cm = copy(&["abc", "def"], 4);
        cm.cursor = Cursor { row: 0, col: 1 };
        cm.anchor = Some(cm.cursor);
        cm.select = SelectKind::Char;
        cm.cursor = Cursor { row: 1, col: 2 };
        assert_eq!(cm.yank_text(), "bc\nde");
        cm.select = SelectKind::Line;
        assert_eq!(cm.yank_text(), "abc\ndef");
    }

    #[test]
    fn block_selection_yanks_a_column_range() {
        let mut cm = copy(&["abcd", "efgh", "ijkl"], 4);
        cm.cursor = Cursor { row: 0, col: 1 };
        cm.anchor = Some(cm.cursor);
        cm.select = SelectKind::Block;
        cm.cursor = Cursor { row: 2, col: 3 };
        assert_eq!(cm.yank_text(), "bc\nfg\njk");
    }

    #[test]
    fn word_motions_handle_punctuation_and_spaces() {
        // "foo   bar.baz" — w stops at bar, then '.', then baz; b walks back.
        let mut cm = copy(&["foo   bar.baz"], 4);
        cm.cursor = Cursor { row: 0, col: 0 };
        cm.next_word(false);
        assert_eq!(cm.cursor.col, 6, "w skips the run of spaces to 'bar'");
        cm.next_word(false);
        assert_eq!(cm.cursor.col, 9, "w stops on the '.' punctuation");
        cm.next_word(false);
        assert_eq!(cm.cursor.col, 10, "w stops on 'baz'");
        cm.end_word(false);
        assert_eq!(cm.cursor.col, 12, "e lands on the last char of 'baz'");
        cm.prev_word(false);
        assert_eq!(cm.cursor.col, 10, "b returns to 'baz' start");
        cm.prev_word(true);
        assert_eq!(cm.cursor.col, 6, "B treats bar.baz as one WORD");
    }

    #[test]
    fn word_motion_crosses_line_boundaries() {
        let mut cm = copy(&["foo", "bar"], 4);
        cm.cursor = Cursor { row: 0, col: 0 };
        cm.next_word(false);
        assert_eq!(
            (cm.cursor.row, cm.cursor.col),
            (1, 0),
            "w wraps to next line"
        );
    }

    #[test]
    fn search_finds_and_steps_through_matches() {
        let mut cm = copy(&["alpha", "beta needle", "gamma NEEDLE end"], 4);
        cm.cursor = Cursor { row: 0, col: 0 };
        cm.set_search("needle", true);
        let hits = cm.search.as_ref().expect("search set").matches.len();
        assert_eq!(hits, 2, "case-insensitive match on both lines");
        cm.search_jump(true);
        assert_eq!((cm.cursor.row, cm.cursor.col), (1, 5));
        cm.search_jump(true);
        assert_eq!((cm.cursor.row, cm.cursor.col), (2, 6));
        cm.search_jump(true);
        assert_eq!(
            (cm.cursor.row, cm.cursor.col),
            (1, 5),
            "forward wraps around"
        );
        cm.search_jump(false);
        assert_eq!(
            (cm.cursor.row, cm.cursor.col),
            (2, 6),
            "backward wraps around"
        );
    }

    #[test]
    fn search_reaches_deep_scrollback() {
        let mut lines: Vec<&str> = vec!["filler"; 3000];
        lines[2900] = "the needle here";
        let mut cm = copy(&lines, 24);
        cm.cursor = Cursor { row: 0, col: 0 };
        cm.set_search("needle", true);
        cm.search_jump(true);
        assert_eq!(cm.cursor.row, 2900);
        // The viewport followed the cursor 2900 lines back.
        assert!(cm.top <= 2900 && 2900 < cm.top + cm.height);
    }
    #[test]
    fn osc52_encodes_and_limits() {
        assert_eq!(osc52("hi").0, "\x1b]52;c;aGk=\x07");
        assert!(osc52(&"a".repeat(OSC52_LIMIT + 1)).1);
    }
    #[test]
    fn menu_hit_tests_popup_bounds() {
        let menu = Menu {
            target: MenuTarget::Pane(PaneId(1)),
            x: 3,
            y: 4,
            selected: 0,
        };
        assert_eq!(menu_hit(&menu, 4, 6, 80), Some(2));
        assert_eq!(menu_hit(&menu, 20, 4, 80), None);
    }

    #[test]
    fn menu_flips_left_near_the_right_edge() {
        // With room to the right the menu opens at the click column.
        assert_eq!(menu_origin_x(3, 80), 3);
        // x + 14 past the edge flips the menu left of the click.
        assert_eq!(menu_origin_x(70, 80), 56);
        let menu = Menu {
            target: MenuTarget::Pane(PaneId(1)),
            x: 70,
            y: 4,
            selected: 0,
        };
        // Hit-testing agrees with the flipped origin (56..70), not 70..84.
        assert_eq!(menu_hit(&menu, 57, 4, 80), Some(0));
        assert_eq!(menu_hit(&menu, 71, 4, 80), None);
    }
}
