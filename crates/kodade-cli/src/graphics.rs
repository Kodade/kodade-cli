//! Fetch image revisions separately from layouts and map them into pane bounds.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use kodade_cli_proto::{
    ClientMessage, ImageData, ImagePlacement, LayoutSnapshot, PaneId, QueryKind, ServerMessage,
};
use ratatui::layout::Rect;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const MAX_ASSETS: usize = 32;
const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    socket: PathBuf,
    pane: PaneId,
    image: u32,
    revision: u64,
}
struct Asset {
    host: u32,
    bytes: usize,
}
struct Loaded {
    key: Key,
    image: Option<ImageData>,
}

#[derive(Clone, Debug, PartialEq)]
struct Placement {
    key: Key,
    source: ImagePlacement,
    area: Rect,
}

pub struct Renderer {
    enabled: bool,
    assets: HashMap<Key, Asset>,
    pending: HashSet<Key>,
    failed: HashMap<Key, Instant>,
    previous: Vec<Placement>,
    next_host: u32,
    tx: mpsc::Sender<Loaded>,
    rx: mpsc::Receiver<Loaded>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new(supported())
    }
}

pub fn supported() -> bool {
    match std::env::var("KODADE_GRAPHICS").as_deref() {
        Ok("off") => false,
        Ok("kitty") => true,
        _ => {
            std::env::var("TERM").is_ok_and(|v| v == "xterm-kitty")
                || std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "ghostty")
        }
    }
}

impl Renderer {
    fn new(enabled: bool) -> Self {
        let (tx, rx) = mpsc::channel(2);
        Self {
            enabled,
            assets: HashMap::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            previous: Vec::new(),
            next_host: 0x4b00_0000,
            tx,
            rx,
        }
    }

    /// Call after the cell frame. Queries run independently; a slow image never
    /// blocks input. `None` hides placements while a modal covers the canvas.
    pub fn draw(
        &mut self,
        out: &mut impl Write,
        socket: &Path,
        layout: Option<&LayoutSnapshot>,
        rects: &[(PaneId, Rect)],
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let desired: Vec<_> = layout
            .into_iter()
            .flat_map(|layout| &layout.panes)
            .flat_map(|pane| {
                let inner = rects
                    .iter()
                    .find(|(id, _)| *id == pane.id)
                    .map(|(_, r)| r.inner(ratatui::layout::Margin::new(1, 1)));
                pane.screen.graphics.iter().filter_map(move |source| {
                    let (source, area) = clip(source, inner?)?;
                    Some(Placement {
                        key: Key {
                            socket: socket.into(),
                            pane: pane.id,
                            image: source.image,
                            revision: source.revision,
                        },
                        source,
                        area,
                    })
                })
            })
            .collect();
        let wanted: HashSet<_> = desired.iter().map(|p| p.key.clone()).collect();
        self.failed.retain(|key, _| wanted.contains(key));
        let mut commands = Vec::new();
        while let Ok(loaded) = self.rx.try_recv() {
            self.pending.remove(&loaded.key);
            if !wanted.contains(&loaded.key) {
                continue;
            }
            let Some(image) = loaded.image else {
                self.failed.insert(loaded.key, Instant::now());
                continue;
            };
            let bytes = image.data.len();
            // Only evict unused revisions; never thrash two large visible panes.
            let stale: Vec<_> = self
                .assets
                .keys()
                .filter(|key| !wanted.contains(*key))
                .cloned()
                .collect();
            for key in stale {
                if self.assets.len() < MAX_ASSETS
                    && self.assets.values().map(|a| a.bytes).sum::<usize>() + bytes <= MAX_BYTES
                {
                    break;
                }
                let asset = self.assets.remove(&key).expect("known asset");
                write!(commands, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", asset.host)?;
            }
            if self.assets.len() >= MAX_ASSETS
                || self.assets.values().map(|a| a.bytes).sum::<usize>() + bytes > MAX_BYTES
            {
                self.failed.insert(loaded.key, Instant::now());
                continue;
            }
            self.next_host = self.next_host.wrapping_add(1).max(1);
            upload(&mut commands, self.next_host, &image)?;
            self.assets.insert(
                loaded.key,
                Asset {
                    host: self.next_host,
                    bytes,
                },
            );
        }
        for key in &wanted {
            if self.pending.len() >= 2 {
                break;
            }
            if self.assets.contains_key(key)
                || self.pending.contains(key)
                || self
                    .failed
                    .get(key)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
            {
                continue;
            }
            self.pending.insert(key.clone());
            let key = key.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let request = ClientMessage::Query(QueryKind::Image {
                    pane: key.pane,
                    id: key.image,
                    revision: key.revision,
                });
                let image = match tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::commands::request(&key.socket, request),
                )
                .await
                {
                    Ok(Ok(ServerMessage::Image { pane, image }))
                        if pane == key.pane
                            && image.id == key.image
                            && image.revision == key.revision
                            && valid_asset(&image) =>
                    {
                        Some(image)
                    }
                    _ => None,
                };
                let _ = tx.send(Loaded { key, image }).await;
            });
        }
        let visible: Vec<_> = desired
            .into_iter()
            .filter(|p| self.assets.contains_key(&p.key))
            .collect();
        if visible != self.previous {
            let old: HashSet<_> = self
                .previous
                .iter()
                .filter_map(|p| self.assets.get(&p.key).map(|a| a.host))
                .collect();
            for host in old {
                write!(commands, "\x1b_Ga=d,d=i,i={host},q=2\x1b\\")?;
            }
            for p in &visible {
                let host = self.assets[&p.key].host;
                write!(commands, "\x1b[{};{}H\x1b_Ga=p,i={host},p={},q=2,C=1,c={},r={},x={},y={},w={},h={},z={}\x1b\\",
                    p.area.y + 1, p.area.x + 1, p.source.placement, p.area.width, p.area.height,
                    p.source.source_x, p.source.source_y, p.source.source_width, p.source.source_height, p.source.z)?;
            }
            self.previous = visible;
        }
        if !commands.is_empty() {
            out.write_all(b"\x1b7")?;
            out.write_all(&commands)?;
            out.write_all(b"\x1b8")?;
            out.flush()?;
        }
        Ok(())
    }

    pub fn clear(&mut self, out: &mut impl Write) -> Result<()> {
        for asset in self.assets.values() {
            write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", asset.host)?;
        }
        self.assets.clear();
        self.previous.clear();
        out.flush()?;
        Ok(())
    }
}

fn upload(out: &mut impl Write, host: u32, image: &ImageData) -> Result<()> {
    let chunks = image.data.as_bytes().chunks(4096);
    let count = chunks.len();
    for (index, data) in chunks.enumerate() {
        if index == 0 {
            write!(
                out,
                "\x1b_Ga=t,t=d,f={},s={},v={},i={host},q=2,m={};",
                image.format,
                image.width,
                image.height,
                u8::from(index + 1 < count)
            )?;
        } else {
            write!(out, "\x1b_Gm={},q=2;", u8::from(index + 1 < count))?;
        }
        out.write_all(data)?;
        out.write_all(b"\x1b\\")?;
    }
    Ok(())
}

fn valid_asset(image: &ImageData) -> bool {
    if image.data.len() > kodade_cli_daemon::MAX_IMAGE_BYTES * 4 / 3 + 4
        || image.width == 0
        || image.height == 0
        || image.width > 16384
        || image.height > 16384
    {
        return false;
    }
    let Ok(bytes) = STANDARD.decode(&image.data) else {
        return false;
    };
    if bytes.len() > kodade_cli_daemon::MAX_IMAGE_BYTES {
        return false;
    }
    match image.format {
        24 | 32 => {
            bytes.len() as u64
                == u64::from(image.width) * u64::from(image.height) * u64::from(image.format / 8)
        }
        100 => kodade_cli_daemon::validate_png(&bytes)
            .is_ok_and(|dimensions| dimensions == (image.width, image.height)),
        _ => false,
    }
}

/// Crop pixels and cells together, so off-screen rows never spill into chrome.
fn clip(source: &ImagePlacement, inner: Rect) -> Option<(ImagePlacement, Rect)> {
    let x = i32::from(source.col);
    let y = source.row;
    let left = x.max(0);
    let top = y.max(0);
    let right = (x + i32::from(source.cols)).min(i32::from(inner.width));
    let bottom = y
        .saturating_add(i32::from(source.rows))
        .min(i32::from(inner.height));
    if left >= right || top >= bottom || source.cols == 0 || source.rows == 0 {
        return None;
    }
    let mut p = source.clone();
    let pixels = |length: u32, cells: i32, total: u16| {
        (u64::from(length) * cells as u64 / u64::from(total)) as u32
    };
    p.source_x += pixels(source.source_width, left - x, source.cols);
    p.source_y += pixels(source.source_height, top - y, source.rows);
    p.source_width = pixels(source.source_width, right - x, source.cols)
        - pixels(source.source_width, left - x, source.cols);
    p.source_height = pixels(source.source_height, bottom - y, source.rows)
        - pixels(source.source_height, top - y, source.rows);
    if p.source_width == 0 || p.source_height == 0 {
        return None;
    }
    Some((
        p,
        Rect::new(
            inner.x + left as u16,
            inner.y + top as u16,
            (right - left) as u16,
            (bottom - top) as u16,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_scrolled_image_to_pane_without_changing_pixel_scale() {
        let source = ImagePlacement {
            image: 1,
            revision: 1,
            placement: 1,
            row: -2,
            col: 3,
            cols: 8,
            rows: 8,
            source_x: 0,
            source_y: 0,
            source_width: 80,
            source_height: 80,
            z: 0,
        };
        let (cropped, rect) = clip(&source, Rect::new(20, 4, 10, 4)).unwrap();
        assert_eq!(rect, Rect::new(23, 4, 7, 4));
        assert_eq!(
            (
                cropped.source_x,
                cropped.source_y,
                cropped.source_width,
                cropped.source_height
            ),
            (0, 20, 70, 40)
        );
        assert!(clip(&source, Rect::new(0, 0, 2, 4)).is_none());
    }
    #[test]
    fn host_upload_uses_owned_ids_and_bounded_quiet_chunks() {
        let mut output = Vec::new();
        upload(
            &mut output,
            500,
            &ImageData {
                id: 7,
                revision: 1,
                format: 24,
                width: 2000,
                height: 1,
                data: "A".repeat(8000),
            },
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\x1b_Ga=t,t=d,f=24,s=2000,v=1,i=500,q=2,m=1;"));
        assert!(output.contains("\x1b_Gm=0,q=2;"));
        assert_eq!(output.matches("\x1b\\").count(), 2);
    }
}
