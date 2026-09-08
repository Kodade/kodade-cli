//! A bounded Kitty graphics store. Escape commands never reach the host terminal.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use kodade_cli_proto::{ImageData, ImagePlacement};
use unicode_width::UnicodeWidthChar;

const MAX_IMAGE: usize = 8 * 1024 * 1024;
const MAX_STORE: usize = 32 * 1024 * 1024;
const MAX_IMAGES: usize = 16;
const MAX_PLACEMENTS: usize = 64;
const MAX_FRAME: usize = 8192;

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
    placements: Vec<(bool, ImagePlacement)>,
    transfer: Option<Transfer>,
    revision: u64,
    next_image: u32,
    next_placement: u32,
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
    pub fn feed(&mut self, byte: u8, screen: &vt100::Screen, store: &mut Store) {
        self.events.0 = None;
        self.parser.advance(&mut self.events, &[byte]);
        let Some(event) = self.events.0.take() else {
            return;
        };
        let alternate = screen.alternate_screen();
        let (height, width) = screen.size();
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
                return;
            }
            Control::Reset => {
                self.margins = [None, None];
                store.clear(false);
                store.clear(true);
                return;
            }
            Control::Clear => {
                store.clear(alternate);
                return;
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
    }
}

impl Store {
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

    pub fn placements(&self, alternate: bool, scroll: usize) -> Vec<ImagePlacement> {
        self.placements
            .iter()
            .filter(|(alt, _)| *alt == alternate)
            .map(|(_, p)| {
                let mut p = p.clone();
                p.row = p.row.saturating_add(scroll.min(i32::MAX as usize) as i32);
                p
            })
            .collect()
    }

    pub fn clear(&mut self, alternate: bool) {
        self.placements.retain(|(alt, _)| *alt != alternate);
    }

    pub fn scroll(&mut self, alternate: bool, rows: i32) {
        for (alt, placement) in &mut self.placements {
            if *alt == alternate {
                placement.row = placement.row.saturating_sub(rows);
            }
        }
        self.placements.retain(|(_, p)| p.row > -10_000);
    }

    fn scroll_region(&mut self, alternate: bool, top: u16, bottom: u16, shift: i32) {
        let top = i32::from(top);
        let end = i32::from(bottom) + 1;
        for (alt, p) in &mut self.placements {
            if *alt != alternate || p.row < top || p.row + i32::from(p.rows) > end {
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
        self.placements
            .retain(|(_, p)| p.rows > 0 && p.source_height > 0);
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
            match mode {
                "a" | "A" => self.clear(alternate),
                "i" | "I" => self.placements.retain(|(_, p)| p.image != id),
                _ => bail!("unsupported delete selector"),
            }
            if mode == "A" {
                self.images.clear();
                self.placements.clear();
            }
            if mode == "I" {
                self.images.remove(&id);
            }
            return Ok(None);
        }
        if params.get("U").is_some_and(|v| v != "0") || params.contains_key("P") {
            bail!("virtual and relative placements are unsupported");
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
            // `a=T` transfers and places in one command. Validate every
            // placement failure before replacing image data so a rejected
            // crop/quota request is atomic from the pane's perspective.
            if action == "T" {
                PlacementSpec::parse(&params, width, height)?;
                number(&params, "p", 0)?;
                if self
                    .placements
                    .iter()
                    .filter(|(_, p)| p.image != id)
                    .count()
                    >= MAX_PLACEMENTS
                {
                    bail!("pane placement quota exceeded");
                }
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
            self.placements.retain(|(_, p)| p.image != id);
        }
        if matches!(action, "p" | "T") {
            let image = self.images.get(&id).context("image not found")?;
            let spec = PlacementSpec::parse(&params, image.width, image.height)?;
            let mut placement = number(&params, "p", 0)?;
            if placement == 0 {
                self.next_placement = self.next_placement.wrapping_add(1).max(1);
                placement = self.next_placement;
            }
            let replaces = self
                .placements
                .iter()
                .any(|(_, p)| p.image == id && p.placement == placement);
            if !replaces && self.placements.len() >= MAX_PLACEMENTS {
                bail!("pane placement quota exceeded");
            }
            self.placements
                .retain(|(_, p)| p.image != id || p.placement != placement);
            self.placements.push((
                alternate,
                ImagePlacement {
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
                    z: spec.z,
                },
            ));
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
    z: i32,
    advance: bool,
}
impl PlacementSpec {
    fn parse(params: &BTreeMap<String, String>, width: u32, height: u32) -> Result<Self> {
        if number(params, "X", 0)? != 0 || number(params, "Y", 0)? != 0 {
            bail!("pixel placement offsets are unsupported; use cell-aligned placements");
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
        Ok(Self {
            cols: cols.clamp(1, u64::from(u16::MAX)) as u16,
            rows: rows.clamp(1, u64::from(u16::MAX)) as u16,
            x,
            y,
            width,
            height,
            z: params
                .get("z")
                .map(|value| value.parse())
                .transpose()
                .context("invalid z index")?
                .unwrap_or(0),
            advance: number(params, "C", 0)? == 0,
        })
    }
}

fn number(params: &BTreeMap<String, String>, key: &str, default: u32) -> Result<u32> {
    params
        .get(key)
        .map(|value| value.parse().with_context(|| format!("invalid {key}")))
        .unwrap_or(Ok(default))
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
            (store.placements[0].1.cols, store.placements[0].1.rows),
            (8, 2)
        );
        store.command(b"a=p,i=7,p=1,c=4,r=0", (0, 0), false);
        assert_eq!(
            (store.placements[0].1.cols, store.placements[0].1.rows),
            (4, 1)
        );
        let before = store.placements.clone();
        for invalid in ["z=bad", "C=bad", "X=1"] {
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
}
