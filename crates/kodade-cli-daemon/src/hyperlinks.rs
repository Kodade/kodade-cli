//! OSC 8 hyperlink cell ownership.  vt100 deliberately exposes no hyperlink
//! state, so keep a small parallel grid whose mutations follow its controls.

use kodade_cli_proto::LinkRange;
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Tracker {
    parser: vte::Parser,
    events: Events,
    active: Option<String>,
    normal: Grid,
    alternate: Grid,
    margins: [Option<(u16, u16)>; 2],
}

#[derive(Default)]
struct Grid {
    rows: u16,
    cols: u16,
    cells: Vec<Option<String>>,
}

#[derive(Default)]
struct Events(Option<Event>);
enum Event {
    Print(char),
    Execute(u8),
    Osc(Option<String>),
    Csi(char, Vec<u16>),
    Esc(u8),
}

impl vte::Perform for Events {
    fn print(&mut self, c: char) {
        self.0 = Some(Event::Print(c));
    }
    fn execute(&mut self, b: u8) {
        self.0 = Some(Event::Execute(b));
    }
    fn esc_dispatch(&mut self, i: &[u8], ignore: bool, b: u8) {
        if !ignore && i.is_empty() {
            self.0 = Some(Event::Esc(b));
        }
    }
    fn osc_dispatch(&mut self, p: &[&[u8]], _: bool) {
        if p.first() == Some(&b"8".as_slice()) && p.len() >= 3 {
            let uri = String::from_utf8_lossy(p[2]).into_owned();
            self.0 = Some(Event::Osc((!uri.is_empty()).then_some(uri)));
        }
    }
    fn csi_dispatch(&mut self, p: &vte::Params, i: &[u8], ignore: bool, c: char) {
        if ignore || !i.is_empty() {
            return;
        }
        self.0 = Some(Event::Csi(
            c,
            p.iter().map(|x| x.first().copied().unwrap_or(0)).collect(),
        ));
    }
}

impl Grid {
    fn resize(&mut self, rows: u16, cols: u16) {
        if self.rows == rows && self.cols == cols {
            return;
        }
        let mut next = vec![None; usize::from(rows) * usize::from(cols)];
        for row in 0..self.rows.min(rows) {
            for col in 0..self.cols.min(cols) {
                next[usize::from(row) * usize::from(cols) + usize::from(col)] =
                    self.get(row, col).cloned();
            }
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = next;
    }
    fn get(&self, r: u16, c: u16) -> Option<&String> {
        self.cells
            .get(usize::from(r) * usize::from(self.cols) + usize::from(c))
            .and_then(Option::as_ref)
    }
    fn set(&mut self, r: u16, c: u16, value: Option<String>) {
        if r < self.rows && c < self.cols {
            let i = usize::from(r) * usize::from(self.cols) + usize::from(c);
            self.cells[i] = value;
        }
    }
    fn clear(&mut self) {
        self.cells.fill(None);
    }
    fn clear_row(&mut self, r: u16, from: u16, to: u16) {
        for c in from.min(self.cols)..to.min(self.cols) {
            self.set(r, c, None);
        }
    }
    fn scroll(&mut self, top: u16, bottom: u16, amount: i32) {
        if top >= self.rows || top > bottom || amount == 0 {
            return;
        }
        let bottom = bottom.min(self.rows - 1);
        let height = bottom - top + 1;
        let n = amount.unsigned_abs().min(u32::from(height)) as u16;
        if amount > 0 {
            for r in top..=bottom {
                let src = r.saturating_add(n);
                for c in 0..self.cols {
                    self.set(
                        r,
                        c,
                        if src <= bottom {
                            self.get(src, c).cloned()
                        } else {
                            None
                        },
                    );
                }
            }
        } else {
            for r in (top..=bottom).rev() {
                let src = r.checked_sub(n);
                for c in 0..self.cols {
                    self.set(
                        r,
                        c,
                        if src.is_some_and(|v| v >= top) {
                            self.get(src.unwrap(), c).cloned()
                        } else {
                            None
                        },
                    );
                }
            }
        }
    }
    fn ranges(&self) -> Vec<LinkRange> {
        let mut result = Vec::new();
        for row in 0..self.rows {
            let mut col = 0;
            while col < self.cols {
                let Some(uri) = self.get(row, col).cloned() else {
                    col += 1;
                    continue;
                };
                let start = col;
                col += 1;
                while col < self.cols && self.get(row, col) == Some(&uri) {
                    col += 1;
                }
                result.push(LinkRange {
                    row,
                    start_col: start,
                    end_col: col,
                    uri,
                });
            }
        }
        result
    }
}

impl Tracker {
    pub fn feed(&mut self, byte: u8, screen: &vt100::Screen) {
        let (rows, cols) = screen.size();
        let (rows, cols) = (rows.get(), cols.get());
        self.normal.resize(rows, cols);
        self.alternate.resize(rows, cols);
        let alternate = screen.alternate_screen();
        let (row, col) = screen.cursor_position();
        let margin = self.margins[usize::from(alternate)].unwrap_or((0, rows - 1));
        let top = margin.0.min(rows - 1);
        let bottom = margin.1.min(rows - 1).max(top);
        self.events.0 = None;
        self.parser.advance(&mut self.events, &[byte]);
        let Some(event) = self.events.0.take() else {
            return;
        };
        let grid = if alternate {
            &mut self.alternate
        } else {
            &mut self.normal
        };
        match event {
            Event::Osc(uri) => self.active = uri,
            Event::Print(c) => {
                let width = c.width().unwrap_or(0) as u16;
                if width > 0 {
                    let (r, c) = if col.saturating_add(width) > cols {
                        (row.saturating_add(1), 0)
                    } else {
                        (row, col)
                    };
                    for x in c..c.saturating_add(width).min(cols) {
                        grid.set(r, x, self.active.clone());
                    }
                }
            }
            Event::Execute(b'\n' | b'\x0b' | b'\x0c') if row == bottom => {
                grid.scroll(top, bottom, 1)
            }
            Event::Esc(b'M') if row == top => grid.scroll(top, bottom, -1),
            Event::Esc(b'c') => {
                self.normal.clear();
                self.alternate.clear();
                self.active = None;
                self.margins = [None, None];
            }
            Event::Csi('r', p) => {
                let start = p.first().copied().unwrap_or(1).max(1) - 1;
                let end = if p.get(1).copied().unwrap_or(0) == 0 {
                    rows - 1
                } else {
                    p[1].min(rows) - 1
                };
                self.margins[usize::from(alternate)] = (start < end).then_some((start, end));
            }
            Event::Csi('J', p) => match p.first().copied().unwrap_or(0) {
                2 | 3 => grid.clear(),
                0 => {
                    grid.clear_row(row, col, cols);
                    for r in row.saturating_add(1)..rows {
                        grid.clear_row(r, 0, cols);
                    }
                }
                1 => {
                    for r in 0..row {
                        grid.clear_row(r, 0, cols);
                    }
                    grid.clear_row(row, 0, col.saturating_add(1));
                }
                _ => {}
            },
            Event::Csi('K', p) => match p.first().copied().unwrap_or(0) {
                0 => grid.clear_row(row, col, cols),
                1 => grid.clear_row(row, 0, col.saturating_add(1)),
                2 => grid.clear_row(row, 0, cols),
                _ => {}
            },
            Event::Csi('X', p) => grid.clear_row(
                row,
                col,
                col.saturating_add(p.first().copied().unwrap_or(1).max(1)),
            ),
            Event::Csi('P', p) => {
                let n = p
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .min(cols.saturating_sub(col));
                for c in col..cols {
                    grid.set(
                        row,
                        c,
                        if c + n < cols {
                            grid.get(row, c + n).cloned()
                        } else {
                            None
                        },
                    );
                }
            }
            Event::Csi('@', p) => {
                let n = p
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .min(cols.saturating_sub(col));
                for c in (col..cols).rev() {
                    grid.set(
                        row,
                        c,
                        if c >= col + n {
                            grid.get(row, c - n).cloned()
                        } else {
                            None
                        },
                    );
                }
            }
            Event::Csi('S', p) => grid.scroll(
                top,
                bottom,
                i32::from(p.first().copied().unwrap_or(1).max(1)),
            ),
            Event::Csi('T', p) => grid.scroll(
                top,
                bottom,
                -i32::from(p.first().copied().unwrap_or(1).max(1)),
            ),
            Event::Csi('L', p) => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.scroll(row, bottom, -i32::from(n));
            }
            Event::Csi('M', p) => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.scroll(row, bottom, i32::from(n));
            }
            _ => {}
        }
    }
    pub fn ranges(&self, alternate: bool) -> Vec<LinkRange> {
        if alternate {
            self.alternate.ranges()
        } else {
            self.normal.ranges()
        }
    }
    pub fn clear_alternate(&mut self) {
        self.alternate.clear();
    }
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.normal.resize(rows, cols);
        self.alternate.resize(rows, cols);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parser() -> vt100::Parser {
        vt100::Parser::new(3.try_into().unwrap(), 8.try_into().unwrap(), 0)
    }
    fn feed(t: &mut Tracker, p: &mut vt100::Parser, s: &[u8]) {
        for &b in s {
            t.feed(b, p.screen());
            p.process(&[b]);
        }
    }
    #[test]
    fn osc8_tracks_overwrite_and_scroll() {
        let (mut t, mut p) = (Tracker::default(), parser());
        feed(
            &mut t,
            &mut p,
            b"\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\\rNO\n\n",
        );
        let r = t.ranges(p.screen().alternate_screen());
        assert!(r.iter().all(|x| x.uri == "https://x"));
        assert!(r.iter().any(|x| x.row == 0 && x.start_col == 2));
    }
}
