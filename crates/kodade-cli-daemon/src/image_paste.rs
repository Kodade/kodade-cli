//! Private, bounded PNG attachments retained until the session stops.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    fs,
    io::{Cursor, Write},
    path::PathBuf,
    sync::Mutex,
};

pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DECODED: usize = 64 * 1024 * 1024;

pub fn validate_png(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("PNG exceeds 8 MiB");
    }
    let mut decoder =
        png::Decoder::new_with_limits(Cursor::new(bytes), png::Limits { bytes: MAX_DECODED });
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let mut reader = decoder.read_info().context("invalid PNG")?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > 16 * 1024 * 1024
    {
        bail!("PNG dimensions exceed 16 megapixels");
    }
    let size = reader
        .output_buffer_size()
        .filter(|size| *size <= MAX_DECODED)
        .context("PNG decoded pixels exceed 64 MiB")?;
    reader
        .next_frame(&mut vec![0; size])
        .context("invalid PNG pixels")?;
    reader.finish().context("incomplete PNG")?;
    Ok((width, height))
}

#[derive(Default)]
pub struct Inbox(Mutex<Option<Directory>>);
struct Directory {
    path: PathBuf,
    count: usize,
    next: usize,
    bytes: usize,
}

impl Inbox {
    pub fn save(&self, encoded: &str) -> Result<PathBuf> {
        if encoded.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
            bail!("PNG exceeds 8 MiB");
        }
        let bytes = STANDARD.decode(encoded).context("invalid PNG base64")?;
        validate_png(&bytes)?;
        let mut state = self.0.lock().expect("image inbox lock");
        if state.is_none() {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("kodade-images-{}-{nonce}", std::process::id()));
            #[cfg(unix)]
            let mut builder = fs::DirBuilder::new();
            #[cfg(windows)]
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(&path)
                .context("create private image directory")?;
            *state = Some(Directory {
                path,
                count: 0,
                next: 0,
                bytes: 0,
            });
        }
        let directory = state.as_mut().expect("created inbox");
        if directory.count >= 64 || directory.bytes + bytes.len() > MAX_DECODED {
            bail!("session attachment quota reached (64 images / 64 MiB)");
        }
        directory.next += 1;
        let path = directory.path.join(format!("image-{}.png", directory.next));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).context("save PNG")?;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        directory.count += 1;
        directory.bytes += bytes.len();
        Ok(path)
    }

    /// Remove an attachment whose path was handed to a pane but could not be
    /// written. Filesystem work happens outside the state lock.
    pub fn discard(&self, path: &std::path::Path) {
        let bytes = match fs::metadata(path) {
            Ok(metadata) => metadata.len() as usize,
            Err(_) => return,
        };
        let owned = self
            .0
            .lock()
            .expect("image inbox lock")
            .as_ref()
            .is_some_and(|directory| path.parent() == Some(directory.path.as_path()));
        if !owned || fs::remove_file(path).is_err() {
            return;
        }
        if let Some(directory) = self.0.lock().expect("image inbox lock").as_mut() {
            directory.count = directory.count.saturating_sub(1);
            directory.bytes = directory.bytes.saturating_sub(bytes);
        }
    }
    pub fn clear(&self) {
        let directory = self.0.lock().expect("image inbox lock").take();
        if let Some(directory) = directory {
            let _ = fs::remove_dir_all(directory.path);
        }
    }
}
impl Drop for Inbox {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_inbox_rejects_corruption_and_retains_private_files_until_cleanup() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[255, 0, 0])
                .unwrap();
        }
        let inbox = Inbox::default();
        let path = inbox.save(&STANDARD.encode(&bytes)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        bytes.truncate(25);
        assert!(inbox.save(&STANDARD.encode(bytes)).is_err());
        inbox.discard(&path);
        assert!(!path.exists());
        let state = inbox.0.lock().unwrap();
        let directory = state.as_ref().unwrap();
        assert_eq!((directory.count, directory.bytes), (0, 0));
        drop(state);
        inbox.clear();
    }
}
