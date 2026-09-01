use kodade_cli_proto::{PaneId, Screen, TabId, WorkspaceId};

use crate::render::SidebarTarget;

pub const OSC52_LIMIT: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub struct CopyMode {
    pub pane: PaneId,
    pub screen: Screen,
    pub cursor: Cursor,
    pub anchor: Option<Cursor>,
}

impl CopyMode {
    pub fn new(pane: PaneId, screen: Screen) -> Self {
        Self {
            pane,
            screen,
            cursor: Cursor { row: 0, col: 0 },
            anchor: None,
        }
    }

    pub fn refresh(&mut self, screen: Screen) {
        self.screen = screen;
        self.clamp();
    }

    pub fn move_by(&mut self, rows: isize, cols: isize) {
        self.cursor.row = self
            .cursor
            .row
            .saturating_add_signed(rows)
            .min(grid(&self.screen.contents).len().saturating_sub(1));
        let line_len = grid(&self.screen.contents)
            .get(self.cursor.row)
            .map_or(0, |line| line.chars().count());
        self.cursor.col = self.cursor.col.saturating_add_signed(cols).min(line_len);
    }

    fn clamp(&mut self) {
        self.move_by(0, 0);
    }
}

pub fn grid(contents: &str) -> Vec<&str> {
    let lines = contents.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        vec![""]
    } else {
        lines
    }
}

pub fn selected_text(contents: &str, first: Cursor, second: Cursor) -> String {
    let lines = grid(contents);
    let (start, end) = if (first.row, first.col) <= (second.row, second.col) {
        (first, second)
    } else {
        (second, first)
    };
    (start.row..=end.row.min(lines.len().saturating_sub(1)))
        .map(|row| {
            let line = lines[row];
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row {
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
        .join("\n")
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

pub fn navigate(rows: &[SidebarTarget], current: Option<usize>, delta: isize) -> Option<usize> {
    (!rows.is_empty()).then(|| {
        current
            .unwrap_or(0)
            .saturating_add_signed(delta)
            .min(rows.len() - 1)
    })
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
                MenuAction::Close,
            ],
            _ => &[MenuAction::Rename, MenuAction::Close],
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

pub fn menu_hit(menu: &Menu, column: u16, row: u16) -> Option<usize> {
    (column >= menu.x
        && column < menu.x.saturating_add(14)
        && row >= menu.y
        && row < menu.y.saturating_add(menu.actions().len() as u16))
    .then(|| (row - menu.y) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_is_charwise_and_linewise() {
        assert_eq!(
            selected_text(
                "abc\ndef",
                Cursor { row: 0, col: 1 },
                Cursor { row: 1, col: 2 }
            ),
            "bc\nde"
        );
    }
    #[test]
    fn osc52_encodes_and_limits() {
        assert_eq!(osc52("hi").0, "\x1b]52;c;aGk=\x07");
        assert!(osc52(&"a".repeat(OSC52_LIMIT + 1)).1);
    }
    #[test]
    fn navigate_traverses_sidebar_rows() {
        let rows = vec![
            SidebarTarget::Workspace(WorkspaceId(1)),
            SidebarTarget::Tab(TabId(2)),
        ];
        assert_eq!(navigate(&rows, Some(0), 1), Some(1));
        assert_eq!(navigate(&rows, Some(1), 1), Some(1));
    }
    #[test]
    fn menu_hit_tests_popup_bounds() {
        let menu = Menu {
            target: MenuTarget::Pane(PaneId(1)),
            x: 3,
            y: 4,
            selected: 0,
        };
        assert_eq!(menu_hit(&menu, 4, 6), Some(2));
        assert_eq!(menu_hit(&menu, 20, 4), None);
    }
}
