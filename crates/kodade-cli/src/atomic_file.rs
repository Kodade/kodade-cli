//! Replace a small configuration file without truncating the user's original.

use anyhow::{Context, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn write(path: &Path, contents: &[u8]) -> Result<()> {
    // Follow existing config symlinks (common with dotfiles) instead of replacing them.
    let path = if path.is_symlink() {
        fs::canonicalize(path).context("resolve configuration symlink")?
    } else {
        path.to_owned()
    };
    let parent = path
        .parent()
        .context("configuration file needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let (temp, mut file) = loop {
        let candidate = parent.join(format!(
            ".kodade-{}-{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match options.open(&candidate) {
            Ok(file) => break (Temp(candidate), file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create configuration temporary file"),
        }
    };
    if let Ok(metadata) = fs::metadata(&path) {
        file.set_permissions(metadata.permissions())?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp.0, &path).with_context(|| format!("replace {}", path.display()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_symlink_target_without_replacing_link() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("kodade-atomic-link-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("dotfiles.toml");
        let link = dir.join("config.toml");
        fs::write(&target, "before").unwrap();
        symlink(&target, &link).unwrap();
        write(&link, b"after").unwrap();
        assert!(link.is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "after");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
