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
pub type OwnedReadHalf = tokio::net::tcp::OwnedReadHalf;
#[cfg(windows)]
pub type OwnedWriteHalf = tokio::net::tcp::OwnedWriteHalf;
#[cfg(windows)]
pub async fn connect(path: &Path) -> Result<Stream> {
    use tokio::io::AsyncWriteExt;
    let record = std::fs::read_to_string(path).context("read private daemon discovery record")?;
    let (endpoint, secret) = parse_record(&record)?;
    let mut stream = Stream::connect(endpoint)
        .await
        .context("connect loopback endpoint")?;
    stream
        .write_all(&secret)
        .await
        .context("authenticate loopback connection")?;
    Ok(stream)
}

#[cfg(windows)]
fn parse_record(record: &str) -> Result<(&str, [u8; 32])> {
    let (endpoint, encoded) = record
        .trim_end()
        .split_once('\n')
        .context("invalid daemon discovery record")?;
    if endpoint.parse::<std::net::SocketAddr>().is_err() || encoded.len() != 64 {
        anyhow::bail!("invalid daemon discovery record");
    }
    let mut secret = [0; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16)
            .context("invalid daemon discovery record")?;
    }
    Ok((endpoint, secret))
}
