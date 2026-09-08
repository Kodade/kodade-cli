//! OSC 8 hyperlink cell ownership.  vt100 deliberately exposes no hyperlink
//! state, so keep a small parallel grid whose mutations follow its controls.

use kodade_cli_proto::LinkRange;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

const MAX_URI_BYTES: usize = 2048;
const MAX_URIS: usize = 128;
const MAX_LINK_SNAPSHOT_BYTES: usize = 32 * 1024;
const MAX_PENDING_BYTES: usize = 8192;

#[derive(Default)]
pub struct Tracker {
    parser: vte::Parser,
    events: Events,
    active: Option<u16>,
    uris: Vec<String>,
    normal: Grid,
    alternate: Grid,
    /// Rows that vt100 retained from the normal grid. This mirrors its bounded
    /// scrollback so a historical viewport never receives a URI from newer text.
    history: VecDeque<Vec<Option<u16>>>,
    history_capacity: usize,
    margins: [Option<(u16, u16)>; 2],
    pending: Vec<u8>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Grid {
    rows: u16,
    cols: u16,
    cells: Vec<Option<u16>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffState {
    active: Option<u16>,
    uris: Vec<String>,
    normal: Grid,
    alternate: Grid,
    history: VecDeque<Vec<Option<u16>>>,
    history_capacity: usize,
    margins: [Option<(u16, u16)>; 2],
    pending: Vec<u8>,
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
    fn row(&self, r: u16) -> &[Option<u16>] {
        let start = usize::from(r) * usize::from(self.cols);
        &self.cells[start..start + usize::from(self.cols)]
    }
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
        ranges(self.rows, self.cols, uris, |row| self.row(row).to_vec())
    }
}

fn ranges(
    rows: u16,
    cols: u16,
    uris: &[String],
    mut row_cells: impl FnMut(u16) -> Vec<Option<u16>>,
) -> Vec<LinkRange> {
    let mut result = Vec::new();
    let mut encoded = 0;
    for row in 0..rows {
        let mut col = 0;
        let cells = row_cells(row);
        while col < cols {
            let Some(uri) = cells[usize::from(col)] else {
                col += 1;
                continue;
            };
            let start = col;
            col += 1;
            while col < cols && cells[usize::from(col)] == Some(uri) {
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
            self.history.clear();
        }
        let alternate = screen.alternate_screen();
        let (row, col) = screen.cursor_position();
        let margin = self.margins[usize::from(alternate)].unwrap_or((0, rows - 1));
        let top = margin.0.min(rows - 1);
        let bottom = margin.1.min(rows - 1).max(top);
        self.events.0 = None;
        self.pending.push(byte);
        self.parser.advance(&mut self.events, &[byte]);
        let Some(event) = self.events.0.take() else {
            return;
        };
        self.pending.clear();
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
                    let wrapped = col.saturating_add(width) > cols;
                    let (r, c) = (
                        if wrapped {
                            row.saturating_add(1).min(bottom)
                        } else {
                            row
                        },
                        if wrapped { 0 } else { col },
                    );
                    if wrapped && row == bottom {
                        self.scroll(alternate, top, bottom, 1);
                    }
                    let grid = if alternate {
                        &mut self.alternate
                    } else {
                        &mut self.normal
                    };
                    // vt100 clears the other half of a replaced wide glyph.
                    // Clear its ownership too before assigning this glyph.
                    if screen
                        .cell(r, c)
                        .is_some_and(vt100::Cell::is_wide_continuation)
                    {
                        grid.set(r, c.saturating_sub(1), None);
                    }
                    if screen.cell(r, c).is_some_and(vt100::Cell::is_wide) {
                        grid.set(r, c.saturating_add(1), None);
                    }
                    if width > 1
                        && screen
                            .cell(r, c.saturating_add(1))
                            .is_some_and(vt100::Cell::is_wide)
                    {
                        grid.set(r, c.saturating_add(2), None);
                    }
                    for x in c..c.saturating_add(width).min(cols) {
                        grid.set(r, x, self.active);
                    }
                }
            }
            Event::Execute(b'\n' | b'\x0b' | b'\x0c') if row == bottom => {
                self.scroll(alternate, top, bottom, 1)
            }
            Event::Esc(b'D' | b'E') if row == bottom => self.scroll(alternate, top, bottom, 1),
            Event::Esc(b'M') if row == top => self.scroll(alternate, top, bottom, -1),
            Event::Esc(b'c') => {
                self.normal.clear();
                self.alternate.clear();
                self.active = None;
                self.uris.clear();
                self.history.clear();
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
                } else {
                    self.margins[usize::from(alternate)] = None;
                }
            }
            Event::Csi('J', p) => match p.first().copied().unwrap_or(0) {
                2 => self.grid_mut(alternate).clear(),
                3 => {
                    if !alternate {
                        self.history.clear();
                    }
                }
                0 => {
                    self.grid_mut(alternate).clear_row(row, col, cols);
                    for r in row.saturating_add(1)..rows {
                        self.grid_mut(alternate).clear_row(r, 0, cols);
                    }
                }
                1 => {
                    for r in 0..row {
                        self.grid_mut(alternate).clear_row(r, 0, cols);
                    }
                    self.grid_mut(alternate)
                        .clear_row(row, 0, col.saturating_add(1));
                }
                _ => {}
            },
            Event::Csi('K', p) => match p.first().copied().unwrap_or(0) {
                0 => self.grid_mut(alternate).clear_row(row, col, cols),
                1 => self
                    .grid_mut(alternate)
                    .clear_row(row, 0, col.saturating_add(1)),
                2 => self.grid_mut(alternate).clear_row(row, 0, cols),
                _ => {}
            },
            Event::Csi('X', p) => self.grid_mut(alternate).clear_row(
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
                    let value = if c + n < cols {
                        self.grid_mut(alternate).get(row, c + n)
                    } else {
                        None
                    };
                    self.grid_mut(alternate).set(row, c, value);
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
                    let value = if c >= col + n {
                        self.grid_mut(alternate).get(row, c - n)
                    } else {
                        None
                    };
                    self.grid_mut(alternate).set(row, c, value);
                }
            }
            Event::Csi('S', p) => self.scroll(
                alternate,
                top,
                bottom,
                i32::from(p.first().copied().unwrap_or(1).max(1)),
            ),
            Event::Csi('T', p) => self.scroll(
                alternate,
                top,
                bottom,
                -i32::from(p.first().copied().unwrap_or(1).max(1)),
            ),
            Event::Csi('L', p) => {
                let n = p.first().copied().unwrap_or(1).max(1);
                if (top..=bottom).contains(&row) {
                    self.scroll(alternate, row, bottom, -i32::from(n));
                }
            }
            Event::Csi('M', p) => {
                let n = p.first().copied().unwrap_or(1).max(1);
                if (top..=bottom).contains(&row) {
                    self.scroll(alternate, row, bottom, i32::from(n));
                }
            }
            _ => {}
        }
    }
    fn grid_mut(&mut self, alternate: bool) -> &mut Grid {
        if alternate {
            &mut self.alternate
        } else {
            &mut self.normal
        }
    }
    fn scroll(&mut self, alternate: bool, top: u16, bottom: u16, amount: i32) {
        let grid = if alternate {
            &mut self.alternate
        } else {
            &mut self.normal
        };
        if !alternate && amount > 0 && top == 0 && bottom + 1 == grid.rows {
            let count = (amount as u32).min(u32::from(grid.rows)) as u16;
            self.history
                .extend((0..count).map(|row| grid.row(row).to_vec()));
            while self.history.len() > self.history_capacity {
                self.history.pop_front();
            }
        }
        grid.scroll(top, bottom, amount);
    }
    pub fn ranges(&self, alternate: bool, scrollback: usize) -> Vec<LinkRange> {
        if alternate {
            self.alternate.ranges(&self.uris)
        } else {
            let start = self.history.len().saturating_sub(scrollback);
            ranges(self.normal.rows, self.normal.cols, &self.uris, |row| {
                let history_row = start + usize::from(row);
                self.history.get(history_row).cloned().unwrap_or_else(|| {
                    self.normal
                        .row(row.saturating_sub(scrollback as u16))
                        .to_vec()
                })
            })
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
        self.history.clear();
    }
    pub fn set_history_capacity(&mut self, capacity: usize) {
        self.history_capacity = capacity;
        while self.history.len() > capacity {
            self.history.pop_front();
        }
    }
    pub(crate) fn capture_handoff(&self) -> anyhow::Result<HandoffState> {
        if self.pending.len() > MAX_PENDING_BYTES {
            anyhow::bail!("unfinished hyperlink sequence exceeds handoff limit");
        }
        Ok(HandoffState {
            active: self.active,
            uris: self.uris.clone(),
            normal: self.normal.clone(),
            alternate: self.alternate.clone(),
            history: self.history.clone(),
            history_capacity: self.history_capacity,
            margins: self.margins,
            pending: self.pending.clone(),
        })
    }
    pub(crate) fn restore_handoff(state: HandoffState) -> Self {
        let mut result = Self {
            active: state.active,
            uris: state.uris,
            normal: state.normal,
            alternate: state.alternate,
            history: state.history,
            history_capacity: state.history_capacity,
            margins: state.margins,
            pending: state.pending,
            ..Default::default()
        };
        let pending = result.pending.clone();
        result.parser.advance(&mut result.events, &pending);
        result.events.0 = None;
        result
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
        let r = t.ranges(p.screen().alternate_screen(), p.screen().scrollback());
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
        assert_eq!(tracker.ranges(false, 0)[0].end_col, 2);
        feed(&mut tracker, &mut parser, b"\r\x1b[2@");
        assert_eq!(tracker.ranges(false, 0)[0].start_col, 2);
        feed(&mut tracker, &mut parser, b"\r\x1b[2K");
        assert!(tracker.ranges(false, 0).is_empty());
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://new\x1b\\x\x1b]8;;\x1b\\",
        );
        tracker.resize(2, 4);
        assert!(tracker.ranges(false, 0).is_empty());
        feed(&mut tracker, &mut parser, b"\x1b[?1049halt");
        assert!(tracker.ranges(true, 0).is_empty());
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://alt\x1b\\a\x1b]8;;\x1b\\",
        );
        assert_eq!(tracker.ranges(true, 0)[0].uri, "https://alt");
        feed(&mut tracker, &mut parser, b"\x1b[?1049l");
        assert!(tracker.ranges(false, 0).is_empty());
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
        let links = tracker.ranges(false, 0);
        assert!(links.iter().any(|link| {
            (link.row, link.start_col, link.end_col, link.uri.as_str()) == (1, 0, 1, "https://new")
        }));
    }

    #[test]
    fn retained_scrollback_uses_its_original_link_cells() {
        let mut parser = vt100::Parser::new(2.try_into().unwrap(), 8.try_into().unwrap(), 4);
        let mut tracker = Tracker::default();
        tracker.set_history_capacity(4);
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://first\x1b\\one\x1b]8;;\x1b\\\r\nplain\r\nlast",
        );
        parser.screen_mut().set_scrollback(1);
        let links = tracker.ranges(false, parser.screen().scrollback());
        assert!(links.iter().any(|link| {
            (link.row, link.start_col, link.end_col, link.uri.as_str())
                == (0, 0, 3, "https://first")
        }));
        assert!(links.iter().all(|link| link.row == 0));
    }

    #[test]
    fn replacing_a_wide_link_with_one_glyph_retires_both_cells() {
        let mut parser = vt100::Parser::new(2.try_into().unwrap(), 6.try_into().unwrap(), 0);
        let mut tracker = Tracker::default();
        feed(
            &mut tracker,
            &mut parser,
            b"\x1b]8;;https://wide\x1b\\\xe7\x95\x8c\x1b]8;;\x1b\\\rX",
        );
        assert!(tracker.ranges(false, 0).is_empty());
    }

    #[test]
    fn handoff_rebuilds_every_partial_osc8_byte() {
        let input = b"\x1b]8;;https://example.test/path\x1b\\label\x1b]8;;\x1b\\";
        for split in 0..input.len() {
            let (mut source, mut source_screen) = (Tracker::default(), parser());
            feed(&mut source, &mut source_screen, &input[..split]);
            let state: HandoffState = serde_json::from_slice(
                &serde_json::to_vec(&source.capture_handoff().unwrap()).unwrap(),
            )
            .unwrap();
            let mut restored = Tracker::restore_handoff(state);
            for &byte in &input[split..] {
                source.feed(byte, source_screen.screen());
                restored.feed(byte, source_screen.screen());
                source_screen.process(&[byte]);
            }
            assert_eq!(
                source.ranges(false, 0),
                restored.ranges(false, 0),
                "split {split}"
            );
        }
    }
}
