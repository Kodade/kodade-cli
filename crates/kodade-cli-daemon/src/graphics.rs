//! A bounded Kitty graphics store. Escape commands never reach the host terminal.

use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use kodade_cli_proto::{ImageData, ImagePlacement};
use unicode_width::UnicodeWidthChar;

const MAX_IMAGE: usize = 8 * 1024 * 1024;
const MAX_STORE: usize = 32 * 1024 * 1024;
const MAX_IMAGES: usize = 16;
const MAX_PLACEMENTS: usize = 64;
const MAX_FRAME: usize = 8192;
const KITTY_UNICODE_PLACEHOLDER: &[u8] = "\u{10eeee}".as_bytes();
const PLACEHOLDER_CELL: u8 = b' ';

pub struct NormalizedByte {
    pub byte: u8,
    pub placeholder: bool,
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
                placeholder: false,
            });
        }
        if pending.as_slice() == KITTY_UNICODE_PLACEHOLDER {
            normalized.push(NormalizedByte {
                byte: PLACEHOLDER_CELL,
                placeholder: true,
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
}

impl VirtualStyle {
    pub fn feed(&mut self, byte: u8) {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, &[byte]);
        self.parser = parser;
    }
    pub fn ids(&self) -> Option<(u32, u32)> {
        self.foreground
            .map(|image| (image, self.underline.unwrap_or(0)))
    }
}

impl vte::Perform for VirtualStyle {
    fn csi_dispatch(&mut self, params: &vte::Params, _: &[u8], ignore: bool, command: char) {
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

#[derive(Default)]
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

struct Transfer {
    params: BTreeMap<String, String>,
    encoded: Vec<u8>,
}

#[derive(Default)]
pub struct Store {
    images: BTreeMap<u32, ImageData>,
    placements: Vec<StoredPlacement>,
    transfer: Option<Transfer>,
    revision: u64,
    next_image: u32,
    next_placement: u32,
    virtual_cells: BTreeMap<(bool, u16, u16), (u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredPlacement {
    alternate: bool,
    placement: ImagePlacement,
    /// A virtual placement gets its visible geometry from Unicode placeholders.
    virtual_placement: bool,
    parent: Option<(u32, u32)>,
    relative_offset: (i32, i32),
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
}

#[derive(Default)]
struct Controls(Option<Control>);
enum Control {
    Print(char),
    Index,
    ReverseIndex,
    Clear,
    Reset,
    Scroll(i32),
    Margins(u16, u16),
}

impl vte::Perform for Controls {
    fn print(&mut self, c: char) {
        self.0 = Some(Control::Print(c));
    }
    fn execute(&mut self, byte: u8) {
        if matches!(byte, 10..=12) {
            self.0 = Some(Control::Index);
        }
    }
    fn esc_dispatch(&mut self, intermediate: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediate.is_empty() {
            return;
        }
        self.0 = match byte {
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
        if ignore || !intermediate.is_empty() {
            return;
        }
        let mut params = params.iter();
        let first = params.next().and_then(|p| p.first()).copied().unwrap_or(0);
        self.0 = match command {
            'J' if first == 2 => Some(Control::Clear),
            'S' => Some(Control::Scroll(i32::from(first.max(1)))),
            'T' => Some(Control::Scroll(-i32::from(first.max(1)))),
            'r' => Some(Control::Margins(
                first,
                params.next().and_then(|p| p.first()).copied().unwrap_or(0),
            )),
            _ => None,
        };
    }
}

impl Tracker {
    pub fn feed(&mut self, byte: u8, screen: &vt100::Screen, store: &mut Store) -> bool {
        self.events.0 = None;
        self.parser.advance(&mut self.events, &[byte]);
        let Some(event) = self.events.0.take() else {
            return false;
        };
        let printed = matches!(event, Control::Print(_));
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
                return false;
            }
            Control::Reset => {
                self.margins = [None, None];
                store.clear(false);
                store.clear(true);
                return false;
            }
            Control::Clear => {
                store.clear(alternate);
                return false;
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
                store.scroll(alternate, shift);
            } else {
                store.scroll_region(alternate, top, bottom, shift);
            }
        }
        printed
    }
}

impl Store {
    fn delete_matching(&mut self, mut matches: impl FnMut(&ImagePlacement) -> bool) {
        let mut deleted = HashSet::new();
        loop {
            let before = self.placements.len();
            self.placements.retain(|entry| {
                let remove = matches(&entry.placement)
                    || entry.parent.is_some_and(|parent| deleted.contains(&parent));
                if remove {
                    deleted.insert((entry.placement.image, entry.placement.placement));
                }
                !remove
            });
            if self.placements.len() == before {
                break;
            }
        }
    }

    fn remove_unused_images(&mut self) {
        self.images.retain(|image, _| {
            self.placements
                .iter()
                .any(|entry| entry.placement.image == *image)
        });
    }

    pub fn clear_virtual_cell(&mut self, alternate: bool, row: u16, col: u16) {
        self.virtual_cells.remove(&(alternate, row, col));
    }

    pub fn record_virtual_cell(
        &mut self,
        alternate: bool,
        row: u16,
        col: u16,
        ids: Option<(u32, u32)>,
    ) {
        if let Some(ids) = ids.filter(|(image, _)| *image != 0) {
            self.virtual_cells.insert((alternate, row, col), ids);
        }
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
            .filter(|entry| entry.alternate == alternate && !entry.virtual_placement)
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
        for row in 0..rows.get() {
            for col in 0..cols.get() {
                let Some(&(image, placement_id)) = self.virtual_cells.get(&(alternate, row, col))
                else {
                    continue;
                };
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
                if u32::from(col) >= grid_cols || u32::from(row) >= grid_rows {
                    continue;
                }
                let source_x = u64::from(p.source_width) * u64::from(col) / u64::from(grid_cols);
                let source_y = u64::from(p.source_height) * u64::from(row) / u64::from(grid_rows);
                p.source_x += source_x as u32;
                p.source_y += source_y as u32;
                p.source_width = (u64::from(p.source_width) * u64::from(col + 1)
                    / u64::from(grid_cols)) as u32
                    - source_x as u32;
                p.source_height = (u64::from(p.source_height) * u64::from(row + 1)
                    / u64::from(grid_rows)) as u32
                    - source_y as u32;
                p.row = i32::from(row).saturating_add(scroll.min(i32::MAX as usize) as i32);
                p.col = col;
                p.cols = 1;
                p.rows = 1;
                visible.push(p);
            }
        }
        visible
    }

    fn resolve_relative(&self, mut visible: Vec<ImagePlacement>) -> Vec<ImagePlacement> {
        let mut resolved: BTreeMap<(u32, u32), ImagePlacement> = visible
            .iter()
            .map(|p| ((p.image, p.placement), p.clone()))
            .collect();
        for entry in &self.placements {
            let Some(parent) = entry.parent else { continue };
            let Some(mut p) = resolved.get(&parent).cloned() else {
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
            resolved.insert((p.image, p.placement), p);
        }
        visible = resolved.into_values().collect();
        visible
    }

    pub fn clear(&mut self, alternate: bool) {
        self.placements.retain(|entry| entry.alternate != alternate);
        self.virtual_cells
            .retain(|(alt, _, _), _| *alt != alternate);
    }

    pub fn scroll(&mut self, alternate: bool, rows: i32) {
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
            .filter_map(|(&(alt, row, col), &ids)| {
                if alt != alternate {
                    return Some(((alt, row, col), ids));
                }
                let row = i32::from(row).saturating_sub(rows);
                (row >= 0 && row <= i32::from(u16::MAX)).then_some(((alt, row as u16, col), ids))
            })
            .collect();
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
    }

    pub fn command(&mut self, frame: &[u8], cursor: (u16, u16), alternate: bool) -> Outcome {
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
        let response = self.transfer.as_ref().map(|t| &t.params).unwrap_or(&params);
        let id = number(response, "i", 0).unwrap_or(0);
        let quiet = number(response, "q", 0).unwrap_or(0);
        let placement = number(response, "p", 0).unwrap_or(0);
        let result = self.apply(params, payload, cursor, alternate);
        let (status, advance) = match result {
            Ok(advance) => ("OK".to_owned(), advance),
            Err(error) => {
                self.transfer = None;
                (format!("EINVAL:{error}"), None)
            }
        };
        let reply =
            if self.transfer.is_some() || quiet == 2 || (quiet == 1 && status == "OK") || id == 0 {
                Vec::new()
            } else {
                let p = if placement > 0 {
                    format!(",p={placement}")
                } else {
                    String::new()
                };
                format!("\x1b_Gi={id}{p};{status}\x1b\\").into_bytes()
            };
        Outcome { reply, advance }
    }

    fn apply(
        &mut self,
        mut params: BTreeMap<String, String>,
        payload: &[u8],
        cursor: (u16, u16),
        alternate: bool,
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
        if action == "d" {
            let mode = params.get("d").map(String::as_str).unwrap_or("a");
            let placement = number(&params, "p", 0)?;
            match mode {
                "a" | "A" => self.clear(alternate),
                "i" | "I" => self.delete_matching(|p| {
                    p.image == id && (placement == 0 || p.placement == placement)
                }),
                "p" | "P" | "q" | "Q" | "c" | "C" | "x" | "X" | "y" | "Y" | "z" | "Z" => {
                    let cell_col = number(&params, "x", 0)?;
                    let cell_row = number(&params, "y", 0)?;
                    let z = signed_number(&params, "z", 0)?;
                    let cursor_col = u32::from(cursor.1) + 1;
                    let cursor_row = u32::from(cursor.0) + 1;
                    self.delete_matching(|p| match mode.to_ascii_lowercase().as_str() {
                        "c" => intersects(p, cursor_col, cursor_row),
                        "p" => intersects(p, cell_col, cell_row),
                        "q" => intersects(p, cell_col, cell_row) && p.z == z,
                        "x" => intersects_column(p, cell_col),
                        "y" => intersects_row(p, cell_row),
                        "z" => p.z == z,
                        _ => false,
                    });
                }
                "r" | "R" => {
                    let first = number(&params, "x", 0)?;
                    let last = number(&params, "y", 0)?;
                    if first == 0 || first > last {
                        bail!("invalid image id range");
                    }
                    self.delete_matching(|p| (first..=last).contains(&p.image));
                }
                "n" | "N" | "f" | "F" => bail!("unsupported delete selector"),
                _ => bail!("unsupported delete selector"),
            }
            if mode.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
                self.remove_unused_images();
            }
            return Ok(None);
        }
        if !matches!(action, "t" | "T" | "p" | "q") {
            bail!("unsupported graphics action");
        }
        if action != "p" {
            if params.get("t").is_some_and(|v| v != "d") || params.contains_key("o") {
                bail!("use uncompressed direct transfer");
            }
            let bytes = STANDARD.decode(&encoded).context("invalid image base64")?;
            if bytes.len() > MAX_IMAGE {
                bail!("image exceeds 8 MiB");
            }
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
            self.revision += 1;
            self.images.insert(
                id,
                ImageData {
                    id,
                    revision: self.revision,
                    format,
                    width,
                    height,
                    data: STANDARD.encode(bytes),
                },
            );
            self.placements.retain(|entry| entry.placement.image != id);
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
            advance: !virtual_placement && number(params, "C", 0)? == 0,
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
    fn direct_images_are_retained_queried_scrolled_and_deleted() {
        let mut store = Store::default();
        let result = store.command(b"a=T,f=24,s=1,v=1,i=7,c=2,r=3;AAAA", (4, 5), false);
        assert!(String::from_utf8_lossy(&result.reply).contains("OK"));
        assert_eq!(result.advance, Some((3, 2)));
        let placement = store.placements(false, 0).remove(0);
        assert_eq!(placement.row, 4);
        assert_eq!(store.image(7, placement.revision).unwrap().data, "AAAA");
        store.scroll(false, 2);
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
        store.scroll(false, 2);
        assert_eq!(store.virtual_cells.get(&(false, 1, 2)), Some(&(7, 4)));
        store.clear_virtual_cell(false, 1, 2);
        assert!(store.virtual_cells.is_empty());
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
