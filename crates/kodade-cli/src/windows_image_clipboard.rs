//! Bounded native Windows image clipboard reads.

use anyhow::{bail, Result};
use std::{
    io::{self, Write},
    ptr::null_mut,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::HGLOBAL,
    System::{
        DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW},
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Ole::{CF_DIB, CF_DIBV5},
    },
};

const MAX_CLIPBOARD_BYTES: usize = 64 * 1024 * 1024;
const MAX_PNG_CLIPBOARD_BYTES: usize = kodade_cli_daemon::MAX_IMAGE_BYTES + 64 * 1024;
const MAX_DIMENSION: u32 = 16_384;
const MAX_PIXELS: usize = 16 * 1024 * 1024;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

pub(super) fn read() -> Result<Vec<u8>> {
    if let Some(png) = registered_png()? {
        return Ok(png);
    }
    for format in [u32::from(CF_DIBV5), u32::from(CF_DIB)] {
        if let Some(bytes) = copy_clipboard_format(format, MAX_CLIPBOARD_BYTES)? {
            if let Some(png) = Dib::parse(&bytes).and_then(|dib| dib.encode_png()) {
                return Ok(png);
            }
        }
    }
    bail!("clipboard has no supported PNG, DIB, or DIBV5 image")
}

fn registered_png() -> Result<Option<Vec<u8>>> {
    let name: Vec<u16> = "PNG".encode_utf16().chain(Some(0)).collect();
    let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    if format == 0 {
        return Ok(None);
    }
    let Some(bytes) = copy_clipboard_format(format, MAX_PNG_CLIPBOARD_BYTES)? else {
        return Ok(None);
    };
    Ok(validated_png(&bytes))
}

/// Copy only the selected format while the OS clipboard is open. Validation and
/// PNG encoding happen after `Clipboard` drops, so image work cannot block other apps.
fn copy_clipboard_format(format: u32, maximum: usize) -> Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { OpenClipboard(null_mut()) } != 0 {
            break;
        }
        if Instant::now() >= deadline {
            bail!("Windows clipboard is busy");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _clipboard = Clipboard;
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return Ok(None);
    }
    let size = unsafe { GlobalSize(handle as HGLOBAL) };
    if size == 0 || size > maximum {
        return Ok(None);
    }
    let data = unsafe { GlobalLock(handle as HGLOBAL) };
    if data.is_null() {
        return Ok(None);
    }
    let _locked = Locked(handle as HGLOBAL);
    let mut bytes = vec![0; size];
    unsafe {
        std::ptr::copy_nonoverlapping(data.cast::<u8>(), bytes.as_mut_ptr(), size);
    }
    Ok(Some(bytes))
}

struct Clipboard;
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}
struct Locked(HGLOBAL);
impl Drop for Locked {
    fn drop(&mut self) {
        unsafe {
            GlobalUnlock(self.0);
        }
    }
}

fn validated_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let end = png_end(bytes)?;
    let png = bytes.get(..end)?;
    kodade_cli_daemon::validate_png(png).ok()?;
    Some(png.to_vec())
}

fn png_end(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut offset = 8usize;
    while offset < bytes.len() {
        let length = usize::try_from(u32::from_be_bytes(
            bytes.get(offset..offset + 4)?.try_into().ok()?,
        ))
        .ok()?;
        let kind = offset.checked_add(4)?;
        let end = kind.checked_add(4)?.checked_add(length)?.checked_add(4)?;
        if end > bytes.len() {
            return None;
        }
        if bytes.get(kind..kind + 4)? == b"IEND" {
            return (length == 0).then_some(end);
        }
        offset = end;
    }
    None
}

struct Dib<'a> {
    width: u32,
    height: u32,
    top_down: bool,
    stride: usize,
    pixels: &'a [u8],
    format: Format,
}
#[derive(Clone, Copy)]
enum Format {
    Bgr24,
    Bgrx32,
    Masks {
        red: Mask,
        green: Mask,
        blue: Mask,
        alpha: Option<Mask>,
    },
}
#[derive(Clone, Copy)]
struct Mask {
    value: u32,
    shift: u32,
    maximum: u32,
}

impl Mask {
    fn parse(value: u32) -> Option<Self> {
        if value == 0 {
            return None;
        }
        let shift = value.trailing_zeros();
        let maximum = value >> shift;
        (maximum & maximum.wrapping_add(1) == 0).then_some(Self {
            value,
            shift,
            maximum,
        })
    }
    fn channel(self, pixel: u32) -> u8 {
        let value = (pixel & self.value) >> self.shift;
        ((u64::from(value) * 255 + u64::from(self.maximum) / 2) / u64::from(self.maximum)) as u8
    }
}

impl<'a> Dib<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        let header = usize::try_from(u32le(bytes, 0)?).ok()?;
        if !matches!(header, 40 | 52 | 56 | 108 | 124) || bytes.len() < header {
            return None;
        }
        let width_i = i32le(bytes, 4)?;
        let height_i = i32le(bytes, 8)?;
        if width_i <= 0 || height_i == 0 || height_i == i32::MIN || u16le(bytes, 12)? != 1 {
            return None;
        }
        let width = u32::try_from(width_i).ok()?;
        let height = height_i.unsigned_abs();
        dimensions_ok(width, height)?;
        let bits = u16le(bytes, 14)?;
        let compression = u32le(bytes, 16)?;
        let (format, masks) = pixel_format(bytes, header, bits, compression)?;
        let palette = usize::try_from(u32le(bytes, 32)?).ok()?.checked_mul(4)?;
        let offset = header.checked_add(masks)?.checked_add(palette)?;
        let stride = usize::try_from(width)
            .ok()?
            .checked_mul(usize::from(bits))?
            .checked_add(31)?
            .checked_div(32)?
            .checked_mul(4)?;
        let length = stride.checked_mul(usize::try_from(height).ok()?)?;
        Some(Self {
            width,
            height,
            top_down: height_i < 0,
            stride,
            pixels: bytes.get(offset..offset.checked_add(length)?)?,
            format,
        })
    }
    fn encode_png(&self) -> Option<Vec<u8>> {
        let mut output = LimitedWriter::new(kodade_cli_daemon::MAX_IMAGE_BYTES);
        {
            let mut encoder = png::Encoder::new(&mut output, self.width, self.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            let mut stream = writer.stream_writer().ok()?;
            let mut row = vec![0; usize::try_from(self.width).ok()?.checked_mul(4)?];
            for y in 0..usize::try_from(self.height).ok()? {
                let source_y = if self.top_down {
                    y
                } else {
                    usize::try_from(self.height).ok()?.checked_sub(y + 1)?
                };
                self.decode_row(
                    self.pixels.get(
                        source_y.checked_mul(self.stride)?
                            ..source_y.checked_add(1)?.checked_mul(self.stride)?,
                    )?,
                    &mut row,
                )?;
                stream.write_all(&row).ok()?;
            }
            stream.finish().ok()?;
            writer.finish().ok()?;
        }
        let output = output.bytes;
        kodade_cli_daemon::validate_png(&output).ok()?;
        Some(output)
    }
    fn decode_row(&self, source: &[u8], output: &mut [u8]) -> Option<()> {
        let width = usize::try_from(self.width).ok()?;
        match self.format {
            Format::Bgr24 => {
                let (source, _) = source.as_chunks::<3>();
                let (output, _) = output.as_chunks_mut::<4>();
                for (source, out) in source.iter().take(width).zip(output) {
                    out.copy_from_slice(&[source[2], source[1], source[0], 255]);
                }
            }
            Format::Bgrx32 => {
                let (source, _) = source.as_chunks::<4>();
                let (output, _) = output.as_chunks_mut::<4>();
                for (source, out) in source.iter().take(width).zip(output) {
                    out.copy_from_slice(&[source[2], source[1], source[0], 255]);
                }
            }
            Format::Masks {
                red,
                green,
                blue,
                alpha,
            } => {
                let (source, _) = source.as_chunks::<4>();
                let (output, _) = output.as_chunks_mut::<4>();
                for (source, out) in source.iter().take(width).zip(output) {
                    let pixel = u32::from_le_bytes(*source);
                    out.copy_from_slice(&[
                        red.channel(pixel),
                        green.channel(pixel),
                        blue.channel(pixel),
                        alpha.map_or(255, |mask| mask.channel(pixel)),
                    ]);
                }
            }
        }
        Some(())
    }
}

fn pixel_format(
    bytes: &[u8],
    header: usize,
    bits: u16,
    compression: u32,
) -> Option<(Format, usize)> {
    match (bits, compression) {
        (24, BI_RGB) => return Some((Format::Bgr24, 0)),
        (32, BI_RGB) => return Some((Format::Bgrx32, 0)),
        (32, BI_BITFIELDS | BI_ALPHABITFIELDS) => {}
        _ => return None,
    }
    let (offset, external) = if header == 40 {
        let count = if compression == BI_ALPHABITFIELDS {
            4
        } else {
            3
        };
        (40, count * 4)
    } else {
        if header < 52 || (compression == BI_ALPHABITFIELDS && header < 56) {
            return None;
        }
        (40, 0)
    };
    let red = Mask::parse(u32le(bytes, offset)?)?;
    let green = Mask::parse(u32le(bytes, offset + 4)?)?;
    let blue = Mask::parse(u32le(bytes, offset + 8)?)?;
    if red.value & green.value != 0 || red.value & blue.value != 0 || green.value & blue.value != 0
    {
        return None;
    }
    let alpha = if compression == BI_ALPHABITFIELDS || header >= 56 {
        match u32le(bytes, offset + 12).filter(|mask| *mask != 0) {
            Some(mask) => Some(Mask::parse(mask)?),
            None => None,
        }
    } else {
        None
    };
    if alpha.is_some_and(|alpha| alpha.value & (red.value | green.value | blue.value) != 0) {
        return None;
    }
    Some((
        Format::Masks {
            red,
            green,
            blue,
            alpha,
        },
        external,
    ))
}

fn dimensions_ok(width: u32, height: u32) -> Option<()> {
    (width != 0
        && height != 0
        && width <= MAX_DIMENSION
        && height <= MAX_DIMENSION
        && usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            <= MAX_PIXELS)
        .then_some(())
}
fn u16le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}
fn u32le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
fn i32le(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "encoded clipboard image exceeds 8 MiB",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn header(width: i32, height: i32, bits: u16, compression: u32) -> Vec<u8> {
        let mut bytes = vec![0; 40];
        bytes[0..4].copy_from_slice(&40u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&width.to_le_bytes());
        bytes[8..12].copy_from_slice(&height.to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&bits.to_le_bytes());
        bytes[16..20].copy_from_slice(&compression.to_le_bytes());
        bytes
    }
    fn rgba(png: &[u8]) -> Vec<u8> {
        let mut reader = png::Decoder::new(Cursor::new(png)).read_info().unwrap();
        let mut output = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut output).unwrap();
        output.truncate(info.buffer_size());
        output
    }
    #[test]
    fn converts_padded_bottom_up_24_bit_dib() {
        let mut dib = header(2, 2, 24, BI_RGB);
        dib.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0, 0, 0, 255, 0, 255, 0, 0, 0]);
        assert_eq!(
            rgba(&Dib::parse(&dib).unwrap().encode_png().unwrap()),
            vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]
        );
    }
    #[test]
    fn rejects_malformed_stride_and_oversized_dimensions() {
        assert!(Dib::parse(&header(2, 1, 24, BI_RGB)).is_none());
        assert!(Dib::parse(&header(16_385, 1, 24, BI_RGB)).is_none());
    }
    #[test]
    fn decodes_bitfield_channels() {
        let mut dib = header(1, -1, 32, BI_BITFIELDS);
        dib.extend_from_slice(&0x00ff0000u32.to_le_bytes());
        dib.extend_from_slice(&0x0000ff00u32.to_le_bytes());
        dib.extend_from_slice(&0x000000ffu32.to_le_bytes());
        dib.extend_from_slice(&[3, 2, 1, 0]);
        assert_eq!(
            rgba(&Dib::parse(&dib).unwrap().encode_png().unwrap()),
            vec![1, 2, 3, 255]
        );
    }

    #[test]
    #[ignore = "uses the Windows system clipboard; CI runs this explicitly"]
    fn windows_native_clipboard_reads_dib_pixels() {
        use windows_sys::Win32::{
            Foundation::GlobalFree,
            System::{
                DataExchange::{EmptyClipboard, SetClipboardData},
                Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
            },
        };

        let mut dib = header(1, -1, 32, BI_RGB);
        dib.extend_from_slice(&[3, 2, 1, 0]);
        unsafe {
            assert_ne!(OpenClipboard(null_mut()), 0, "open clipboard");
            let _clipboard = Clipboard;
            assert_ne!(EmptyClipboard(), 0, "empty clipboard");
            let memory = GlobalAlloc(GMEM_MOVEABLE, dib.len());
            assert!(!memory.is_null(), "allocate DIB");
            let destination = GlobalLock(memory).cast::<u8>();
            assert!(!destination.is_null(), "lock DIB");
            std::ptr::copy_nonoverlapping(dib.as_ptr(), destination, dib.len());
            GlobalUnlock(memory);
            if SetClipboardData(u32::from(CF_DIB), memory).is_null() {
                GlobalFree(memory);
                panic!(
                    "set DIB clipboard data: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        assert_eq!(
            rgba(&read().expect("read native DIB clipboard")),
            vec![1, 2, 3, 255]
        );
    }
}
