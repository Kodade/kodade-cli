//! OSC 8 hyperlink cell ownership.  vt100 deliberately exposes no hyperlink
//! state, so keep a small parallel grid whose mutations follow its controls.

use kodade_cli_proto::LinkRange;
use unicode_width::UnicodeWidthChar;

const MAX_URI_BYTES: usize = 2048;
const MAX_URIS: usize = 128;
const MAX_LINK_SNAPSHOT_BYTES: usize = 32 * 1024;

#[derive(Default)]
pub struct Tracker {
    parser: vte::Parser,
    events: Events,
    active: Option<u16>,
    uris: Vec<String>,
    normal: Grid,
    alternate: Grid,
    margins: [Option<(u16, u16)>; 2],
}

#[derive(Default)]
struct Grid {
    rows: u16,
    cols: u16,
    cells: Vec<Option<u16>>,
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
            self.0 = Some(Event::Osc(valid_uri(&uri).then_some(uri)));
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

fn valid_uri(uri: &str) -> bool {
    uri.len() <= MAX_URI_BYTES
        && (uri.starts_with("https://") || uri.starts_with("http://"))
        && !uri.chars().any(char::is_control)
}

impl Grid {
    fn get(&self, r: u16, c: u16) -> Option<u16> {
        self.cells
            .get(usize::from(r) * usize::from(self.cols) + usize::from(c))
            .copied()
            .flatten()
    }
    fn set(&mut self, r: u16, c: u16, value: Option<u16>) {
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
                            self.get(src, c)
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
                            self.get(src.unwrap(), c)
                        } else {
                            None
                        },
                    );
                }
            }
        }
    }
    fn ranges(&self, uris: &[String]) -> Vec<LinkRange> {
        let mut result = Vec::new();
        let mut encoded = 0;
        for row in 0..self.rows {
            let mut col = 0;
            while col < self.cols {
                let Some(uri) = self.get(row, col) else {
                    col += 1;
                    continue;
                };
                let start = col;
                col += 1;
                while col < self.cols && self.get(row, col) == Some(uri) {
                    col += 1;
                }
                let Some(uri) = uris.get(usize::from(uri)) else {
                    continue;
                };
                encoded += uri.len() + 16;
                if encoded > MAX_LINK_SNAPSHOT_BYTES {
                    break;
                }
                result.push(LinkRange {
                    row,
                    start_col: start,
                    end_col: col,
                    uri: uri.clone(),
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
        if self.normal.rows != rows || self.normal.cols != cols {
            // vt100 can reflow or discard rows on resize. Without its exact
            // row mapping, retaining a target could attach it to new text.
            self.normal = Grid {
                rows,
                cols,
                cells: vec![None; usize::from(rows) * usize::from(cols)],
            };
            self.alternate = Grid {
                rows,
                cols,
                cells: vec![None; usize::from(rows) * usize::from(cols)],
            };
        }
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
            Event::Osc(uri) => {
                self.active = uri.and_then(|uri| {
                    self.uris
                        .iter()
                        .position(|known| known == &uri)
                        .or_else(|| {
                            (self.uris.len() < MAX_URIS).then(|| {
                                self.uris.push(uri);
                                self.uris.len() - 1
                            })
                        })
                        .and_then(|id| u16::try_from(id).ok())
                });
            }
            Event::Print(c) => {
                let width = c.width().unwrap_or(0) as u16;
                if width > 0 {
                    let wrapped = col >= cols
                        || (col >= cols.saturating_sub(width)
                            && screen.cell(row, cols - 1).is_some_and(|cell| {
                                cell.has_contents() || cell.is_wide_continuation()
                            }));
                    if wrapped {
                        grid.scroll(top, bottom, 1);
                    }
                    let (r, c) = (row, if wrapped { 0 } else { col });
                    for x in c..c.saturating_add(width).min(cols) {
                        grid.set(r, x, self.active);
                    }
                }
            }
            Event::Execute(b'\n' | b'\x0b' | b'\x0c') if row == bottom => {
                grid.scroll(top, bottom, 1)
            }
            Event::Esc(b'D' | b'E') if row == bottom => grid.scroll(top, bottom, 1),
            Event::Esc(b'M') if row == top => grid.scroll(top, bottom, -1),
            Event::Esc(b'c') => {
                self.normal.clear();
                self.alternate.clear();
                self.active = None;
                self.uris.clear();
                self.margins = [None, None];
            }
            Event::Csi('r', p) => {
                let start = p.first().copied().unwrap_or(1).max(1) - 1;
                let end = if p.get(1).copied().unwrap_or(0) == 0 {
                    rows - 1
                } else {
                    p[1].min(rows) - 1
                };
                if start < end {
                    self.margins[usize::from(alternate)] = Some((start, end));
                }
            }
            Event::Csi('J', p) => match p.first().copied().unwrap_or(0) {
                2 => grid.clear(),
                3 => {}
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
                            grid.get(row, c + n)
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
                            grid.get(row, c - n)
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
                if (top..=bottom).contains(&row) {
                    grid.scroll(row, bottom, -i32::from(n));
                }
            }
            Event::Csi('M', p) => {
                let n = p.first().copied().unwrap_or(1).max(1);
                if (top..=bottom).contains(&row) {
                    grid.scroll(row, bottom, i32::from(n));
                }
            }
            _ => {}
        }
    }
    pub fn ranges(&self, alternate: bool) -> Vec<LinkRange> {
        if alternate {
            self.alternate.ranges(&self.uris)
        } else {
            self.normal.ranges(&self.uris)
        }
    }
    pub fn clear_alternate(&mut self) {
        self.alternate.clear();
    }
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.normal = Grid {
            rows,
            cols,
            cells: vec![None; usize::from(rows) * usize::from(cols)],
        };
        self.alternate = Grid {
            rows,
            cols,
            cells: vec![None; usize::from(rows) * usize::from(cols)],
        };
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

    #[test]
    fn metadata_follows_edits_resize_and_alternate_buffers() {
        let (mut tracker, mut parser) = (Tracker::default(), parser());
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://old\x1b\\abcd\x1b]8;;\x1b\\",
        );
        feed(&mut tracker, &mut parser, b"\r\x1b[2P");
        assert_eq!(tracker.ranges(false)[0].end_col, 2);
        feed(&mut tracker, &mut parser, b"\r\x1b[2@");
        assert_eq!(tracker.ranges(false)[0].start_col, 2);
        feed(&mut tracker, &mut parser, b"\r\x1b[2K");
        assert!(tracker.ranges(false).is_empty());
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://new\x1b\\x\x1b]8;;\x1b\\",
        );
        tracker.resize(2, 4);
        assert!(tracker.ranges(false).is_empty());
        feed(&mut tracker, &mut parser, b"\x1b[?1049halt");
        assert!(tracker.ranges(true).is_empty());
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://alt\x1b\\a\x1b]8;;\x1b\\",
        );
        assert_eq!(tracker.ranges(true)[0].uri, "https://alt");
        feed(&mut tracker, &mut parser, b"\x1b[?1049l");
        assert!(tracker.ranges(false).is_empty());
    }

    #[test]
    fn autowrap_and_same_glyph_overwrite_keep_only_current_uri() {
        let mut parser = vt100::Parser::new(2.try_into().unwrap(), 3.try_into().unwrap(), 0);
        let mut tracker = Tracker::default();
        feed(&mut tracker, &mut parser, b"\x1b]8;;https://old\x1b\\abc");
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://new\x1b\\d\x1b]8;;\x1b\\",
        );
        let links = tracker.ranges(false);
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].start_col, links[0].end_col), (0, 1));
        assert_eq!(links[0].uri, "https://new");
    }
}
