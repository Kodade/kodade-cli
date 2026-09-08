//! Local daemon transport: Unix sockets on Unix, authenticated loopback on Windows.
use anyhow::{Context, Result};
use std::path::Path;
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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
#[cfg(windows)]
pub type Stream = TcpStream;
#[cfg(windows)]
pub type OwnedWriteHalf = tokio::net::tcp::OwnedWriteHalf;
#[cfg(windows)]
pub struct Listener {
    listener: TcpListener,
    secret: [u8; 32],
}

#[cfg(windows)]
#[allow(dead_code)] // The CLI crate connects in production; daemon tests use this seam.
pub async fn connect(path: &Path) -> Result<Stream> {
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
pub async fn bind(path: &Path) -> Result<Listener> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind loopback endpoint")?;
    let mut secret = [0; 32];
    getrandom::getrandom(&mut secret)
        .map_err(|error| anyhow::anyhow!("generate loopback authentication secret: {error}"))?;
    write_record(
        path,
        &listener
            .local_addr()
            .context("read loopback endpoint")?
            .to_string(),
        &secret,
    )?;
    Ok(Listener { listener, secret })
}
#[cfg(windows)]
impl Listener {
    /// Reject unauthenticated peers before handing a byte of JSON to the daemon.
    pub async fn accept(&self) -> Result<(Stream, std::net::SocketAddr)> {
        let (mut stream, address) = self
            .listener
            .accept()
            .await
            .context("accept loopback connection")?;
        let mut presented = [0; 32];
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            stream.read_exact(&mut presented),
        )
        .await
        .context("loopback authentication timed out")?
        .context("read loopback authentication")?;
        if !constant_time_eq(&presented, &self.secret) {
            anyhow::bail!("unauthenticated loopback connection");
        }
        Ok((stream, address))
    }
}

#[cfg(windows)]
fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0u8, |different, (a, b)| different | (a ^ b))
        == 0
}

#[cfg(windows)]
#[allow(dead_code)] // Used by the test-only daemon transport client above.
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

#[cfg(windows)]
fn write_record(path: &Path, endpoint: &str, secret: &[u8; 32]) -> Result<()> {
    let parent = path.parent().context("discovery record needs parent")?;
    std::fs::create_dir_all(parent).context("create discovery directory")?;
    let text = format!(
        "{endpoint}\n{}\n",
        secret
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    std::fs::write(path, text).context("write daemon discovery record")?;
    restrict_to_owner(path)?;
    Ok(())
}

/// The secret is useful only if another local account cannot read it. `OW` is
/// the SID of the file owner; the protected DACL prevents inherited broad ACLs.
#[cfg(windows)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR,
        },
    };
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)\0".encode_utf16().collect();
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            anyhow::bail!(
                "create private discovery ACL: {}",
                std::io::Error::last_os_error()
            );
        }
        let result = SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        );
        LocalFree(descriptor.cast());
        if result == 0 {
            anyhow::bail!(
                "restrict daemon discovery record: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}
