//! Client connection transport matching the daemon platform endpoint.
use anyhow::{Context, Result};
use std::path::Path;
#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;
#[cfg(unix)]
pub type OwnedReadHalf = tokio::net::unix::OwnedReadHalf;
#[cfg(unix)]
pub type OwnedWriteHalf = tokio::net::unix::OwnedWriteHalf;
#[cfg(unix)]
pub async fn connect(path: &Path) -> Result<Stream> {
    Stream::connect(path).await.context("connect Unix socket")
}
#[cfg(windows)]
pub type Stream = tokio::net::TcpStream;
#[cfg(windows)]
pub type OwnedReadHalf = tokio::net::tcp::OwnedReadHalf;
#[cfg(windows)]
pub type OwnedWriteHalf = tokio::net::tcp::OwnedWriteHalf;
#[cfg(windows)]
pub async fn connect(path: &Path) -> Result<Stream> {
    Stream::connect(path.to_string_lossy().as_ref())
        .await
        .context("connect loopback endpoint")
}
