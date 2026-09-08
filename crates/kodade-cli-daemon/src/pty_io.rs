//! Interruptible PTY reads and bounded writes over duplicated Unix descriptors.

use std::{
    fs::File,
    io::{self, Write},
    os::fd::{AsRawFd, FromRawFd, RawFd},
    time::{Duration, Instant},
};

pub(crate) fn pair(fd: RawFd) -> io::Result<(File, Box<dyn Write + Send>)> {
    fn duplicate(fd: RawFd) -> io::Result<File> {
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(copy) })
    }
    let reader = duplicate(fd)?;
    let writer = duplicate(fd)?;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((reader, Box::new(Writer(writer))))
}

struct Writer(File);
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.0.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                result => return result,
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "pane input remained blocked",
                ));
            }
            let mut ready = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut ready, 1, 50) };
            if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::net::UnixStream, thread};

    #[test]
    fn nonblocking_reads_do_not_lose_backpressured_input() {
        let (local, mut remote) = UnixStream::pair().unwrap();
        let (mut reader, mut writer) = pair(local.as_raw_fd()).unwrap();
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let payload = vec![0x5a; 1024 * 1024];
        let peer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut received = vec![0; 1024 * 1024];
            remote.read_exact(&mut received).unwrap();
            received
        });
        writer.write_all(&payload).unwrap();
        assert_eq!(peer.join().unwrap(), payload);
    }
}
