//! Unix transport for a live daemon handoff.
//!
//! The source retains each PTY master until a later commit handshake. SCM_RIGHTS
//! duplicates descriptors, so a rejected import leaves running panes untouched.

use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
    time::{Duration, Instant},
};

use kodade_cli_proto::{SessionFile, PROTOCOL_VERSION};
use portable_pty::{MasterPty, PtySize};
use serde::{Deserialize, Serialize};

/// Master adapter for a descriptor received over SCM_RIGHTS.
pub(crate) struct ImportedMaster {
    file: fs::File,
    writer_taken: std::sync::atomic::AtomicBool,
}

impl ImportedMaster {
    pub(crate) unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self {
            file: fs::File::from(OwnedFd::from_raw_fd(fd)),
            writer_taken: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl MasterPty for ImportedMaster {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let size = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCSWINSZ, &size) } != 0 {
            anyhow::bail!(io::Error::last_os_error());
        }
        Ok(())
    }
    fn get_size(&self) -> anyhow::Result<PtySize> {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
            anyhow::bail!(io::Error::last_os_error());
        }
        Ok(PtySize {
            rows: size.ws_row,
            cols: size.ws_col,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
    }
    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        Ok(Box::new(self.file.try_clone()?))
    }
    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        if self
            .writer_taken
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            anyhow::bail!("cannot take PTY writer more than once");
        }
        Ok(Box::new(self.file.try_clone()?))
    }
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        let pid = unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) };
        (pid > 0).then_some(pid)
    }
    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.file.as_raw_fd())
    }
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

pub(crate) const HANDOFF_VERSION: u32 = 1;
pub(crate) const MAX_HANDOFF_FDS: usize = 64;
const MAX_TOKEN_BYTES: usize = 256;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime data reconstructed beside a transferred PTY. `history_ansi` is
/// bounded by the exporter: vt100's private state is never serialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PaneRuntime {
    pub(crate) pane_id: u64,
    pub(crate) child_pid: i32,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    #[serde(default)]
    pub(crate) start_identity: Option<String>,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) spawn_command: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) cwd: Option<std::path::PathBuf>,
    #[serde(default)]
    pub(crate) agent_identity: Option<String>,
    #[serde(default)]
    pub(crate) hook_state: Option<kodade_cli_proto::AgentStateKind>,
    #[serde(default)]
    pub(crate) hook_source: Option<String>,
    #[serde(default)]
    pub(crate) state: Option<kodade_cli_proto::AgentStateKind>,
    /// Bounded formatted active screen; this is replayed into a fresh parser.
    #[serde(default)]
    pub(crate) screen_ansi: Vec<u8>,
    #[serde(default)]
    pub(crate) history_ansi: String,
}

/// Independently versioned handoff manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffManifest {
    pub(crate) version: u32,
    pub(crate) protocol_version: u32,
    pub(crate) session: SessionFile,
    pub(crate) panes: Vec<PaneRuntime>,
}

impl HandoffManifest {
    pub(crate) fn new(session: SessionFile, panes: Vec<PaneRuntime>) -> io::Result<Self> {
        if panes.len() > MAX_HANDOFF_FDS {
            return Err(input("handoff supports at most 64 PTYs"));
        }
        Ok(Self {
            version: HANDOFF_VERSION,
            protocol_version: PROTOCOL_VERSION,
            session,
            panes,
        })
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != HANDOFF_VERSION {
            return Err(data("unsupported handoff version"));
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(data("handoff protocol version mismatch"));
        }
        if self.panes.len() > MAX_HANDOFF_FDS {
            return Err(data("handoff contains too many PTYs"));
        }
        // Layout validation belongs to the importer, after it has accepted the
        // transport. Keeping it out of this boundary lets a new daemon report
        // a precise import error while the source still owns every PTY.
        Ok(())
    }
}

/// Descriptors received by the importer. It must import or close every fd.
pub(crate) struct ReceivedHandoff {
    pub(crate) manifest: HandoffManifest,
    pub(crate) fds: Vec<RawFd>,
    pub(crate) stream: UnixStream,
}

/// Bind a temporary private endpoint. This must never be the public or hook
/// socket alias; callers keep those paths until their final commit.
pub(crate) fn bind_listener(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub(crate) fn accept_and_validate(
    listener: &UnixListener,
    token: &str,
    manifest: &HandoffManifest,
) -> io::Result<UnixStream> {
    accept_and_validate_with_timeout(listener, token, manifest, DEFAULT_TIMEOUT)
}

/// Authenticate and send the bounded JSON manifest before transferring any fd.
fn accept_and_validate_with_timeout(
    listener: &UnixListener,
    token: &str,
    manifest: &HandoffManifest,
    timeout: Duration,
) -> io::Result<UnixStream> {
    valid_token(token)?;
    manifest.validate()?;
    let bytes = manifest_bytes(manifest)?;
    let (mut stream, _) = accept_with_timeout(listener, timeout)?;
    configure(&stream, timeout)?;
    if line(&mut stream, MAX_TOKEN_BYTES)? != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff token mismatch",
        ));
    }
    write_frame(&mut stream, &bytes)?;
    if line(&mut stream, 32)? != "validated" {
        return Err(data("handoff importer rejected manifest"));
    }
    Ok(stream)
}

pub(crate) fn receive(path: &Path, token: &str) -> io::Result<ReceivedHandoff> {
    receive_with_timeout(path, token, DEFAULT_TIMEOUT)
}

/// The importer has built its paused runtime and bound its private listener.
pub(crate) fn ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

/// The source only switches public aliases after this acknowledgement.
pub(crate) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    if line(stream, 32)? == "ready" {
        Ok(())
    } else {
        Err(data("handoff importer was not ready"))
    }
}

/// Tell the target that the public aliases now address its staged listener.
pub(crate) fn commit(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"commit\n")?;
    stream.flush()
}

pub(crate) fn wait_commit(stream: &mut UnixStream) -> io::Result<()> {
    if line(stream, 32)? == "commit" {
        Ok(())
    } else {
        Err(data("handoff source did not commit"))
    }
}

pub(crate) fn committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"committed\n")?;
    stream.flush()
}

pub(crate) fn wait_committed(stream: &mut UnixStream) -> io::Result<()> {
    if line(stream, 32)? == "committed" {
        Ok(())
    } else {
        Err(data("handoff importer did not commit"))
    }
}

fn receive_with_timeout(
    path: &Path,
    token: &str,
    timeout: Duration,
) -> io::Result<ReceivedHandoff> {
    valid_token(token)?;
    let mut stream = UnixStream::connect(path)?;
    configure(&stream, timeout)?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let bytes = read_frame(&mut stream)?;
    let manifest = serde_json::from_slice::<HandoffManifest>(&bytes)
        .map_err(io::Error::other)
        .and_then(|manifest| {
            manifest.validate()?;
            Ok(manifest)
        });
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = stream.write_all(b"rejected\n");
            let _ = stream.flush();
            return Err(error);
        }
    };
    stream.write_all(b"validated\n")?;
    stream.flush()?;
    let fds = recv_fds(&stream, manifest.panes.len())?;
    Ok(ReceivedHandoff {
        manifest,
        fds,
        stream,
    })
}

/// Send descriptor duplicates only after the importer has accepted the manifest.
pub(crate) fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_HANDOFF_FDS {
        return Err(input("handoff supports at most 64 PTYs"));
    }
    if fds.is_empty() {
        return (&*stream).write_all(b"F");
    }
    let marker = *b"F";
    let iov = [libc::iovec {
        iov_base: marker.as_ptr() as *mut _,
        iov_len: 1,
    }];
    let bytes = std::mem::size_of_val(fds);
    let mut control = vec![0; unsafe { libc::CMSG_SPACE(bytes as u32) as usize }];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = iov.as_ptr() as *mut _;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut _;
    message.msg_controllen = control.len();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        if cmsg.is_null() {
            return Err(io::Error::other(
                "could not allocate handoff control message",
            ));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), bytes);
        if libc::sendmsg(stream.as_raw_fd(), &message, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn close_fds(fds: impl IntoIterator<Item = RawFd>) {
    for fd in fds {
        let _ = unsafe { libc::close(fd) };
    }
}

pub(crate) fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    if expected > MAX_HANDOFF_FDS {
        return Err(data("handoff contains too many PTYs"));
    }
    let mut marker = [0; 1];
    let mut iov = [libc::iovec {
        iov_base: marker.as_mut_ptr() as *mut _,
        iov_len: 1,
    }];
    let mut control = vec![
        0;
        unsafe {
            libc::CMSG_SPACE((MAX_HANDOFF_FDS * std::mem::size_of::<RawFd>()) as u32) as usize
        }
    ];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = iov.as_mut_ptr();
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut _;
    message.msg_controllen = control.len();
    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = ancillary_fds(&message);
    if read != 1
        || marker != *b"F"
        || message.msg_flags & libc::MSG_CTRUNC != 0
        || fds.len() != expected
    {
        close_fds(fds.drain(..));
        return Err(data("handoff PTY descriptor transfer is invalid"));
    }
    for &fd in &fds {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            close_fds(fds);
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fds)
}

fn ancillary_fds(message: &libc::msghdr) -> Vec<RawFd> {
    let mut result = Vec::new();
    unsafe {
        let mut current = libc::CMSG_FIRSTHDR(message);
        while !current.is_null() {
            if (*current).cmsg_level == libc::SOL_SOCKET && (*current).cmsg_type == libc::SCM_RIGHTS
            {
                let bytes =
                    ((*current).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let items = bytes / std::mem::size_of::<RawFd>();
                let values = libc::CMSG_DATA(current) as *const RawFd;
                for item in 0..items {
                    result.push(*values.add(item));
                }
            }
            current = libc::CMSG_NXTHDR(message, current);
        }
    }
    result
}

fn manifest_bytes(manifest: &HandoffManifest) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(manifest).map_err(io::Error::other)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(input("handoff manifest exceeds 1 MiB"));
    }
    Ok(bytes)
}
fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    let size = u32::try_from(bytes.len()).map_err(|_| input("handoff manifest is too large"))?;
    stream.write_all(&size.to_be_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()
}
fn read_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix)?;
    let size = u32::from_be_bytes(prefix) as usize;
    if size > MAX_MANIFEST_BYTES {
        return Err(data("handoff manifest exceeds 1 MiB"));
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn line(stream: &mut UnixStream, maximum: usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0; 1];
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return String::from_utf8(bytes).map_err(|_| data("handoff line is not UTF-8"));
        }
        if bytes.len() == maximum {
            return Err(data("handoff line exceeds limit"));
        }
        bytes.push(byte[0]);
    }
}
fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(stream) => return Ok(stream),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for handoff importer",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}
fn configure(stream: &UnixStream, timeout: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}
fn valid_token(token: &str) -> io::Result<()> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.contains(['\n', '\r']) {
        return Err(input("invalid handoff token"));
    }
    Ok(())
}
fn data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::fd::IntoRawFd, thread};

    fn manifest() -> HandoffManifest {
        HandoffManifest::new(
            SessionFile {
                version: 1,
                name: "handoff".into(),
                active_workspace: 1,
                workspaces: vec![],
            },
            vec![],
        )
        .unwrap()
    }
    fn directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kodade-handoff-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn private_listener_is_owner_only() {
        let path = directory().join("handoff.sock");
        let _listener = bind_listener(&path).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn rejects_an_unauthenticated_importer_before_sending_manifest() {
        let path = directory().join("handoff.sock");
        let listener = bind_listener(&path).unwrap();
        let peer = thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(path).unwrap();
                stream.write_all(b"wrong\n").unwrap();
            }
        });
        let error = accept_and_validate_with_timeout(
            &listener,
            "right",
            &manifest(),
            Duration::from_secs(1),
        )
        .unwrap_err();
        peer.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
    #[test]
    fn bounds_a_manifest_before_allocating_it() {
        let (mut source, mut target) = UnixStream::pair().unwrap();
        thread::spawn(move || target.write_all(&(u32::MAX).to_be_bytes()).unwrap());
        assert_eq!(
            read_frame(&mut source).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn transfers_expected_descriptors_close_on_exec() {
        let (source, target) = UnixStream::pair().unwrap();
        let fd = fs::File::open("/dev/null").unwrap().into_raw_fd();
        let sender = thread::spawn(move || {
            send_fds(&source, &[fd]).unwrap();
            unsafe { libc::close(fd) };
        });
        let received = recv_fds(&target, 1).unwrap();
        sender.join().unwrap();
        assert_ne!(
            unsafe { libc::fcntl(received[0], libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        close_fds(received);
    }
    #[test]
    fn rejects_unexpected_descriptors() {
        let (source, target) = UnixStream::pair().unwrap();
        let fd = fs::File::open("/dev/null").unwrap().into_raw_fd();
        let sender = thread::spawn(move || {
            send_fds(&source, &[fd]).unwrap();
            unsafe { libc::close(fd) };
        });
        assert_eq!(
            recv_fds(&target, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        sender.join().unwrap();
    }
    #[test]
    fn importer_timeout_preserves_source_ownership() {
        let path = directory().join("handoff.sock");
        let listener = bind_listener(&path).unwrap();
        assert_eq!(
            accept_and_validate_with_timeout(
                &listener,
                "token",
                &manifest(),
                Duration::from_millis(20)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
