//! Image paste targets the selected daemon; the clipboard stays on this client.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use kodade_cli_proto::{ClientMessage, PaneId, ServerMessage};
#[cfg(not(windows))]
use std::process::Stdio;
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
#[cfg(not(windows))]
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

#[cfg(windows)]
#[path = "windows_image_clipboard.rs"]
mod windows_image_clipboard;

pub async fn paste(socket: &Path, pane: PaneId, path: Option<&Path>) -> Result<PathBuf> {
    let bytes = if let Some(path) = path {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || read_file(&path))
            .await
            .context("read image task")??
    } else {
        clipboard().await?
    };
    kodade_cli_daemon::validate_png(&bytes)?;
    let request = ClientMessage::PasteImage {
        pane,
        data: STANDARD.encode(bytes),
    };
    match tokio::time::timeout(
        Duration::from_secs(15),
        crate::commands::request(socket, request),
    )
    .await
    .context("image upload timed out")??
    {
        ServerMessage::ImagePasted { pane: target, path } if target == pane => Ok(path),
        ServerMessage::Error { message } => bail!("{message}"),
        _ => bail!("daemon did not acknowledge image paste"),
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    // A clipboard action must not hang on a FIFO/device path. O_NONBLOCK also
    // closes the metadata/open race on Unix before checking the opened file.
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("image path must be a regular PNG file");
    }
    let mut bytes = Vec::new();
    file.take(kodade_cli_daemon::MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn image_path_rejects_a_fifo_without_waiting_for_a_writer() {
        let path = std::env::temp_dir().join(format!("kodade-image-fifo-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(super::read_file(&path)
            .unwrap_err()
            .to_string()
            .contains("regular"));
        std::fs::remove_file(path).unwrap();
    }
}

async fn clipboard() -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        tokio::task::spawn_blocking(windows_image_clipboard::read)
            .await
            .context("Windows image clipboard worker")?
    }

    #[cfg(not(windows))]
    {
        let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
            ("pngpaste", &["-"])
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            ("wl-paste", &["--type", "image/png", "--no-newline"])
        } else {
            (
                "xclip",
                &["-selection", "clipboard", "-t", "image/png", "-o"],
            )
        };
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "image clipboard needs {program}; install it or use pane paste-image PANE PATH"
                )
            })?;
        let mut stdout = child
            .stdout
            .take()
            .expect("piped clipboard")
            .take(kodade_cli_daemon::MAX_IMAGE_BYTES as u64 + 1);
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await?;
            if bytes.len() > kodade_cli_daemon::MAX_IMAGE_BYTES {
                child.kill().await?;
                bail!("clipboard image exceeds 8 MiB");
            }
            if !child.wait().await?.success() || bytes.is_empty() {
                bail!("clipboard has no PNG image");
            }
            Ok(bytes)
        })
        .await
        .context("clipboard read timed out after 5s")?
    }
}

pub struct Clipboard {
    tx: mpsc::Sender<String>,
    rx: mpsc::Receiver<String>,
    busy: bool,
}
impl Default for Clipboard {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            tx,
            rx,
            busy: false,
        }
    }
}
impl Clipboard {
    pub fn start(&mut self, socket: PathBuf, pane: PaneId) -> bool {
        if self.busy {
            return false;
        }
        self.busy = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let note = match paste(&socket, pane, None).await {
                Ok(_) => " image path pasted · press Enter to send".into(),
                Err(error) => format!(" image paste: {error:#}"),
            };
            let _ = tx.send(note).await;
        });
        true
    }
    pub fn poll(&mut self) -> Option<String> {
        let note = self.rx.try_recv().ok()?;
        self.busy = false;
        Some(note)
    }
}
