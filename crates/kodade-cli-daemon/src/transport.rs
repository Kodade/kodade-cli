//! Local daemon transport: Unix sockets on Unix, user-scoped loopback on Windows.
use anyhow::{Context, Result};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(unix)]
pub use tokio::net::{UnixListener as Listener, UnixStream as Stream};
#[cfg(unix)]
pub type OwnedWriteHalf = tokio::net::unix::OwnedWriteHalf;
#[cfg(unix)]
pub async fn connect(path: &Path) -> Result<Stream> {
    Stream::connect(path).await.context("connect Unix socket")
}
#[cfg(unix)]
pub async fn bind(path: &Path) -> Result<Listener> {
    Listener::bind(path).context("bind Unix socket")
}
#[cfg(windows)]
pub use tokio::net::{TcpListener as Listener, TcpStream as Stream};
#[cfg(windows)]
pub type OwnedWriteHalf = tokio::net::tcp::OwnedWriteHalf;
#[cfg(windows)]
pub async fn connect(path: &Path) -> Result<Stream> {
    Stream::connect(path.to_string_lossy().as_ref())
        .await
        .context("connect loopback endpoint")
}
#[cfg(windows)]
pub async fn bind(path: &Path) -> Result<Listener> {
    Listener::bind(path.to_string_lossy().as_ref())
        .await
        .context("bind loopback endpoint")
}
#[cfg(windows)]
pub fn endpoint(session: &str) -> PathBuf {
    let mut hash = 2166136261u32;
    for byte in session.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(16777619);
    }
    PathBuf::from(format!("127.0.0.1:{}", 40000 + hash % 20000))
}
