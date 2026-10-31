//! Filesystem checks shared by public, hook, and handoff endpoints.
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::Path,
};

pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // Reject redirected application/runtime ancestors. macOS system aliases
    // (/tmp and /var) are OS-managed and expected in standard runtime paths.
    for ancestor in path.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(m) if m.file_type().is_symlink() => {
                if cfg!(target_os = "macos")
                    && [Path::new("/tmp"), Path::new("/var")].contains(&ancestor)
                {
                    continue;
                }
                return Err(io::Error::other("symlink ancestor in socket directory"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    // Only the dedicated leaf belongs to us. Never chmod user ancestors.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let m = directory.metadata()?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe private socket directory: {}", path.display()),
        ));
    }
    // Migrate legacy 0755/0777 leaves without following a swapped symlink.
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    let current = fs::symlink_metadata(path)?;
    if !current.is_dir() || current.dev() != m.dev() || current.ino() != m.ino() {
        return Err(io::Error::other(
            "socket directory changed during validation",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity(u64, u64);
pub(crate) fn identity(path: &Path) -> io::Result<Identity> {
    let m = fs::symlink_metadata(path)?;
    if !m.file_type().is_socket() || m.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing unsafe non-socket or foreign socket: {}",
                path.display()
            ),
        ));
    }
    Ok(Identity(m.dev(), m.ino()))
}
pub(crate) fn remove(path: &Path, expected: Identity) -> io::Result<()> {
    if identity(path)? != expected {
        return Err(io::Error::other("socket identity changed"));
    }
    fs::remove_file(path)
}
pub(crate) fn restrict(path: &Path) -> io::Result<()> {
    identity(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Serialize competing cold starts from stale probing through binding.
pub(crate) fn startup_lock(directory: &Path) -> io::Result<fs::File> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(".socket-lock"))?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.nlink() != 1
        || m.mode() & 0o077 != 0
    {
        return Err(io::Error::other("unsafe socket startup lock"));
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::symlink, net::UnixListener};

    #[test]
    fn permissive_umask_subprocess() {
        if std::env::var_os("KODADE_SOCKET_UMASK_TEST").is_some() {
            unsafe {
                libc::umask(0);
            }
            let root = std::env::temp_dir().join(format!("kodade-safety-{}", std::process::id()));
            private_directory(&root).unwrap();
            assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
            let path = root.join("endpoint");
            let listener = crate::handoff::bind_listener(&path).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            let hook_dir = root.join("hooks");
            private_directory(&hook_dir).unwrap();
            let hook = hook_dir.join("hook");
            fs::hard_link(&path, &hook).unwrap();
            let published = root.join("published");
            fs::rename(&path, &published).unwrap();
            assert_eq!(fs::metadata(&hook).unwrap().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&published).unwrap().mode() & 0o777, 0o600);
            let id = identity(&published).unwrap();
            drop(listener);
            remove(&published, id).unwrap();
            remove(&hook, id).unwrap();
            fs::remove_dir(hook_dir).unwrap();
            fs::remove_dir(root).unwrap();
            return;
        }
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "socket_safety::tests::permissive_umask_subprocess",
                "--exact"
            ])
            .env("KODADE_SOCKET_UMASK_TEST", "1")
            .status()
            .unwrap()
            .success());
    }

    #[tokio::test]
    async fn stale_cleanup_preserves_files_symlinks_and_live_sockets() {
        let root = std::env::temp_dir().join(format!("kodade-stale-{}", std::process::id()));
        private_directory(&root).unwrap();
        let path = root.join("endpoint");
        fs::write(&path, b"user data").unwrap();
        assert!(crate::remove_stale_socket(&path).await.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"user data");
        fs::remove_file(&path).unwrap();
        symlink(root.join("missing"), &path).unwrap();
        assert!(crate::remove_stale_socket(&path).await.is_err());
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_file(&path).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        assert!(crate::remove_stale_socket(&path).await.is_err());
        let id = identity(&path).unwrap();
        let mut clients = Vec::new();
        for _ in 0..256 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(10),
                tokio::net::UnixStream::connect(&path),
            )
            .await
            {
                Ok(Ok(client)) => clients.push(client),
                _ => break,
            }
        }
        assert!(crate::remove_stale_socket(&path).await.is_err());
        assert_eq!(identity(&path).unwrap(), id);
        drop(clients);
        drop(listener);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(remove(&path, id).is_err());
        fs::remove_file(&path).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        crate::remove_stale_socket(&path).await.unwrap();
        let alias = root.with_extension("alias");
        symlink(&root, &alias).unwrap();
        assert!(private_directory(&alias).is_err());
        let redirected_hook = alias.join("hooks");
        assert!(private_directory(&redirected_hook).is_err());
        assert!(!root.join("hooks").exists());
        fs::remove_file(alias).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        private_directory(&root).unwrap();
        assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        private_directory(&root).unwrap();
        assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        fs::remove_dir(root).unwrap();
    }
}
