//! Load one bounded image on the daemon machine, independent of its clients.

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::{Decompress, FlushDecompress, Status};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use super::MAX_IMAGE;

pub(super) fn load(params: &BTreeMap<String, String>, encoded: &[u8]) -> Result<Vec<u8>> {
    let payload = STANDARD.decode(encoded).context("invalid image base64")?;
    let bytes = match params.get("t").map(String::as_str).unwrap_or("d") {
        "d" => payload,
        "f" | "t" => read_file(params, &payload)?,
        "s" => read_shared_memory(params, &payload)?,
        _ => bail!("unsupported image medium"),
    };
    ensure!(bytes.len() <= MAX_IMAGE, "image exceeds 8 MiB");
    match params.get("o").map(String::as_str) {
        None => Ok(bytes),
        Some("z") => {
            // One extra byte distinguishes a full valid image from a bomb.
            let mut output = vec![0; MAX_IMAGE + 1];
            let mut decoder = Decompress::new(true);
            let status = decoder
                .decompress(&bytes, &mut output, FlushDecompress::Finish)
                .context("invalid zlib image")?;
            ensure!(
                decoder.total_out() <= MAX_IMAGE as u64,
                "image exceeds 8 MiB after decompression"
            );
            ensure!(
                status == Status::StreamEnd && decoder.total_in() == bytes.len() as u64,
                "incomplete zlib image or trailing data"
            );
            output.truncate(decoder.total_out() as usize);
            Ok(output)
        }
        Some(_) => bail!("unsupported image compression"),
    }
}

fn read_file(params: &BTreeMap<String, String>, payload: &[u8]) -> Result<Vec<u8>> {
    let name = std::str::from_utf8(payload).context("image path is not UTF-8")?;
    ensure!(
        !name.is_empty() && name.len() <= 4096 && !name.contains('\0'),
        "invalid image path"
    );
    let path = Path::new(name);
    // Resolve symlinks as the protocol requires, then exclude pseudo files.
    let resolved = path.canonicalize().context("image file unavailable")?;
    #[cfg(unix)]
    ensure!(
        !resolved.starts_with("/proc")
            && !resolved.starts_with("/sys")
            && (!resolved.starts_with("/dev") || resolved.starts_with("/dev/shm")),
        "image must be an ordinary file"
    );
    ensure!(
        resolved.metadata()?.is_file(),
        "image must be a regular file"
    );
    let temporary = params.get("t").is_some_and(|v| v == "t");
    if temporary {
        ensure!(
            name.contains("tty-graphics-protocol") && temporary_path(path)?,
            "temporary image needs a protocol name in a temporary directory"
        );
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Checking metadata alone races a producer replacing a file with FIFO.
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(&resolved).context("cannot open image file")?;
    ensure!(file.metadata()?.is_file(), "image must be a regular file");
    verify_owner(&file)?;
    let _cleanup = temporary.then(|| TemporaryFile(path.to_path_buf()));
    read_range(file, params)
}

/// A pane may only consume media it owns. Verify the descriptor after opening
/// so a rename between pathname checks cannot substitute another user's file.
#[cfg(unix)]
fn verify_owner(file: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    ensure!(
        file.metadata()?.uid() == unsafe { libc::geteuid() },
        "image media must be owned by the daemon user"
    );
    Ok(())
}

#[cfg(not(unix))]
fn verify_owner(_: &File) -> Result<()> {
    Ok(())
}

fn read_range(mut file: File, params: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    let offset = u64::from(super::number(params, "O", 0)?);
    let size = usize::try_from(super::number(params, "S", 0)?)?;
    ensure!(size <= MAX_IMAGE, "image range exceeds 8 MiB");
    file.seek(SeekFrom::Start(offset))
        .context("cannot seek image")?;
    let mut bytes = Vec::new();
    file.take(if size == 0 { MAX_IMAGE + 1 } else { size } as u64)
        .read_to_end(&mut bytes)
        .context("cannot read image")?;
    ensure!(bytes.len() <= MAX_IMAGE, "image exceeds 8 MiB");
    ensure!(
        size == 0 || bytes.len() == size,
        "image range extends beyond its data"
    );
    Ok(bytes)
}

fn temporary_path(path: &Path) -> Result<bool> {
    let parent = path
        .parent()
        .context("temporary image has no parent")?
        .canonicalize()?;
    Ok([
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/dev/shm"),
    ]
    .into_iter()
    .filter_map(|p| p.canonicalize().ok())
    .any(|root| parent.starts_with(root)))
}

struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn read_shared_memory(params: &BTreeMap<String, String>, payload: &[u8]) -> Result<Vec<u8>> {
    use std::{ffi::CString, os::fd::FromRawFd};
    ensure!(
        payload.len() > 1
            && payload.len() <= 255
            && payload[0] == b'/'
            && !payload[1..].contains(&b'/'),
        "invalid shared memory name"
    );
    let name = CString::new(payload).context("invalid shared memory name")?;
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    ensure!(fd >= 0, "shared memory unavailable");
    let file = unsafe { File::from_raw_fd(fd) };
    verify_owner(&file)?;
    // Once opened, our descriptor stays valid and no path is left behind.
    unsafe {
        libc::shm_unlink(name.as_ptr());
    }
    #[cfg(target_os = "linux")]
    {
        read_range(file, params)
    }
    #[cfg(not(target_os = "linux"))]
    {
        read_shared_mapping(file, params)
    }
}

// BSD shared-memory descriptors support mapping but not ordinary read(2).
#[cfg(all(unix, not(target_os = "linux")))]
fn read_shared_mapping(file: File, params: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let offset = u64::from(super::number(params, "O", 0)?);
    let requested = u64::from(super::number(params, "S", 0)?);
    let available = file
        .metadata()?
        .len()
        .checked_sub(offset)
        .context("image offset exceeds shared memory")?;
    let size = if requested == 0 { available } else { requested };
    ensure!(
        size > 0 && size <= available && size <= MAX_IMAGE as u64,
        "invalid shared memory image range"
    );
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    ensure!(page > 0, "cannot determine shared memory page size");
    let aligned = offset / page as u64 * page as u64;
    let delta = (offset - aligned) as usize;
    let length = size as usize + delta;
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            aligned as libc::off_t,
        )
    };
    ensure!(
        mapping != libc::MAP_FAILED,
        "cannot map shared memory image"
    );
    let bytes = unsafe {
        std::slice::from_raw_parts(mapping.cast::<u8>().add(delta), size as usize).to_vec()
    };
    unsafe {
        libc::munmap(mapping, length);
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_shared_memory(_: &BTreeMap<String, String>, _: &[u8]) -> Result<Vec<u8>> {
    bail!("shared memory images require a Unix daemon; use a file or direct transfer")
}
