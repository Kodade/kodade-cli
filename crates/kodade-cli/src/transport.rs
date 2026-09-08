//! Client connection transport matching the daemon platform endpoint.
use anyhow::{Context, Result};
#[cfg(windows)]
use std::io::Read;
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
    use tokio::io::AsyncWriteExt;
    let record = read_record(path)?;
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
fn read_record(path: &Path) -> Result<String> {
    let mut record = String::new();
    std::fs::File::open(path)
        .context("open private daemon discovery record")?
        .take(512)
        .read_to_string(&mut record)
        .context("read private daemon discovery record")?;
    Ok(record)
}

#[cfg(windows)]
fn parse_record(record: &str) -> Result<(&str, [u8; 32])> {
    let record = record
        .strip_suffix('\n')
        .context("invalid daemon discovery record")?;
    let (endpoint, encoded) = record
        .split_once('\n')
        .context("invalid daemon discovery record")?;
    let address = endpoint
        .parse::<std::net::SocketAddr>()
        .context("invalid daemon discovery record")?;
    if !address.ip().is_loopback()
        || encoded.len() != 64
        || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("invalid daemon discovery record");
    }
    let mut secret = [0; 32];
    let (pairs, remainder) = encoded.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (byte, hex) in secret.iter_mut().zip(pairs) {
        *byte = u8::from_str_radix(std::str::from_utf8(hex).expect("ASCII hex"), 16)
            .context("invalid daemon discovery record")?;
    }
    Ok((endpoint, secret))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn discovery_record_accepts_only_ascii_loopback_secrets() {
        let record = format!("127.0.0.1:4000\n{}\n", "ab".repeat(32));
        assert_eq!(parse_record(&record).unwrap().1, [0xab; 32]);
        assert!(parse_record(&format!("192.0.2.1:4000\n{}\n", "ab".repeat(32))).is_err());
        assert!(parse_record(&format!("127.0.0.1:4000\n{}\n", "é".repeat(32))).is_err());
    }
}
