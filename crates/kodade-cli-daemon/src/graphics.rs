//! A bounded Kitty graphics store. Escape commands never reach the host terminal.

mod media;

use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use kodade_cli_proto::{ImageData, ImagePlacement};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

const MAX_IMAGE: usize = 8 * 1024 * 1024;
const MAX_STORE: usize = 32 * 1024 * 1024;
const MAX_IMAGES: usize = 16;
const MAX_PLACEMENTS: usize = 64;
const MAX_VIRTUAL_CELLS: usize = 65_536;
const MAX_FRAME: usize = 8192;
const KITTY_UNICODE_PLACEHOLDER: &[u8] = "\u{10eeee}".as_bytes();
const PLACEHOLDER_CELL: u8 = b' ';

pub struct NormalizedByte {
    pub byte: u8,
}

/// Decodes Unicode scalars alongside vt100. This keeps Kitty placeholder
/// metadata intact even though the visible terminal receives a blank cell.
#[derive(Default)]
pub struct UnicodeTracker {
    parser: vte::Parser,
    chars: UnicodeChars,
    pending: Vec<u8>,
    overflow: bool,
}

#[derive(Default)]
struct UnicodeChars(Vec<char>, bool);

impl vte::Perform for UnicodeChars {
    fn print(&mut self, c: char) {
        self.0.push(c);
        self.1 = true;
    }
    fn execute(&mut self, byte: u8) {
        self.1 = matches!(byte, 0x18 | 0x1a);
    }
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
        self.1 = true;
    }
    fn csi_dispatch(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        self.1 = true;
    }
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        self.1 = true;
    }
    fn unhook(&mut self) {
        self.1 = true;
    }
}

impl UnicodeTracker {
    pub fn feed(&mut self, byte: u8) -> Vec<char> {
        self.chars.0.clear();
        self.chars.1 = false;
        let standalone = self.pending.is_empty() && byte < 32 && byte != 27;
        if self.pending.len() < MAX_FRAME {
            self.pending.push(byte);
        } else {
            self.overflow = true;
        }
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(&mut self.chars, &[byte]);
        self.parser = parser;
        if self.chars.1 || standalone {
            self.pending.clear();
            self.overflow = false;
            // OSC/DCS dispatch at ESC finishes the string, leaving the parser
            // inside the new escape sequence until the next byte arrives.
            if byte == 27 {
                self.pending.push(byte);
            }
        }
        std::mem::take(&mut self.chars.0)
    }
    #[cfg(any(unix, test))]
    pub fn capture_handoff(&self) -> Result<UnicodeHandoff> {
        if self.overflow {
            bail!("unfinished unicode sequence exceeds handoff limit");
        }
        Ok(UnicodeHandoff {
            pending: self.pending.clone(),
        })
    }
    #[cfg(any(unix, test))]
    pub fn restore_handoff(state: UnicodeHandoff) -> Self {
        let mut result = Self {
            pending: state.pending,
            ..Default::default()
        };
        let pending = result.pending.clone();
        result.parser.advance(&mut result.chars, &pending);
        result.chars.0.clear();
        result.chars.1 = false;
        result
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg(any(unix, test))]
pub struct UnicodeHandoff {
    pending: Vec<u8>,
}

// Kitty's ordered row/column diacritic table. Its index, rather than the
// codepoint value, is the image-grid coordinate.
const KITTY_DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

pub fn kitty_diacritic_index(c: char) -> Option<u32> {
    KITTY_DIACRITICS
        .binary_search(&(c as u32))
        .ok()
        .map(|index| index as u32)
}

/// vt100 treats Kitty's dedicated placeholder as a combining character. Keep
/// its style but substitute a blank one-cell glyph so its grid position stays
/// observable by the renderer.
pub fn normalize_unicode_placeholders(pending: &mut Vec<u8>, text: &[u8]) -> Vec<NormalizedByte> {
    let mut normalized = Vec::with_capacity(text.len());
    for &byte in text {
        pending.push(byte);
        while !KITTY_UNICODE_PLACEHOLDER.starts_with(pending) {
            normalized.push(NormalizedByte {
                byte: pending.remove(0),
            });
        }
        if pending.as_slice() == KITTY_UNICODE_PLACEHOLDER {
            normalized.push(NormalizedByte {
                byte: PLACEHOLDER_CELL,
            });
            pending.clear();
        }
    }
    normalized
}

#[derive(Default)]
pub struct VirtualStyle {
    parser: vte::Parser,
    foreground: Option<u32>,
    underline: Option<u32>,
    pending: Vec<u8>,
    complete: bool,
    overflow: bool,
}

impl VirtualStyle {
    pub fn feed(&mut self, byte: u8) {
        self.complete = false;
        let standalone = self.pending.is_empty() && byte < 32 && byte != 27;
        if self.pending.len() < MAX_FRAME {
            self.pending.push(byte);
        } else {
            self.overflow = true;
        }
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, &[byte]);
        self.parser = parser;
        if self.complete || standalone {
            self.pending.clear();
            self.overflow = false;
            if byte == 27 {
                self.pending.push(byte);
            }
        }
    }
    pub fn ids(&self) -> Option<(u32, u32)> {
        self.foreground
            .map(|image| (image, self.underline.unwrap_or(0)))
    }
    #[cfg(any(unix, test))]
    pub fn capture_handoff(&self) -> Result<VirtualStyleHandoff> {
        if self.overflow {
            bail!("unfinished SGR sequence exceeds handoff limit");
        }
        Ok(VirtualStyleHandoff {
            foreground: self.foreground,
            underline: self.underline,
            pending: self.pending.clone(),
        })
    }
    #[cfg(any(unix, test))]
    pub fn restore_handoff(state: VirtualStyleHandoff) -> Self {
        let mut result = Self {
            foreground: state.foreground,
            underline: state.underline,
            pending: state.pending,
            ..Default::default()
        };
        let pending = result.pending.clone();
        let mut parser = std::mem::take(&mut result.parser);
        parser.advance(&mut result, &pending);
        result.parser = parser;
        result.complete = false;
        result
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg(any(unix, test))]
pub struct VirtualStyleHandoff {
    foreground: Option<u32>,
    underline: Option<u32>,
    pending: Vec<u8>,
}

impl vte::Perform for VirtualStyle {
    fn print(&mut self, _: char) {
        self.complete = true;
    }
    fn execute(&mut self, byte: u8) {
        self.complete = matches!(byte, 0x18 | 0x1a);
    }
    fn esc_dispatch(&mut self, intermediate: &[u8], ignore: bool, byte: u8) {
        self.complete = true;
        if !ignore && intermediate.is_empty() && byte == b'c' {
            self.foreground = None;
            self.underline = None;
        }
    }
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        self.complete = true;
    }
    fn unhook(&mut self) {
        self.complete = true;
    }
    fn csi_dispatch(&mut self, params: &vte::Params, _: &[u8], ignore: bool, command: char) {
        self.complete = true;
        if ignore || command != 'm' {
            return;
        }
        let values: Vec<u16> = params
            .iter()
            .flat_map(|value| value.iter().copied())
            .collect();
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => {
                    self.foreground = None;
                    self.underline = None;
                }
                39 => self.foreground = None,
                59 => self.underline = None,
                38 | 58 => {
                    let target = values[index];
                    let Some(mode) = values.get(index + 1).copied() else {
                        break;
                    };
                    let color = match mode {
                        5 => values.get(index + 2).copied().map(u32::from),
                        2 => match (
                            values.get(index + 2),
                            values.get(index + 3),
                            values.get(index + 4),
                        ) {
                            (Some(r), Some(g), Some(b)) => {
                                Some((u32::from(*r) << 16) | (u32::from(*g) << 8) | u32::from(*b))
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    if target == 38 {
                        self.foreground = color;
                    } else {
                        self.underline = color;
                    }
                    index += if mode == 2 { 5 } else { 3 };
                    continue;
                }
                _ => {}
            }
            index += 1;
        }
    }
}

pub enum Token {
    Text(Vec<u8>),
    Graphics(Vec<u8>),
    Invalid,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decoder {
    pending: Vec<u8>,
    graphics: bool,
    overflow: bool,
    escaped: bool,
}

impl Decoder {
    /// The APC prefix and terminator may both straddle PTY reads.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut text = Vec::new();
        for &byte in bytes {
            if self.graphics {
                if self.escaped && byte == b'\\' {
                    if !self.overflow {
                        self.pending.pop();
                        tokens.push(Token::Graphics(std::mem::take(&mut self.pending)));
                    } else {
                        tokens.push(Token::Invalid);
                    }
                    self.pending.clear();
                    self.graphics = false;
                    self.overflow = false;
                    self.escaped = false;
                } else {
                    if self.pending.len() < MAX_FRAME {
                        self.pending.push(byte);
                    } else {
                        self.overflow = true;
                    }
                    self.escaped = byte == 27;
                }
                continue;
            }
            match self.pending.as_slice() {
                [] if byte == 27 => self.pending.push(byte),
                [27] if byte == b'_' => self.pending.push(byte),
                [27, b'_'] if byte == b'G' => {
                    if !text.is_empty() {
                        tokens.push(Token::Text(std::mem::take(&mut text)));
                    }
                    self.pending.clear();
                    self.graphics = true;
                }
                _ => {
                    text.append(&mut self.pending);
                    if byte == 27 {
                        self.pending.push(byte);
                    } else {
                        text.push(byte);
                    }
                }
            }
        }
        if !text.is_empty() {
            tokens.push(Token::Text(text));
        }
        tokens
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Transfer {
    params: BTreeMap<String, String>,
    encoded: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct Store {
    images: BTreeMap<u32, ImageData>,
    image_numbers: BTreeMap<u32, u32>,
    placements: Vec<StoredPlacement>,
    transfer: Option<Transfer>,
    revision: u64,
    next_image: u32,
    next_placement: u32,
    /// Placeholder cells use logical rows: zero is the live viewport's top,
    /// negative rows are retained normal-screen history.
    virtual_cells: BTreeMap<(bool, i32, u16), VirtualCell>,
    last_virtual_cell: Option<(bool, i32, u16)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredPlacement {
    alternate: bool,
    placement: ImagePlacement,
    /// A virtual placement gets its visible geometry from Unicode placeholders.
    virtual_placement: bool,
    parent: Option<(u32, u32)>,
    relative_offset: (i32, i32),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct VirtualCell {
    image_low: u32,
    placement: Option<u32>,
    row: Option<u32>,
    col: Option<u32>,
    image_high: Option<u8>,
}

#[derive(Clone, Copy)]
struct ResolvedVirtualCell {
    image_low: u32,
    placement: Option<u32>,
    row: u32,
    col: u32,
    image_high: Option<u8>,
}

pub struct Outcome {
    pub reply: Vec<u8>,
    pub advance: Option<(u16, u16)>,
}

/// Observe only controls that move images, using vt100's own parser grammar.
#[derive(Default)]
pub struct Tracker {
    parser: vte::Parser,
    events: Controls,
    margins: [Option<(u16, u16)>; 2],
    pending: Vec<u8>,
    overflow: bool,
}

#[derive(Default)]
struct Controls(Option<Control>, bool);
enum Control {
    Print(char),
    Index,
    ReverseIndex,
    Clear,
    EraseHistory,
    EraseDisplay(u16),
    EraseLine(u16),
    EraseChars(u16),
    InsertChars(u16),
    DeleteChars(u16),
    InsertLines(u16),
    DeleteLines(u16),
    Reset,
    Scroll(i32),
    Margins(u16, u16),
}

impl vte::Perform for Controls {
    fn print(&mut self, c: char) {
        self.1 = true;
        self.0 = Some(Control::Print(c));
    }
    fn execute(&mut self, byte: u8) {
        self.1 = matches!(byte, 0x18 | 0x1a);
        if matches!(byte, 10..=12) {
            self.0 = Some(Control::Index);
        }
    }
    fn esc_dispatch(&mut self, intermediate: &[u8], ignore: bool, byte: u8) {
        self.1 = true;
        if ignore || !intermediate.is_empty() {
            return;
        }
        self.0 = match byte {
            b'D' => Some(Control::Index),
            b'M' => Some(Control::ReverseIndex),
            b'c' => Some(Control::Reset),
            _ => None,
        };
    }
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediate: &[u8],
        ignore: bool,
        command: char,
    ) {
        self.1 = true;
        if ignore || !intermediate.is_empty() {
            return;
        }
        let mut params = params.iter();
        let first = params.next().and_then(|p| p.first()).copied().unwrap_or(0);
        self.0 = match command {
            'J' if first == 2 => Some(Control::Clear),
            'J' if first == 3 => Some(Control::EraseHistory),
            'J' if first <= 1 => Some(Control::EraseDisplay(first)),
            'K' if first <= 2 => Some(Control::EraseLine(first)),
            'X' => Some(Control::EraseChars(first.max(1))),
            '@' => Some(Control::InsertChars(first.max(1))),
            'P' => Some(Control::DeleteChars(first.max(1))),
            'L' => Some(Control::InsertLines(first.max(1))),
            'M' => Some(Control::DeleteLines(first.max(1))),
            'S' => Some(Control::Scroll(i32::from(first.max(1)))),
            'T' => Some(Control::Scroll(-i32::from(first.max(1)))),
            'r' => Some(Control::Margins(
                first,
                params.next().and_then(|p| p.first()).copied().unwrap_or(0),
            )),
            _ => None,
        };
    }
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        self.1 = true;
    }
    fn unhook(&mut self) {
        self.1 = true;
    }
}

impl Tracker {
    pub fn reset_resize(&mut self) {
        self.margins = [None, None];
    }

    pub fn feed(&mut self, byte: u8, screen: &vt100::Screen, store: &mut Store) -> u16 {
        self.events.0 = None;
        self.events.1 = false;
        let standalone_control = self.pending.is_empty() && byte < 32 && byte != 27;
        if self.pending.len() < MAX_FRAME {
            self.pending.push(byte);
        } else {
            self.overflow = true;
        }
        self.parser.advance(&mut self.events, &[byte]);
        if self.events.1 || standalone_control {
            self.pending.clear();
            self.overflow = false;
            if byte == 27 {
                self.pending.push(byte);
            }
        }
        let Some(event) = self.events.0.take() else {
            return 0;
        };
        let printed = match event {
            Control::Print(c) => c.width().unwrap_or(0) as u16,
            _ => 0,
        };
        let alternate = screen.alternate_screen();
        let (height, width) = screen.size();
        let (height, width) = (height.get(), width.get());
        let (row, col) = screen.cursor_position();
        let margin = &mut self.margins[usize::from(alternate)];
        let (top, bottom) = margin.unwrap_or((0, height - 1));
        let bottom = bottom.min(height - 1);
        let top = top.min(bottom);
        let shift = match event {
            Control::Margins(start, end) => {
                let start = start.max(1) - 1;
                let end = if end == 0 {
                    height - 1
                } else {
                    (end - 1).min(height - 1)
                };
                *margin = (start < end).then_some((start, end));
                return 0;
            }
            Control::Reset => {
                self.margins = [None, None];
                store.clear_screen(false);
                store.clear_screen(true);
                return 0;
            }
            Control::Clear => {
                store.clear_visible(alternate);
                return 0;
            }
            Control::EraseHistory => {
                store
                    .virtual_cells
                    .retain(|&(alt, row, _), _| alt != alternate || row >= 0);
                store.placements.retain(|entry| {
                    entry.alternate != alternate
                        || entry.virtual_placement
                        || entry.placement.row + i32::from(entry.placement.rows) > 0
                });
                return 0;
            }
            Control::EraseDisplay(mode) => {
                store.erase_display(alternate, row, col, height, width, mode);
                return 0;
            }
            Control::EraseLine(mode) => {
                store.erase_line(alternate, row, col, width, mode);
                return 0;
            }
            Control::EraseChars(count) => {
                store.erase_range(alternate, row, col, col.saturating_add(count).min(width));
                return 0;
            }
            Control::InsertChars(count) => {
                store.insert_chars(
                    alternate,
                    row,
                    col,
                    count.min(width.saturating_sub(col)),
                    width,
                );
                return 0;
            }
            Control::DeleteChars(count) => {
                store.delete_chars(
                    alternate,
                    row,
                    col,
                    count.min(width.saturating_sub(col)),
                    width,
                );
                return 0;
            }
            Control::InsertLines(count) if (top..=bottom).contains(&row) => {
                store.insert_lines(alternate, row, bottom, count.min(bottom - row + 1));
                return 0;
            }
            Control::DeleteLines(count) if (top..=bottom).contains(&row) => {
                store.delete_lines(alternate, row, bottom, count.min(bottom - row + 1));
                return 0;
            }
            Control::Index if row == bottom => 1,
            Control::ReverseIndex if row == top => -1,
            Control::Scroll(rows) => {
                rows.clamp(-i32::from(bottom - top + 1), i32::from(bottom - top + 1))
            }
            Control::Print(c) if row == bottom && c.width().unwrap_or(0) > 0 => {
                let cell_width = c.width().unwrap_or(1) as u16;
                let last = screen.cell(row, width - 1);
                if col > width.saturating_sub(cell_width)
                    && last.is_some_and(|cell| cell.has_contents() || cell.is_wide_continuation())
                {
                    1
                } else {
                    0
                }
            }
            _ => 0,
        };
        if shift != 0 {
            if top == 0 && bottom == height - 1 && !alternate && shift > 0 {
                store.scroll(alternate, shift, height);
            } else {
                store.scroll_region(alternate, top, bottom, shift);
            }
        }
        printed
    }
}

/// Bounded graphics and unfinished escape input carried across a live handoff.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg(any(unix, test))]
pub(crate) struct HandoffState {
    store: HandoffStore,
    decoder: Decoder,
    margins: [Option<(u16, u16)>; 2],
    pending_text: Vec<u8>,
}

/// The JSON handoff format cannot encode tuple map keys. Keep Store optimized
/// for terminal updates and serialize its sparse virtual grid as explicit rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg(any(unix, test))]
struct HandoffStore {
    images: BTreeMap<u32, ImageData>,
    image_numbers: BTreeMap<u32, u32>,
    placements: Vec<StoredPlacement>,
    transfer: Option<Transfer>,
    revision: u64,
    next_image: u32,
    next_placement: u32,
    virtual_cells: Vec<(bool, i32, u16, VirtualCell)>,
    last_virtual_cell: Option<(bool, i32, u16)>,
}

#[cfg(any(unix, test))]
impl From<&Store> for HandoffStore {
    fn from(store: &Store) -> Self {
        Self {
            images: store.images.clone(),
            image_numbers: store.image_numbers.clone(),
            placements: store.placements.clone(),
            transfer: store.transfer.clone(),
            revision: store.revision,
            next_image: store.next_image,
            next_placement: store.next_placement,
            virtual_cells: store
                .virtual_cells
                .iter()
                .map(|(&(alternate, row, col), &cell)| (alternate, row, col, cell))
                .collect(),
            last_virtual_cell: store.last_virtual_cell,
        }
    }
}

#[cfg(any(unix, test))]
impl From<HandoffStore> for Store {
    fn from(store: HandoffStore) -> Self {
        Self {
            images: store.images,
            image_numbers: store.image_numbers,
            placements: store.placements,
            transfer: store.transfer,
            revision: store.revision,
            next_image: store.next_image,
            next_placement: store.next_placement,
            virtual_cells: store
                .virtual_cells
                .into_iter()
                .map(|(alternate, row, col, cell)| ((alternate, row, col), cell))
                .collect(),
            last_virtual_cell: store.last_virtual_cell,
        }
    }
}

#[cfg(any(unix, test))]
impl HandoffState {
    pub(crate) fn capture(store: &Store, decoder: &Decoder, tracker: &Tracker) -> Result<Self> {
        if tracker.overflow {
            bail!("unfinished terminal sequence exceeds handoff limit");
        }
        let state = Self {
            store: store.into(),
            decoder: decoder.clone(),
            margins: tracker.margins,
            pending_text: tracker.pending.clone(),
        };
        state.validate()?;
        Ok(state)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.store.images.len() > MAX_IMAGES
            || self.store.placements.len() > MAX_PLACEMENTS
            || self.decoder.pending.len() > MAX_FRAME
            || self.pending_text.len() > MAX_FRAME
            || self
                .store
                .images
                .values()
                .map(|image| image.data.len())
                .sum::<usize>()
                > MAX_STORE * 4 / 3 + MAX_IMAGES * 4
            || self
                .store
                .transfer
                .as_ref()
                .is_some_and(|transfer| transfer.encoded.len() > MAX_IMAGE * 4 / 3 + 4)
        {
            bail!("graphics handoff exceeds bounded store limits");
        }
        Ok(())
    }

    pub(crate) fn pending_text(&self) -> &[u8] {
        &self.pending_text
    }

    pub(crate) fn restore(self) -> (Store, Decoder, Tracker) {
        let mut tracker = Tracker {
            margins: self.margins,
            pending: self.pending_text,
            ..Default::default()
        };
        tracker
            .parser
            .advance(&mut tracker.events, &tracker.pending);
        (self.store.into(), self.decoder, tracker)
    }
}

impl Store {
    /// Keep virtual cells in the same logical coordinate space as vt100 after
    /// its active grid moves rows during a resize. The inactive grid has no
    /// observable cursor delta, so its placeholders are conservatively reset.
    pub fn resize_virtual_cells(&mut self, active: bool, row_delta: i32, rows: u16, cols: u16) {
        self.virtual_cells = self
            .virtual_cells
            .iter()
            .filter_map(|(&(alternate, row, col), &cell)| {
                if alternate != active {
                    return None;
                }
                let row = row.saturating_add(row_delta);
                let row_valid = if alternate {
                    (0..i32::from(rows)).contains(&row)
                } else {
                    row >= -10_000
                };
                (row_valid && col < cols).then_some(((alternate, row, col), cell))
            })
            .collect();
        self.last_virtual_cell = self
            .last_virtual_cell
            .filter(|key| self.virtual_cells.contains_key(key));
    }

    fn image_by_number(&self, number: u32) -> Option<u32> {
        self.images
            .values()
            .filter(|image| self.image_numbers.get(&image.id) == Some(&number))
            .max_by_key(|image| image.revision)
            .map(|image| image.id)
    }
    fn delete_matching(
        &mut self,
        mut matches: impl FnMut(&StoredPlacement) -> bool,
    ) -> HashSet<u32> {
        let mut deleted = HashSet::new();
        loop {
            let before = self.placements.len();
            self.placements.retain(|entry| {
                let remove =
                    matches(entry) || entry.parent.is_some_and(|parent| deleted.contains(&parent));
                if remove {
                    deleted.insert((entry.placement.image, entry.placement.placement));
                }
                !remove
            });
            if self.placements.len() == before {
                break;
            }
        }
        deleted.into_iter().map(|(image, _)| image).collect()
    }

    fn remove_image_data(&mut self, affected: &HashSet<u32>) {
        self.images.retain(|image, _| {
            !affected.contains(image)
                || self
                    .placements
                    .iter()
                    .any(|entry| entry.placement.image == *image)
        });
    }

    pub fn clear_virtual_cell(&mut self, alternate: bool, row: u16, col: u16) {
        let key = (alternate, i32::from(row), col);
        self.virtual_cells.remove(&key);
        if self.last_virtual_cell == Some(key) {
            self.last_virtual_cell = None;
        }
    }

    pub fn record_virtual_cell(
        &mut self,
        alternate: bool,
        row: u16,
        col: u16,
        ids: Option<(u32, u32)>,
    ) {
        if let Some((image_low, placement)) = ids.filter(|(image, _)| *image != 0) {
            self.virtual_cells.insert(
                (alternate, i32::from(row), col),
                VirtualCell {
                    image_low,
                    placement: (placement != 0).then_some(placement),
                    row: None,
                    col: None,
                    image_high: None,
                },
            );
            self.last_virtual_cell = Some((alternate, i32::from(row), col));
            while self.virtual_cells.len() > MAX_VIRTUAL_CELLS {
                self.virtual_cells.pop_first();
            }
        }
    }

    pub fn add_virtual_diacritic(&mut self, index: u32) {
        let Some(key) = self.last_virtual_cell else {
            return;
        };
        let Some(cell) = self.virtual_cells.get_mut(&key) else {
            return;
        };
        match (cell.row, cell.col, cell.image_high) {
            (None, _, _) => cell.row = Some(index),
            (Some(_), None, _) => cell.col = Some(index),
            (Some(_), Some(_), None) if index <= u32::from(u8::MAX) => {
                cell.image_high = Some(index as u8)
            }
            _ => {}
        }
    }

    pub fn end_virtual_sequence(&mut self) {
        self.last_virtual_cell = None;
    }

    fn validate_relative(&self, key: (u32, u32), parent: (u32, u32)) -> Result<()> {
        let mut current = parent;
        for _ in 0..8 {
            if current == key {
                bail!("ECYCLE: relative placement cycle");
            }
            let Some(entry) = self
                .placements
                .iter()
                .find(|entry| (entry.placement.image, entry.placement.placement) == current)
            else {
                bail!("ENOPARENT: parent placement not found");
            };
            let Some(next) = entry.parent else {
                return Ok(());
            };
            current = next;
        }
        bail!("ETOODEEP: relative placement chain exceeds 8");
    }

    pub fn cancel_transfer(&mut self) {
        self.transfer = None;
    }

    pub fn image(&self, id: u32, revision: u64) -> Result<ImageData> {
        let image = self.images.get(&id).context("image no longer available")?;
        if image.revision != revision {
            bail!("image revision changed; refresh the pane");
        }
        Ok(image.clone())
    }

    #[cfg(test)]
    pub fn placements(&self, alternate: bool, scroll: usize) -> Vec<ImagePlacement> {
        self.placements_with_screen(alternate, scroll, None)
    }

    pub fn placements_with_screen(
        &self,
        alternate: bool,
        scroll: usize,
        screen: Option<&vt100::Screen>,
    ) -> Vec<ImagePlacement> {
        let mut placements: Vec<_> = self
            .placements
            .iter()
            .filter(|entry| {
                entry.alternate == alternate && !entry.virtual_placement && entry.parent.is_none()
            })
            .map(|entry| {
                let mut p = entry.placement.clone();
                p.row = p.row.saturating_add(scroll.min(i32::MAX as usize) as i32);
                p
            })
            .collect();
        if let Some(screen) = screen {
            placements.extend(self.virtual_placements(alternate, scroll, screen));
        }
        self.resolve_relative(placements)
    }

    fn virtual_placements(
        &self,
        alternate: bool,
        scroll: usize,
        screen: &vt100::Screen,
    ) -> Vec<ImagePlacement> {
        let (rows, cols) = screen.size();
        let mut visible = Vec::new();
        if self
            .virtual_cells
            .range((alternate, i32::MIN, 0)..=(alternate, i32::MAX, u16::MAX))
            .next()
            .is_none()
        {
            return visible;
        }
        for row in 0..rows.get() {
            let logical_row = i32::from(row).saturating_sub(scroll.min(i32::MAX as usize) as i32);
            let mut previous: Option<ResolvedVirtualCell> = None;
            for col in 0..cols.get() {
                let Some(cell) = self
                    .virtual_cells
                    .get(&(alternate, logical_row, col))
                    .copied()
                else {
                    previous = None;
                    continue;
                };
                let continues = previous.is_some_and(|previous| {
                    previous.image_low == cell.image_low
                        && previous.placement == cell.placement
                        && cell.row.is_none_or(|value| value == previous.row)
                        && cell
                            .col
                            .is_none_or(|value| value == previous.col.saturating_add(1))
                        && cell
                            .image_high
                            .is_none_or(|value| Some(value) == previous.image_high)
                });
                let resolved = ResolvedVirtualCell {
                    image_low: cell.image_low,
                    placement: cell.placement,
                    row: cell
                        .row
                        .or_else(|| {
                            continues.then(|| previous.expect("continuation has a cell").row)
                        })
                        .unwrap_or(0),
                    col: cell
                        .col
                        .or_else(|| {
                            continues.then(|| {
                                previous
                                    .expect("continuation has a cell")
                                    .col
                                    .saturating_add(1)
                            })
                        })
                        .unwrap_or(0),
                    image_high: cell.image_high.or_else(|| {
                        continues
                            .then(|| previous.expect("continuation has a cell").image_high)
                            .flatten()
                    }),
                };
                previous = Some(resolved);
                let image =
                    resolved.image_low | (u32::from(resolved.image_high.unwrap_or(0)) << 24);
                let placement_id = resolved.placement.unwrap_or(0);
                let Some(entry) = self.placements.iter().find(|entry| {
                    entry.alternate == alternate
                        && entry.virtual_placement
                        && entry.placement.image == image
                        && (placement_id == 0 || entry.placement.placement == placement_id)
                }) else {
                    continue;
                };
                let mut p = entry.placement.clone();
                let grid_cols = u32::from(p.cols.max(1));
                let grid_rows = u32::from(p.rows.max(1));
                if resolved.col >= grid_cols || resolved.row >= grid_rows {
                    continue;
                }
                let source_x =
                    u64::from(p.source_width) * u64::from(resolved.col) / u64::from(grid_cols);
                let source_y =
                    u64::from(p.source_height) * u64::from(resolved.row) / u64::from(grid_rows);
                p.source_x += source_x as u32;
                p.source_y += source_y as u32;
                p.source_width = (u64::from(p.source_width) * u64::from(resolved.col + 1)
                    / u64::from(grid_cols)) as u32
                    - source_x as u32;
                p.source_height = (u64::from(p.source_height) * u64::from(resolved.row + 1)
                    / u64::from(grid_rows)) as u32
                    - source_y as u32;
                p.row = i32::from(row);
                p.col = col;
                p.cols = 1;
                p.rows = 1;
                visible.push(p);
            }
        }
        visible
    }

    fn resolve_relative(&self, mut visible: Vec<ImagePlacement>) -> Vec<ImagePlacement> {
        // Keep every virtual cell render entry. Relative placements use the
        // top-left visible placeholder as their parent anchor.
        let mut anchors: BTreeMap<(u32, u32), ImagePlacement> = BTreeMap::new();
        for p in &visible {
            anchors
                .entry((p.image, p.placement))
                .and_modify(|anchor| {
                    if (p.row, p.col) < (anchor.row, anchor.col) {
                        *anchor = p.clone();
                    }
                })
                .or_insert_with(|| p.clone());
        }
        for _ in 0..8 {
            let before = anchors.len();
            for entry in &self.placements {
                let key = (entry.placement.image, entry.placement.placement);
                if anchors.contains_key(&key) {
                    continue;
                }
                let Some(parent) = entry.parent else { continue };
                let Some(mut p) = anchors.get(&parent).cloned() else {
                    continue;
                };
                p.image = entry.placement.image;
                p.revision = entry.placement.revision;
                p.placement = entry.placement.placement;
                p.cols = entry.placement.cols;
                p.rows = entry.placement.rows;
                p.source_x = entry.placement.source_x;
                p.source_y = entry.placement.source_y;
                p.source_width = entry.placement.source_width;
                p.source_height = entry.placement.source_height;
                p.x_offset = entry.placement.x_offset;
                p.y_offset = entry.placement.y_offset;
                p.z = entry.placement.z;
                p.col = (i32::from(p.col).saturating_add(entry.relative_offset.0))
                    .clamp(0, i32::from(u16::MAX)) as u16;
                p.row = p.row.saturating_add(entry.relative_offset.1);
                anchors.insert((p.image, p.placement), p.clone());
                visible.push(p);
            }
            if anchors.len() == before {
                break;
            }
        }
        visible
    }

    /// A terminal erase removes the text cells, while virtual placement
    /// definitions survive for a later placeholder redraw.
    pub fn clear_screen(&mut self, alternate: bool) {
        self.placements
            .retain(|entry| entry.alternate != alternate || entry.virtual_placement);
        self.virtual_cells
            .retain(|(alt, _, _), _| *alt != alternate);
        if self
            .last_virtual_cell
            .is_some_and(|(alt, _, _)| alt == alternate)
        {
            self.last_virtual_cell = None;
        }
    }

    fn clear_visible(&mut self, alternate: bool) {
        self.placements.retain(|entry| {
            entry.alternate != alternate
                || entry.virtual_placement
                || entry.placement.row + i32::from(entry.placement.rows) <= 0
        });
        self.virtual_cells
            .retain(|&(alt, row, _), _| alt != alternate || row < 0);
        self.last_virtual_cell = None;
    }

    fn erase_range(&mut self, alternate: bool, row: u16, start: u16, end: u16) {
        self.virtual_cells.retain(|&(alt, cell_row, cell_col), _| {
            alt != alternate || cell_row != i32::from(row) || cell_col < start || cell_col >= end
        });
        if self
            .last_virtual_cell
            .is_some_and(|(alt, cell_row, cell_col)| {
                alt == alternate && cell_row == i32::from(row) && (start..end).contains(&cell_col)
            })
        {
            self.last_virtual_cell = None;
        }
    }

    fn erase_line(&mut self, alternate: bool, row: u16, col: u16, width: u16, mode: u16) {
        let (start, end) = match mode {
            0 => (col, width),
            1 => (0, col.saturating_add(1)),
            _ => (0, width),
        };
        self.erase_range(alternate, row, start, end);
    }

    fn erase_display(
        &mut self,
        alternate: bool,
        row: u16,
        col: u16,
        height: u16,
        width: u16,
        mode: u16,
    ) {
        match mode {
            0 => self.virtual_cells.retain(|&(alt, cell_row, cell_col), _| {
                alt != alternate
                    || cell_row < i32::from(row)
                    || (cell_row == i32::from(row) && cell_col < col)
            }),
            1 => self.virtual_cells.retain(|&(alt, cell_row, cell_col), _| {
                alt != alternate
                    || cell_row < 0
                    || cell_row > i32::from(row)
                    || (cell_row == i32::from(row) && cell_col > col)
            }),
            _ => self.clear_screen(alternate),
        }
        let _ = (height, width); // dimensions document the VT erase boundary.
        self.last_virtual_cell = None;
    }

    fn shift_cells(
        &mut self,
        alternate: bool,
        row: u16,
        width: u16,
        map_col: impl Fn(u16) -> Option<u16>,
    ) {
        let logical_row = i32::from(row);
        self.virtual_cells = self
            .virtual_cells
            .iter()
            .filter_map(|(&(alt, cell_row, col), &cell)| {
                if alt != alternate || cell_row != logical_row {
                    return Some(((alt, cell_row, col), cell));
                }
                map_col(col)
                    .filter(|next| *next < width)
                    .map(|next| ((alt, cell_row, next), cell))
            })
            .collect();
        self.last_virtual_cell = self
            .last_virtual_cell
            .filter(|key| self.virtual_cells.contains_key(key));
    }

    fn insert_chars(&mut self, alternate: bool, row: u16, col: u16, count: u16, width: u16) {
        if count == 0 {
            return;
        }
        self.shift_cells(alternate, row, width, |cell_col| {
            if cell_col < col {
                Some(cell_col)
            } else {
                cell_col.checked_add(count)
            }
        });
    }

    fn delete_chars(&mut self, alternate: bool, row: u16, col: u16, count: u16, width: u16) {
        if count == 0 {
            return;
        }
        self.shift_cells(alternate, row, width, |cell_col| {
            if cell_col < col {
                Some(cell_col)
            } else if cell_col < col.saturating_add(count) {
                None
            } else {
                Some(cell_col - count)
            }
        });
    }

    fn shift_lines(
        &mut self,
        alternate: bool,
        row: u16,
        bottom: u16,
        map_row: impl Fn(i32) -> Option<i32>,
    ) {
        let row = i32::from(row);
        let end = i32::from(bottom) + 1;
        self.virtual_cells = self
            .virtual_cells
            .iter()
            .filter_map(|(&(alt, cell_row, col), &cell)| {
                if alt != alternate || !(row..end).contains(&cell_row) {
                    return Some(((alt, cell_row, col), cell));
                }
                map_row(cell_row)
                    .filter(|next| (row..end).contains(next))
                    .map(|next| ((alt, next, col), cell))
            })
            .collect();
        self.last_virtual_cell = self
            .last_virtual_cell
            .filter(|key| self.virtual_cells.contains_key(key));
    }

    fn insert_lines(&mut self, alternate: bool, row: u16, bottom: u16, count: u16) {
        if count > 0 {
            self.shift_lines(alternate, row, bottom, |cell_row| {
                cell_row.checked_add(i32::from(count))
            });
        }
    }

    fn delete_lines(&mut self, alternate: bool, row: u16, bottom: u16, count: u16) {
        if count > 0 {
            let first_retained = i32::from(row) + i32::from(count);
            self.shift_lines(alternate, row, bottom, |cell_row| {
                (cell_row >= first_retained).then_some(cell_row - i32::from(count))
            });
        }
    }

    pub fn scroll(&mut self, alternate: bool, rows: i32, height: u16) {
        for entry in &mut self.placements {
            if entry.alternate == alternate && !entry.virtual_placement {
                entry.placement.row = entry.placement.row.saturating_sub(rows);
            }
        }
        self.placements
            .retain(|entry| entry.virtual_placement || entry.placement.row > -10_000);
        self.virtual_cells = self
            .virtual_cells
            .iter()
            .filter_map(|(&(alt, row, col), &cell)| {
                if alt != alternate {
                    return Some(((alt, row, col), cell));
                }
                let row = row.saturating_sub(rows);
                let retained = if alternate {
                    (0..i32::from(height)).contains(&row)
                } else {
                    row >= -10_000
                };
                retained.then_some(((alt, row, col), cell))
            })
            .collect();
        self.last_virtual_cell = self.last_virtual_cell.and_then(|(alt, row, col)| {
            if alt != alternate {
                return Some((alt, row, col));
            }
            let row = row.saturating_sub(rows);
            self.virtual_cells
                .contains_key(&(alt, row, col))
                .then_some((alt, row, col))
        });
    }

    fn scroll_region(&mut self, alternate: bool, top: u16, bottom: u16, shift: i32) {
        let top = i32::from(top);
        let end = i32::from(bottom) + 1;
        for entry in &mut self.placements {
            let p = &mut entry.placement;
            if entry.alternate != alternate
                || entry.virtual_placement
                || p.row < top
                || p.row + i32::from(p.rows) > end
            {
                continue;
            }
            p.row -= shift;
            let clipped_top = (top - p.row).max(0).min(i32::from(p.rows)) as u16;
            let clipped_bottom = (p.row + i32::from(p.rows) - end)
                .max(0)
                .min(i32::from(p.rows)) as u16;
            let removed = clipped_top.saturating_add(clipped_bottom).min(p.rows);
            let pixels = u64::from(p.source_height) * u64::from(clipped_top) / u64::from(p.rows);
            p.source_y += pixels as u32;
            p.source_height = (u64::from(p.source_height) * u64::from(p.rows - removed)
                / u64::from(p.rows)) as u32;
            p.rows -= removed;
            p.row += i32::from(clipped_top);
        }
        self.placements.retain(|entry| {
            entry.virtual_placement
                || (entry.placement.rows > 0 && entry.placement.source_height > 0)
        });
        self.virtual_cells = self
            .virtual_cells
            .iter()
            .filter_map(|(&(alt, row, col), &cell)| {
                if alt != alternate || !(top..end).contains(&row) {
                    return Some(((alt, row, col), cell));
                }
                let row = row.saturating_sub(shift);
                (top..end).contains(&row).then_some(((alt, row, col), cell))
            })
            .collect();
        self.last_virtual_cell = self
            .last_virtual_cell
            .filter(|key| self.virtual_cells.contains_key(key));
    }

    #[cfg(test)]
    pub fn command(&mut self, frame: &[u8], cursor: (u16, u16), alternate: bool) -> Outcome {
        self.command_with_screen(frame, cursor, alternate, None)
    }

    pub fn command_with_screen(
        &mut self,
        frame: &[u8],
        cursor: (u16, u16),
        alternate: bool,
        screen: Option<&vt100::Screen>,
    ) -> Outcome {
        let (header, payload) = frame
            .iter()
            .position(|byte| *byte == b';')
            .map(|index| (&frame[..index], &frame[index + 1..]))
            .unwrap_or((frame, &[]));
        let params = std::str::from_utf8(header)
            .unwrap_or("")
            .split(',')
            .filter_map(|part| {
                part.split_once('=')
                    .map(|(key, value)| (key.to_string(), value.to_string()))
            })
            .collect::<BTreeMap<_, _>>();
        if params.get("a").is_some_and(|action| action == "d") {
            self.transfer = None;
        }
        let response = self.transfer.as_ref().map(|t| &t.params).unwrap_or(&params);
        let mut id = number(response, "i", 0).unwrap_or(0);
        let image_number = number(response, "I", 0).unwrap_or(0);
        let quiet = number(response, "q", 0).unwrap_or(0);
        let placement = number(response, "p", 0).unwrap_or(0);
        let result = self.apply(params, payload, cursor, alternate, screen, &mut id);
        self.image_numbers
            .retain(|id, _| self.images.contains_key(id));
        let (status, advance) = match result {
            Ok(advance) => ("OK".to_owned(), advance),
            Err(error) => {
                self.transfer = None;
                (format!("EINVAL:{error}"), None)
            }
        };
        let reply = if self.transfer.is_some()
            || quiet == 2
            || (quiet == 1 && status == "OK")
            || (id == 0 && image_number == 0)
        {
            Vec::new()
        } else {
            let p = if placement > 0 {
                format!(",p={placement}")
            } else {
                String::new()
            };
            let n = if image_number > 0 {
                format!(",I={image_number}")
            } else {
                String::new()
            };
            format!("\x1b_Gi={id}{n}{p};{status}\x1b\\").into_bytes()
        };
        Outcome { reply, advance }
    }

    fn apply(
        &mut self,
        mut params: BTreeMap<String, String>,
        payload: &[u8],
        cursor: (u16, u16),
        alternate: bool,
        screen: Option<&vt100::Screen>,
        response_id: &mut u32,
    ) -> Result<Option<(u16, u16)>> {
        let more = number(&params, "m", 0)? == 1;
        let mut encoded = payload.to_vec();
        if let Some(mut transfer) = self.transfer.take() {
            if params.keys().any(|key| !matches!(key.as_str(), "m" | "q")) {
                bail!("continuation chunk contains a new graphics command");
            }
            if transfer.encoded.len().saturating_add(encoded.len()) > MAX_IMAGE * 4 / 3 + 4 {
                bail!("image transfer exceeds 8 MiB");
            }
            transfer.encoded.append(&mut encoded);
            encoded = transfer.encoded;
            params = transfer.params;
        }
        if encoded.len() > MAX_IMAGE * 4 / 3 + 4 {
            bail!("image transfer exceeds 8 MiB");
        }
        if more {
            self.transfer = Some(Transfer { params, encoded });
            return Ok(None);
        }
        let action = params.get("a").map(String::as_str).unwrap_or("t");
        let mut id = number(&params, "i", 0)?;
        let image_number = number(&params, "I", 0)?;
        if id != 0 && image_number != 0 {
            bail!("image id and number are mutually exclusive");
        }
        if action == "q" && id == 0 {
            bail!("image id required for query");
        }
        if image_number > 0 && matches!(action, "p" | "d") {
            id = self
                .image_by_number(image_number)
                .context("image number not found")?;
            *response_id = id;
        }
        if action == "d" {
            let mode = params.get("d").map(String::as_str).unwrap_or("a");
            let selector = mode.to_ascii_lowercase();
            let placement = number(&params, "p", 0)?;
            let affected = match selector.as_str() {
                "a" => self.delete_matching(|entry| {
                    entry.alternate == alternate && !entry.virtual_placement
                }),
                "i" | "n" => {
                    if selector == "n" && image_number == 0 {
                        bail!("image number required");
                    }
                    let mut affected = self.delete_matching(|entry| {
                        entry.placement.image == id
                            && (placement == 0 || entry.placement.placement == placement)
                    });
                    // A named image can be deleted even before it is placed.
                    affected.insert(id);
                    affected
                }
                "p" | "q" | "c" | "x" | "y" | "z" => {
                    let cell_col = number(&params, "x", 0)?;
                    let cell_row = number(&params, "y", 0)?;
                    let z = signed_number(&params, "z", 0)?;
                    let cursor_col = u32::from(cursor.1) + 1;
                    let cursor_row = u32::from(cursor.0) + 1;
                    let selected: HashSet<_> = self
                        .placements_with_screen(alternate, 0, screen)
                        .iter()
                        .filter(|p| match selector.as_str() {
                            "c" => intersects(p, cursor_col, cursor_row),
                            "p" => intersects(p, cell_col, cell_row),
                            "q" => intersects(p, cell_col, cell_row) && p.z == z,
                            "x" => intersects_column(p, cell_col),
                            "y" => intersects_row(p, cell_row),
                            "z" => p.z == z,
                            _ => false,
                        })
                        .map(|p| (p.image, p.placement))
                        .collect();
                    self.delete_matching(|entry| {
                        !entry.virtual_placement
                            && selected
                                .contains(&(entry.placement.image, entry.placement.placement))
                    })
                }
                "r" => {
                    let first = number(&params, "x", 0)?;
                    let last = number(&params, "y", 0)?;
                    if first == 0 || first > last {
                        bail!("invalid image id range");
                    }
                    let mut affected = self
                        .delete_matching(|entry| (first..=last).contains(&entry.placement.image));
                    affected.extend(
                        self.images
                            .keys()
                            .filter(|id| (first..=last).contains(id))
                            .copied(),
                    );
                    affected
                }
                _ => bail!("unsupported delete selector"),
            };
            if mode.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
                self.remove_image_data(&affected);
            }
            return Ok(None);
        }
        if !matches!(action, "t" | "T" | "p" | "q") {
            bail!("unsupported graphics action");
        }
        if action != "p" {
            let bytes = media::load(&params, &encoded)?;
            let format = number(&params, "f", 32)?;
            let (width, height) = dimensions(format, &params, &bytes)?;
            if action == "q" {
                return Ok(None);
            }
            if id == 0 {
                self.next_image = self.next_image.saturating_add(1).max(1);
                while self.images.contains_key(&self.next_image) {
                    self.next_image = self
                        .next_image
                        .checked_add(1)
                        .context("image id space exhausted")?;
                }
                id = self.next_image;
            }
            // `a=T` transfers and places in one command. Validate every
            // placement failure before replacing image data so a rejected
            // crop/quota request is atomic from the pane's perspective.
            if action == "T" {
                let spec = PlacementSpec::parse(&params, width, height)?;
                let placement = number(&params, "p", 0)?;
                if let Some(parent) = spec.parent {
                    self.validate_relative((id, placement), parent)?;
                }
                if self
                    .placements
                    .iter()
                    .filter(|entry| entry.placement.image != id)
                    .count()
                    >= MAX_PLACEMENTS
                {
                    bail!("pane placement quota exceeded");
                }
            }
            let encoded = STANDARD.encode(bytes);
            let stored = self
                .images
                .iter()
                .filter(|(other, _)| **other != id)
                .map(|(_, image)| image.data.len())
                .sum::<usize>();
            if (!self.images.contains_key(&id) && self.images.len() >= MAX_IMAGES)
                || stored + encoded.len() > MAX_STORE * 4 / 3
            {
                bail!("pane image quota exceeded; delete unused images");
            }
            // Re-transmitting an image deletes its old placements. Remove
            // descendants too: relative placements have their parent's
            // lifetime, even when they belong to another image.
            let deleted_images = self.delete_matching(|entry| entry.placement.image == id);
            let used_images: HashSet<_> = self
                .placements
                .iter()
                .map(|entry| entry.placement.image)
                .collect();
            self.images.retain(|image, _| {
                *image == id || !deleted_images.contains(image) || used_images.contains(image)
            });
            self.revision += 1;
            if image_number > 0 {
                self.image_numbers.insert(id, image_number);
                *response_id = id;
            } else {
                self.image_numbers.remove(&id);
            }
            self.images.insert(
                id,
                ImageData {
                    id,
                    revision: self.revision,
                    format,
                    width,
                    height,
                    data: encoded,
                },
            );
        }
        if matches!(action, "p" | "T") {
            let image = self.images.get(&id).context("image not found")?;
            let spec = PlacementSpec::parse(&params, image.width, image.height)?;
            let mut placement = number(&params, "p", 0)?;
            if placement == 0 {
                self.next_placement = self.next_placement.wrapping_add(1).max(1);
                placement = self.next_placement;
            }
            if let Some(parent) = spec.parent {
                self.validate_relative((id, placement), parent)?;
            }
            let replaces = self
                .placements
                .iter()
                .any(|entry| entry.placement.image == id && entry.placement.placement == placement);
            if !replaces && self.placements.len() >= MAX_PLACEMENTS {
                bail!("pane placement quota exceeded");
            }
            self.placements.retain(|entry| {
                entry.placement.image != id || entry.placement.placement != placement
            });
            self.placements.push(StoredPlacement {
                alternate,
                placement: ImagePlacement {
                    image: id,
                    revision: image.revision,
                    placement,
                    row: i32::from(cursor.0),
                    col: cursor.1,
                    cols: spec.cols,
                    rows: spec.rows,
                    source_x: spec.x,
                    source_y: spec.y,
                    source_width: spec.width,
                    source_height: spec.height,
                    x_offset: spec.x_offset,
                    y_offset: spec.y_offset,
                    z: spec.z,
                },
                virtual_placement: spec.virtual_placement,
                parent: spec.parent,
                relative_offset: spec.relative_offset,
            });
            if spec.advance {
                return Ok(Some((spec.rows, spec.cols)));
            }
        }
        Ok(None)
    }
}

/// Validate a placement completely before changing the retained image/placement.
struct PlacementSpec {
    cols: u16,
    rows: u16,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    x_offset: u32,
    y_offset: u32,
    z: i32,
    advance: bool,
    virtual_placement: bool,
    parent: Option<(u32, u32)>,
    relative_offset: (i32, i32),
}
impl PlacementSpec {
    fn parse(params: &BTreeMap<String, String>, width: u32, height: u32) -> Result<Self> {
        let x_offset = number(params, "X", 0)?;
        let y_offset = number(params, "Y", 0)?;
        if x_offset >= 8 || y_offset >= 16 {
            bail!("pixel placement offsets must fit within a cell");
        }
        let x = number(params, "x", 0)?.min(width);
        let y = number(params, "y", 0)?.min(height);
        let crop_width = number(params, "w", 0)?;
        let crop_height = number(params, "h", 0)?;
        let width = if crop_width == 0 {
            width - x
        } else {
            crop_width.min(width - x)
        };
        let height = if crop_height == 0 {
            height - y
        } else {
            crop_height.min(height - y)
        };
        if width == 0 || height == 0 {
            bail!("crop lies outside image");
        }
        // PTY cell pixels are unspecified. Use 8x16 consistently when either
        // dimension is automatic, preserving the crop's aspect ratio.
        let cols = number(params, "c", 0)?;
        let rows = number(params, "r", 0)?;
        let (cols, rows) = match (cols, rows) {
            (0, 0) => (u64::from(width.div_ceil(8)), u64::from(height.div_ceil(16))),
            (0, rows) => (
                (u64::from(rows) * 2 * u64::from(width)).div_ceil(u64::from(height)),
                u64::from(rows),
            ),
            (cols, 0) => (
                u64::from(cols),
                (u64::from(cols) * u64::from(height)).div_ceil(2 * u64::from(width)),
            ),
            (cols, rows) => (u64::from(cols), u64::from(rows)),
        };
        let virtual_placement = number(params, "U", 0)? != 0;
        let parent = if params.contains_key("P") {
            let parent = (number(params, "P", 0)?, number(params, "Q", 0)?);
            (parent.0 != 0).then_some(parent)
        } else {
            None
        };
        if virtual_placement && parent.is_some() {
            bail!("virtual placement cannot refer to a parent");
        }
        Ok(Self {
            cols: cols.clamp(1, u64::from(u16::MAX)) as u16,
            rows: rows.clamp(1, u64::from(u16::MAX)) as u16,
            x,
            y,
            width,
            height,
            x_offset,
            y_offset,
            z: params
                .get("z")
                .map(|value| value.parse())
                .transpose()
                .context("invalid z index")?
                .unwrap_or(0),
            advance: !virtual_placement && parent.is_none() && number(params, "C", 0)? == 0,
            virtual_placement,
            parent,
            relative_offset: (
                signed_number(params, "H", 0)?,
                signed_number(params, "V", 0)?,
            ),
        })
    }
}

fn number(params: &BTreeMap<String, String>, key: &str, default: u32) -> Result<u32> {
    params
        .get(key)
        .map(|value| value.parse().with_context(|| format!("invalid {key}")))
        .unwrap_or(Ok(default))
}

fn signed_number(params: &BTreeMap<String, String>, key: &str, default: i32) -> Result<i32> {
    params
        .get(key)
        .map(|value| value.parse().with_context(|| format!("invalid {key}")))
        .unwrap_or(Ok(default))
}

fn intersects(p: &ImagePlacement, col: u32, row: u32) -> bool {
    col > 0
        && row > 0
        && (i32::from(p.col)..i32::from(p.col) + i32::from(p.cols)).contains(&(col as i32 - 1))
        && (p.row..p.row + i32::from(p.rows)).contains(&(row as i32 - 1))
}

fn intersects_column(p: &ImagePlacement, col: u32) -> bool {
    col > 0 && (i32::from(p.col)..i32::from(p.col) + i32::from(p.cols)).contains(&(col as i32 - 1))
}

fn intersects_row(p: &ImagePlacement, row: u32) -> bool {
    row > 0 && (p.row..p.row + i32::from(p.rows)).contains(&(row as i32 - 1))
}

fn dimensions(format: u32, params: &BTreeMap<String, String>, bytes: &[u8]) -> Result<(u32, u32)> {
    let (width, height) = match format {
        24 | 32 => (number(params, "s", 0)?, number(params, "v", 0)?),
        100 => crate::image_paste::validate_png(bytes)?,
        _ => bail!("expected RGB, RGBA or PNG image"),
    };
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > 16 * 1024 * 1024
    {
        bail!("image dimensions exceed limits");
    }
    if format != 100
        && bytes.len() as u64 != u64::from(width) * u64::from(height) * u64::from(format / 8)
    {
        bail!("pixel count does not match dimensions");
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_preserves_images_revisions_and_unfinished_chunked_frames() {
        let mut store = Store::default();
        store.command(b"a=T,f=24,s=1,v=1,i=7;AAAA", (2, 3), false);
        let original = store.placements(false, 0);
        store.command(b"a=T,f=24,s=2,v=1,i=42,p=8,m=1;AAAA", (0, 0), false);
        let mut decoder = Decoder::default();
        assert!(decoder.feed(b"\x1b_Gm=0;AA").is_empty());
        let state = HandoffState::capture(&store, &decoder, &Tracker::default()).unwrap();
        let state: HandoffState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        state.validate().unwrap();
        let (mut restored, mut decoder, _) = state.restore();
        assert_eq!(restored.placements(false, 0), original);
        assert_eq!(
            restored.image(7, original[0].revision).unwrap().data,
            "AAAA"
        );
        let tokens = decoder.feed(b"AA\x1b\\");
        assert_eq!(tokens.len(), 1);
        let Token::Graphics(frame) = &tokens[0] else {
            panic!("expected resumed frame")
        };
        assert_eq!(
            restored.command(frame, (4, 5), false).reply,
            b"\x1b_Gi=42,p=8;OK\x1b\\"
        );
        let placements = restored.placements(false, 0);
        let added = placements.iter().find(|p| p.image == 42).unwrap();
        assert!(added.revision > original[0].revision);
        assert_eq!(restored.image(42, added.revision).unwrap().data, "AAAAAAAA");
    }

    #[test]
    fn handoff_continues_partial_utf8_and_terminal_controls_at_every_byte() {
        let input = "start 界\x1b[31mred\x1b[0m\x1b]2;pane title\x07 done".as_bytes();
        let mut expected = vt100::Parser::new(12.try_into().unwrap(), 80.try_into().unwrap(), 100);
        expected.process(input);
        for split in 0..input.len() {
            let mut source =
                vt100::Parser::new(12.try_into().unwrap(), 80.try_into().unwrap(), 100);
            let mut tracker = Tracker::default();
            let mut store = Store::default();
            let mut decoder = Decoder::default();
            for token in decoder.feed(&input[..split]) {
                let Token::Text(text) = token else {
                    panic!("expected text")
                };
                for byte in text {
                    tracker.feed(byte, source.screen(), &mut store);
                    source.process(&[byte]);
                }
            }
            let state = HandoffState::capture(&store, &decoder, &tracker).unwrap();
            let state: HandoffState =
                serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
            let mut restored =
                vt100::Parser::new(12.try_into().unwrap(), 80.try_into().unwrap(), 100);
            restored.process(source.screen().state_formatted().as_bytes());
            restored.process(state.pending_text());
            let (mut store, mut decoder, mut tracker) = state.restore();
            for token in decoder.feed(&input[split..]) {
                let Token::Text(text) = token else {
                    panic!("expected text")
                };
                for byte in text {
                    tracker.feed(byte, restored.screen(), &mut store);
                    restored.process(&[byte]);
                }
            }
            assert_eq!(
                restored.screen().contents(),
                expected.screen().contents(),
                "split {split}"
            );
            assert_eq!(
                restored.screen().contents_formatted(),
                expected.screen().contents_formatted(),
                "style at split {split}"
            );
        }
    }

    #[test]
    fn handoff_rebuilds_unicode_and_sgr_parsers_at_every_byte() {
        let unicode = "\x1b]0;title\x1b\\\x1bP+qquery\x1b\\\x1b[3\n1m\u{10eeee}\u{0305}".as_bytes();
        for split in 0..unicode.len() {
            let mut source = UnicodeTracker::default();
            for &byte in &unicode[..split] {
                source.feed(byte);
            }
            let state: UnicodeHandoff = serde_json::from_slice(
                &serde_json::to_vec(&source.capture_handoff().unwrap()).unwrap(),
            )
            .unwrap();
            let mut restored = UnicodeTracker::restore_handoff(state);
            let mut expected = Vec::new();
            for &byte in &unicode[split..] {
                expected.extend(source.feed(byte));
            }
            let mut actual = Vec::new();
            for &byte in &unicode[split..] {
                actual.extend(restored.feed(byte));
            }
            assert_eq!(expected, actual, "unicode split {split}");
        }
        let sgr = b"\x1b]0;title\x1b\\\x1bP+qquery\x1b\\\x1b[38;\n5;42;58;5;9m";
        for split in 0..sgr.len() {
            let mut source = VirtualStyle::default();
            for &byte in &sgr[..split] {
                source.feed(byte);
            }
            let state: VirtualStyleHandoff = serde_json::from_slice(
                &serde_json::to_vec(&source.capture_handoff().unwrap()).unwrap(),
            )
            .unwrap();
            let mut restored = VirtualStyle::restore_handoff(state);
            for &byte in &sgr[split..] {
                source.feed(byte);
                restored.feed(byte);
            }
            assert_eq!(source.ids(), restored.ids(), "SGR split {split}");
            assert_eq!(source.ids(), Some((42, 9)));
        }
    }

    #[test]
    fn unrelated_complete_controls_do_not_accumulate_handoff_state() {
        let mut unicode = UnicodeTracker::default();
        let mut style = VirtualStyle::default();
        for _ in 0..1024 {
            for &byte in b"\x1b]0;window title\x1b\\\x1bP+qquery\x1b\\" {
                unicode.feed(byte);
                style.feed(byte);
            }
        }
        assert!(unicode.capture_handoff().unwrap().pending.is_empty());
        assert!(style.capture_handoff().unwrap().pending.is_empty());
        for &byte in b"\x1b[38;5;42m\x1bc" {
            style.feed(byte);
        }
        assert_eq!(style.ids(), None);
    }

    #[test]
    fn decoder_handles_every_split_without_exposing_graphics_to_text() {
        let input = b"before\x1b_Ga=T,f=24,s=1,v=1,i=7;AAAA\x1b\\after";
        for split in 0..input.len() {
            let mut decoder = Decoder::default();
            let tokens = decoder
                .feed(&input[..split])
                .into_iter()
                .chain(decoder.feed(&input[split..]));
            let mut text = Vec::new();
            let mut frames = Vec::new();
            for token in tokens {
                match token {
                    Token::Text(t) => text.extend(t),
                    Token::Graphics(frame) => frames.push(frame),
                    Token::Invalid => panic!("unexpected invalid frame"),
                }
            }
            assert_eq!(text, b"beforeafter");
            assert_eq!(frames, vec![b"a=T,f=24,s=1,v=1,i=7;AAAA".to_vec()]);
        }
    }

    #[test]
    fn zlib_pixels_are_decoded_before_storage_and_bad_streams_preserve_the_image() {
        let mut store = Store::default();
        // RFC 1950 stream for the RGB pixel [255, 0, 128].
        let compressed = b"eJz7z9AAAAOAAYA=".to_vec();
        let mut frame = b"a=T,f=24,s=1,v=1,i=7,o=z;".to_vec();
        frame.extend(&compressed);
        let outcome = store.command(&frame, (0, 0), false);
        assert_eq!(outcome.reply, b"\x1b_Gi=7;OK\x1b\\");
        let placement = &store.placements(false, 0)[0];
        assert_eq!(store.image(7, placement.revision).unwrap().data, "/wCA");
        let revision = placement.revision;
        let outcome = store.command(b"a=T,f=24,s=1,v=1,i=7,o=z;AAAA", (0, 0), false);
        assert!(String::from_utf8_lossy(&outcome.reply).contains("EINVAL"));
        assert_eq!(store.image(7, revision).unwrap().data, "/wCA");
    }

    #[test]
    fn file_images_read_only_the_requested_range_and_temporary_images_are_removed() {
        let path = std::env::temp_dir().join(format!(
            "tty-graphics-protocol-kodade-{}-file",
            std::process::id()
        ));
        std::fs::write(&path, [99, 255, 0, 128, 77]).unwrap();
        let encoded = STANDARD.encode(path.to_str().unwrap());
        let mut store = Store::default();
        let frame = format!("a=T,t=f,f=24,s=1,v=1,i=9,O=1,S=3;{encoded}");
        assert_eq!(
            store.command(frame.as_bytes(), (0, 0), false).reply,
            b"\x1b_Gi=9;OK\x1b\\"
        );
        let placement = &store.placements(false, 0)[0];
        assert_eq!(store.image(9, placement.revision).unwrap().data, "/wCA");
        assert!(path.exists(), "regular file belongs to the sender");
        let frame = frame.replace("t=f", "t=t");
        assert_eq!(
            store.command(frame.as_bytes(), (0, 0), false).reply,
            b"\x1b_Gi=9;OK\x1b\\"
        );
        assert!(!path.exists(), "protocol temporary file was consumed");
    }

    #[cfg(unix)]
    #[test]
    fn shared_memory_pixels_are_loaded_and_the_object_is_unlinked() {
        use std::os::fd::FromRawFd;
        let name = std::ffi::CString::new(format!("/kc-graphics-{}", std::process::id())).unwrap();
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let _file = unsafe { std::fs::File::from_raw_fd(fd) };
        assert_eq!(unsafe { libc::ftruncate(fd, 5) }, 0);
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                5,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED);
        unsafe {
            std::ptr::copy_nonoverlapping(
                [99u8, 255, 0, 128, 77].as_ptr(),
                mapping.cast::<u8>(),
                5,
            );
            libc::munmap(mapping, 5);
        }
        let frame = format!(
            "a=T,t=s,f=24,s=1,v=1,i=19,O=1,S=3;{}",
            STANDARD.encode(name.as_bytes())
        );
        let mut store = Store::default();
        let outcome = store.command(frame.as_bytes(), (0, 0), false);
        // Always clean the fixture even on the red run.
        let reopened = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        if reopened >= 0 {
            unsafe {
                libc::close(reopened);
                libc::shm_unlink(name.as_ptr());
            }
        }
        assert_eq!(outcome.reply, b"\x1b_Gi=19;OK\x1b\\");
        let placement = &store.placements(false, 0)[0];
        assert_eq!(store.image(19, placement.revision).unwrap().data, "/wCA");
        assert_eq!(reopened, -1, "shared memory was consumed");
    }

    #[test]
    fn malformed_and_oversized_compression_cannot_replace_valid_pixels() {
        use std::io::Write;
        let mut store = Store::default();
        store.command(b"a=T,f=24,s=1,v=1,i=7;/wCA", (0, 0), false);
        let revision = store.placements(false, 0)[0].revision;
        let compressed = STANDARD.decode("eJz7z9AAAAOAAYA=").unwrap();
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&vec![0; MAX_IMAGE + 1]).unwrap();
        let bomb = encoder.finish().unwrap();
        for bytes in [compressed[..compressed.len() - 1].to_vec(), bomb] {
            let frame = format!("a=T,f=24,s=1,v=1,i=7,o=z;{}", STANDARD.encode(bytes));
            assert!(
                String::from_utf8_lossy(&store.command(frame.as_bytes(), (0, 0), false).reply)
                    .contains("EINVAL")
            );
            assert_eq!(store.image(7, revision).unwrap().data, "/wCA");
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_media_follows_symlinks_but_refuses_special_files_and_unsafe_cleanup() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("kc-graphics-files-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("pixels");
        let link = root.join("linked-pixels");
        let fifo = root.join("pipe");
        std::fs::write(&source, [255, 0, 128]).unwrap();
        symlink(&source, &link).unwrap();
        let mut store = Store::default();
        let request = |mode: &str, path: &std::path::Path| {
            format!(
                "a=q,t={mode},f=24,s=1,v=1,i=7;{}",
                STANDARD.encode(path.to_str().unwrap())
            )
        };
        assert_eq!(
            store
                .command(request("f", &link).as_bytes(), (0, 0), false)
                .reply,
            b"\x1b_Gi=7;OK\x1b\\"
        );
        assert!(String::from_utf8_lossy(
            &store
                .command(request("t", &source).as_bytes(), (0, 0), false)
                .reply
        )
        .contains("EINVAL"));
        assert!(
            source.exists(),
            "a file without the protocol marker is never deleted"
        );
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(String::from_utf8_lossy(
            &store
                .command(request("f", &fifo).as_bytes(), (0, 0), false)
                .reply
        )
        .contains("EINVAL"));
        assert!(String::from_utf8_lossy(
            &store
                .command(
                    request("f", std::path::Path::new("/dev/zero")).as_bytes(),
                    (0, 0),
                    false
                )
                .reply
        )
        .contains("EINVAL"));
        assert!(
            store.placements(false, 0).is_empty(),
            "query leaves no placement"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_numbers_allocate_ids_and_target_the_newest_transmission() {
        let mut store = Store::default();
        let first = store.command(b"a=t,f=24,s=1,v=1,I=13;/wCA", (0, 0), false);
        assert_eq!(first.reply, b"\x1b_Gi=1,I=13;OK\x1b\\");
        assert_eq!(
            store
                .command(b"a=t,f=24,s=1,v=1,I=13;AP9A", (0, 0), false)
                .reply,
            b"\x1b_Gi=2,I=13;OK\x1b\\"
        );
        assert_eq!(
            store.command(b"a=p,I=13,p=7,C=1", (2, 3), false).reply,
            b"\x1b_Gi=2,I=13,p=7;OK\x1b\\"
        );
        assert_eq!(store.placements(false, 0)[0].image, 2);
        store.command(b"a=d,d=N,I=13", (0, 0), false);
        assert!(store.placements(false, 0).is_empty());
        assert_eq!(store.image(1, 1).unwrap().data, "/wCA");
        assert!(store.image(2, 2).is_err());
        assert!(
            String::from_utf8_lossy(&store.command(b"a=p,i=1,I=13", (0, 0), false).reply)
                .contains("EINVAL")
        );
    }

    #[test]
    fn uppercase_deletion_preserves_unrelated_assets_and_cancels_partial_uploads() {
        let mut store = Store::default();
        store.command(b"a=t,f=24,s=1,v=1,i=8;AP9A", (0, 0), false);
        store.command(b"a=T,f=24,s=1,v=1,i=7,C=1;/wCA", (0, 0), false);
        assert!(store
            .command(b"a=t,f=24,s=1,v=1,I=42,m=1;/w", (0, 0), false)
            .reply
            .is_empty());
        store.command(b"a=d,d=I,i=7", (0, 0), false);
        assert!(store.placements(false, 0).is_empty());
        assert!(store.image(7, 2).is_err());
        assert_eq!(store.image(8, 1).unwrap().data, "AP9A");
        assert!(
            String::from_utf8_lossy(&store.command(b"a=p,I=42", (0, 0), false).reply)
                .contains("EINVAL")
        );
        assert_eq!(
            store
                .command(b"a=t,f=24,s=1,v=1,I=42,m=1;/w", (0, 0), false)
                .reply,
            b""
        );
        assert_eq!(
            store.command(b"m=0;CA", (0, 0), false).reply,
            b"\x1b_Gi=1,I=42;OK\x1b\\"
        );
        assert_eq!(store.image(1, 3).unwrap().data, "/wCA");
    }

    #[test]
    fn direct_images_are_retained_queried_scrolled_and_deleted() {
        let mut store = Store::default();
        let result = store.command(b"a=T,f=24,s=1,v=1,i=7,c=2,r=3;AAAA", (4, 5), false);
        assert!(String::from_utf8_lossy(&result.reply).contains("OK"));
        assert_eq!(result.advance, Some((3, 2)));
        let placement = store.placements(false, 0).remove(0);
        assert_eq!(placement.row, 4);
        assert_eq!(store.image(7, placement.revision).unwrap().data, "AAAA");
        store.scroll(false, 2, 24);
        assert_eq!(store.placements(false, 1)[0].row, 3);
        store.command(b"a=d,d=I,i=7", (0, 0), false);
        assert!(store.placements(false, 0).is_empty());
        assert!(store.image(7, placement.revision).is_err());
    }

    #[test]
    fn queries_do_not_store_and_untrusted_paths_are_never_read() {
        let mut store = Store::default();
        assert!(String::from_utf8_lossy(
            &store
                .command(b"a=q,f=24,s=1,v=1,i=7;AAAA", (0, 0), false)
                .reply
        )
        .contains("OK"));
        assert!(store.images.is_empty());
        assert!(String::from_utf8_lossy(
            &store
                .command(b"a=t,t=f,i=7;L2V0Yy9wYXNzd2Q=", (0, 0), false)
                .reply
        )
        .contains("EINVAL"));
        assert!(store.images.is_empty());
    }

    #[test]
    fn chunked_transfers_reply_once_with_the_original_identity_and_quiet_mode() {
        let mut store = Store::default();
        assert!(store
            .command(b"a=T,f=24,s=2,v=1,i=42,p=8,m=1;AAAA", (0, 0), false)
            .reply
            .is_empty());
        let done = store.command(b"m=0;AAAA", (0, 0), false);
        assert_eq!(done.reply, b"\x1b_Gi=42,p=8;OK\x1b\\");
        assert_eq!(store.placements(false, 0)[0].image, 42);
        assert!(store
            .command(b"a=t,f=24,s=2,v=1,i=43,q=2,m=1;AAAA", (0, 0), false)
            .reply
            .is_empty());
        assert!(store.command(b"m=0;AAAA", (0, 0), false).reply.is_empty());
    }

    #[test]
    fn automatic_dimensions_preserve_aspect_and_invalid_replacement_is_atomic() {
        let mut store = Store::default();
        let pixels = STANDARD.encode(vec![0; 64 * 32 * 3]);
        store.command(
            format!("a=T,f=24,s=64,v=32,i=7,p=1,c=0,r=0;{pixels}").as_bytes(),
            (0, 0),
            false,
        );
        assert_eq!(
            (
                store.placements[0].placement.cols,
                store.placements[0].placement.rows
            ),
            (8, 2)
        );
        store.command(b"a=p,i=7,p=1,c=4,r=0", (0, 0), false);
        assert_eq!(
            (
                store.placements[0].placement.cols,
                store.placements[0].placement.rows
            ),
            (4, 1)
        );
        let before = store.placements.clone();
        for invalid in ["z=bad", "C=bad", "X=8"] {
            let result = store.command(format!("a=p,i=7,p=1,{invalid}").as_bytes(), (3, 3), false);
            assert!(String::from_utf8_lossy(&result.reply).contains("EINVAL"));
            assert_eq!(store.placements, before);
        }
    }

    #[test]
    fn rejected_transfer_and_place_keeps_existing_image_and_quota_unchanged() {
        let mut store = Store::default();
        store.command(b"a=t,f=24,s=1,v=1,i=7;AAAA", (0, 0), false);
        let original = store.images.get(&7).cloned().unwrap();
        let bad_crop = store.command(b"a=T,f=24,s=1,v=1,i=7,x=1;AAAA", (0, 0), false);
        assert!(String::from_utf8_lossy(&bad_crop.reply).contains("EINVAL"));
        assert_eq!(store.images.get(&7), Some(&original));

        for placement in 1..=MAX_PLACEMENTS as u32 {
            store.command(format!("a=p,i=7,p={placement};").as_bytes(), (0, 0), false);
        }
        let quota = store.command(b"a=T,f=24,s=1,v=1,i=9;AAAA", (0, 0), false);
        assert!(String::from_utf8_lossy(&quota.reply).contains("EINVAL"));
        assert!(!store.images.contains_key(&9));
        assert_eq!(store.placements.len(), MAX_PLACEMENTS);
    }

    #[test]
    fn placements_keep_pixel_offsets_and_relative_geometry() {
        let mut store = Store::default();
        store.command(b"a=t,f=24,s=2,v=1,i=7;AAAAAAAA", (1, 2), false);
        store.command(b"a=p,i=7,p=3,c=1,r=1,X=3,Y=4,C=1", (1, 2), false);
        store.command(b"a=t,f=24,s=1,v=1,i=8;AAAA", (0, 0), false);
        let relative = store.command(b"a=p,i=8,p=4,P=7,Q=3,H=2,V=-1", (9, 9), false);
        assert!(String::from_utf8_lossy(&relative.reply).contains("OK"));
        assert_eq!(
            relative.advance, None,
            "relative placements never move the cursor"
        );
        let placements = store.placements(false, 0);
        let base = placements.iter().find(|p| p.image == 7).unwrap();
        assert_eq!((base.x_offset, base.y_offset), (3, 4));
        let child = placements.iter().find(|p| p.image == 8).unwrap();
        assert_eq!((child.col, child.row), (4, 0));
        assert_eq!(
            store.command(b"a=p,i=8,p=5,P=99,Q=1", (0, 0), false).reply,
            b"\x1b_Gi=8,p=5;EINVAL:ENOPARENT: parent placement not found\x1b\\"
        );
    }

    #[test]
    fn deletion_uses_relative_geometry_after_parent_replacement() {
        let mut store = Store::default();
        for id in 7..=9 {
            store.command(
                format!("a=t,f=24,s=1,v=1,i={id};AAAA").as_bytes(),
                (0, 0),
                false,
            );
        }
        store.command(b"a=p,i=7,p=1,c=1,r=1,C=1", (1, 2), false);
        store.command(b"a=p,i=8,p=1,c=1,r=1,P=7,Q=1,H=2,V=1", (9, 9), false);
        store.command(b"a=p,i=9,p=1,c=1,r=1,P=8,Q=1,H=1,V=1", (9, 9), false);
        // Replacing an intermediate placement moves it after its child in storage.
        store.command(b"a=p,i=8,p=1,c=1,r=1,P=7,Q=1,H=3,V=1", (9, 9), false);
        let visible = store.placements(false, 0);
        let grandchild = visible.iter().find(|p| p.image == 9).unwrap();
        assert_eq!((grandchild.col, grandchild.row), (6, 3));
        store.command(b"a=d,d=P,x=7,y=4", (0, 0), false);
        assert!(store.images.contains_key(&7));
        assert!(store.images.contains_key(&8));
        assert!(!store.images.contains_key(&9));
        assert_eq!(store.placements(false, 0).len(), 2);
    }

    #[test]
    fn retransmitting_a_parent_removes_relative_children() {
        let mut store = Store::default();
        store.command(b"a=t,f=24,s=1,v=1,i=7;AAAA", (0, 0), false);
        store.command(b"a=p,i=7,p=1,C=1", (0, 0), false);
        store.command(b"a=t,f=24,s=1,v=1,i=8;AAAA", (0, 0), false);
        store.command(b"a=p,i=8,p=1,P=7,Q=1", (4, 4), false);
        store.command(b"a=t,f=24,s=1,v=1,i=9;AAAA", (0, 0), false);
        assert!(store.placements(false, 0).iter().any(|p| p.image == 8));

        store.command(b"a=t,f=24,s=1,v=1,i=7;AQID", (0, 0), false);

        assert!(store.placements(false, 0).is_empty());
        assert!(!store.images.contains_key(&8));
        assert!(store.images.contains_key(&9));
    }

    #[test]
    fn unicode_placeholder_activates_virtual_placement_without_cursor_movement() {
        let mut store = Store::default();
        store.command(b"a=t,f=24,s=2,v=1,i=7;AAAAAAAA", (0, 0), false);
        let created = store.command(b"a=p,i=7,p=3,U=1,c=2,r=1", (4, 4), false);
        assert_eq!(created.advance, None);
        store.command(b"a=p,i=7,p=4,U=1,c=2,r=1", (4, 4), false);
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(1).unwrap(),
            std::num::NonZeroU16::new(2).unwrap(),
            0,
        );
        let mut pending = Vec::new();
        let normalized = normalize_unicode_placeholders(
            &mut pending,
            "\x1b[1;1H\u{10eeee}\x1b[1;2H\u{10eeee}".as_bytes(),
        );
        parser.process(&normalized.iter().map(|item| item.byte).collect::<Vec<_>>());
        store.record_virtual_cell(false, 0, 0, Some((7, 3)));
        store.record_virtual_cell(false, 0, 1, Some((7, 4)));
        let placements = store.placements_with_screen(false, 0, Some(parser.screen()));
        assert_eq!(placements.len(), 2);
        assert_eq!(
            (placements[0].source_width, placements[1].source_width),
            (1, 1)
        );
        assert_ne!(placements[0].placement, placements[1].placement);
        assert!(String::from_utf8_lossy(
            &store
                .command(b"a=p,i=7,p=4,U=1,P=7,Q=3", (0, 0), false)
                .reply
        )
        .contains("EINVAL"));
    }

    #[test]
    fn virtual_style_tracks_underline_color_and_sgr_reset() {
        let mut style = VirtualStyle::default();
        for byte in b"\x1b[38;2;0;0;7;58;2;0;0;3m" {
            style.feed(*byte);
        }
        assert_eq!(style.ids(), Some((7, 3)));
        for byte in b"\x1b[59m" {
            style.feed(*byte);
        }
        assert_eq!(style.ids(), Some((7, 0)));
    }

    #[test]
    fn virtual_cells_scroll_and_clear_when_text_overwrites_them() {
        let mut store = Store::default();
        store.record_virtual_cell(false, 3, 2, Some((7, 4)));
        store.scroll(false, 2, 24);
        assert_eq!(
            store
                .virtual_cells
                .get(&(false, 1, 2))
                .map(|cell| (cell.image_low, cell.placement)),
            Some((7, Some(4)))
        );
        store.clear_virtual_cell(false, 1, 2);
        assert!(store.virtual_cells.is_empty());
    }

    #[test]
    fn virtual_cells_follow_character_and_regional_line_edits() {
        let mut store = Store::default();
        for (row, col) in [(1, 0), (1, 2), (2, 1), (3, 1)] {
            store.record_virtual_cell(false, row, col, Some((7, 4)));
        }

        store.insert_chars(false, 1, 1, 2, 5);
        assert!(store.virtual_cells.contains_key(&(false, 1, 0)));
        assert!(store.virtual_cells.contains_key(&(false, 1, 4)));
        store.delete_chars(false, 1, 0, 1, 5);
        assert!(!store.virtual_cells.contains_key(&(false, 1, 0)));
        assert!(store.virtual_cells.contains_key(&(false, 1, 3)));

        store.insert_lines(false, 2, 3, 1);
        assert!(store.virtual_cells.contains_key(&(false, 3, 1)));
        assert!(!store.virtual_cells.contains_key(&(false, 2, 1)));
        store.delete_lines(false, 2, 3, 1);
        assert!(store.virtual_cells.contains_key(&(false, 2, 1)));
    }

    #[test]
    fn virtual_cells_render_source_crops_on_the_alternate_screen() {
        let mut store = Store::default();
        store.command(
            b"a=t,f=24,s=4,v=2,i=7;AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            (0, 0),
            true,
        );
        store.command(b"a=p,i=7,p=3,U=1,c=2,r=2,x=1,y=0,w=2,h=2", (0, 0), true);
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(2).unwrap(),
            std::num::NonZeroU16::new(2).unwrap(),
            0,
        );
        parser.process(b"\x1b[?1049h");
        store.record_virtual_cell(true, 1, 1, Some((7, 3)));
        store.add_virtual_diacritic(1);
        store.add_virtual_diacritic(1);
        let placement = store
            .placements_with_screen(true, 0, Some(parser.screen()))
            .remove(0);
        assert_eq!((placement.row, placement.col), (1, 1));
        assert_eq!(
            (
                placement.source_x,
                placement.source_y,
                placement.source_width,
                placement.source_height,
            ),
            (2, 1, 1, 1)
        );
    }

    #[test]
    fn delete_selectors_remove_intersecting_placements_and_unused_images() {
        let mut store = Store::default();
        store.command(b"a=T,f=24,s=1,v=1,i=7,p=1,c=2,r=2,z=-1;AAAA", (2, 3), false);
        store.command(b"a=T,f=24,s=1,v=1,i=8,p=2,c=1,r=1,z=2;AAAA", (0, 0), false);
        store.command(b"a=d,d=q,x=4,y=3,z=-1", (0, 0), false);
        assert!(store.placements(false, 0).iter().all(|p| p.image != 7));
        store.command(b"a=d,d=Z,z=2", (0, 0), false);
        assert!(store.image(8, 1).is_err());
    }
}
