//! Verified standalone release updates. Package-managed installs only receive guidance.

#[cfg(windows)]
use std::collections::HashSet;
use std::{
    fs,
    fs::OpenOptions,
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;
const MAX_BINARY: usize = 64 * 1024 * 1024;
const MAX_EXTRACTED_ARCHIVE: usize = 96 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}
pub fn select_release(channel: &str, body: &str) -> Result<Release> {
    if channel == "stable" {
        let release: Release =
            serde_json::from_str(body).context("parse stable release metadata")?;
        if release.draft || release.prerelease {
            bail!("stable endpoint returned a non-stable release");
        }
        return validate_release_tag(release);
    }
    let releases: Vec<Release> =
        serde_json::from_str(body).context("parse preview release metadata")?;
    releases
        .into_iter()
        .find(|release| release.prerelease && !release.draft)
        .context("no published preview release")
        .and_then(validate_release_tag)
}

fn validate_release_tag(release: Release) -> Result<Release> {
    if !release.tag_name.starts_with('v')
        || semver::Version::parse(release.tag_name.trim_start_matches('v')).is_err()
    {
        bail!("published release has an empty or invalid semantic version tag");
    }
    Ok(release)
}
pub fn metadata_url(channel: &str) -> &'static str {
    if channel == "preview" {
        "https://api.github.com/repos/Kodade/kodade-cli/releases"
    } else {
        "https://api.github.com/repos/Kodade/kodade-cli/releases/latest"
    }
}

pub fn channel_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("home directory unavailable")?
        .join(".config/kodade-cli/update.toml"))
}
pub fn saved_channel() -> Result<String> {
    read_channel(&channel_path()?)
}

fn read_channel(path: &Path) -> Result<String> {
    let source = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("stable".into()),
        Err(error) => return Err(error.into()),
    };
    let value: toml::Value = source.parse().context("parse update settings")?;
    match value.get("channel").and_then(toml::Value::as_str) {
        Some("stable" | "preview") => Ok(value["channel"].as_str().unwrap().into()),
        _ => bail!("update channel must be stable or preview"),
    }
}
pub fn save_channel(channel: &str) -> Result<()> {
    write_channel(&channel_path()?, channel)
}

fn write_channel(path: &Path, channel: &str) -> Result<()> {
    if !matches!(channel, "stable" | "preview") {
        bail!("update channel must be stable or preview");
    }
    fs::create_dir_all(path.parent().expect("config parent"))?;
    crate::atomic_file::write(path, format!("channel = {channel:?}\n").as_bytes())
}

pub fn platform_asset(version: &str) -> Result<String> {
    platform_asset_for(version, std::env::consts::OS, std::env::consts::ARCH)
}

/// The release asset for a Unix host reported over SSH. Windows remote
/// bootstrap follows the native distribution work tracked separately.
pub fn platform_asset_for(version: &str, os: &str, arch: &str) -> Result<String> {
    let target = match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => bail!("no standalone update asset for this platform"),
    };
    let extension = if os == "windows" { "zip" } else { "tar.gz" };
    Ok(format!("kodade-cli-{version}-{target}.{extension}"))
}

pub fn release_asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.as_str())
        .context("published release is missing the expected platform asset")
}

pub fn release_is_newer(tag: &str, installed: &str) -> bool {
    let Ok(release) = semver::Version::parse(tag.trim_start_matches('v')) else {
        return true;
    };
    let Ok(installed) = semver::Version::parse(installed.trim_start_matches('v')) else {
        return true;
    };
    release > installed
}

/// Commands are guidance only: package managers own their installed files.
pub fn package_upgrade_command(executable: &Path) -> Option<&'static str> {
    let path = executable.to_string_lossy();
    if path.contains("/Cellar/") || path.starts_with("/opt/homebrew/") {
        Some("brew upgrade kodade-cli")
    } else if path.starts_with("/usr/bin/") {
        Some("upgrade Ködade with the package manager that installed it")
    } else {
        None
    }
}

pub fn checksum(sums: &str, asset: &str) -> Result<String> {
    sums.lines()
        .find_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            (name.trim().trim_start_matches('*') == asset).then(|| hash.to_owned())
        })
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("SHA256SUMS has no valid checksum for {asset}"))
}

pub fn verify(bytes: &[u8], expected: &str) -> Result<()> {
    if bytes.len() > MAX_DOWNLOAD {
        bail!("release archive exceeds 64 MiB");
    }
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected.to_ascii_lowercase() {
        bail!("release checksum mismatch");
    }
    Ok(())
}

/// Verify a published archive before extracting the executable for a remote
/// install. The caller owns transport and atomic replacement on that host.
#[cfg_attr(windows, allow(dead_code))]
pub fn verified_remote_binary(archive: &[u8], expected: &str) -> Result<Vec<u8>> {
    verify(archive, expected)?;
    Ok(extract_tar_binary(archive)?.0)
}

/// Extract the one executable from a verified archive and atomically replace
/// the requested executable. The staged file lives beside the destination so
/// the final rename cannot cross filesystems.
pub fn install_archive(archive: &[u8], expected: &str, destination: &Path) -> Result<()> {
    verify(archive, expected)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before Unix epoch")
        .as_nanos();
    let destination = if destination.is_symlink() {
        fs::canonicalize(destination).context("resolve executable symlink")?
    } else {
        destination.to_owned()
    };
    let parent = destination
        .parent()
        .context("update destination has no parent directory")?;
    let staged = parent.join(format!(".kodade-cli.update-{}-{nonce}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut staged_file = options
        .open(&staged)
        .context("create update file beside destination")?;
    let mut cleanup = TempFileCleanup(Some(staged.clone()));
    let (binary, mode) = extract_binary(archive)?;
    #[cfg(not(unix))]
    let _ = mode;
    staged_file
        .write_all(&binary)
        .context("stage replacement executable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let current_mode = fs::metadata(&destination)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(mode & 0o755);
        staged_file.set_permissions(fs::Permissions::from_mode(current_mode | 0o100))?;
    }
    staged_file
        .sync_all()
        .context("sync replacement executable")?;
    drop(staged_file);
    #[cfg(unix)]
    {
        fs::rename(&staged, &destination).context("atomically replace executable")?;
        cleanup.0 = None;
        sync_parent(parent)?;
    }
    #[cfg(windows)]
    {
        let _helper = schedule_windows_replacement(&staged, &destination)?;
        // The detached helper owns the staged file after this point. It first
        // moves the old executable aside, then restores it if the replacement
        // move fails, so a failed update keeps a runnable binary.
        cleanup.0 = None;
    }
    Ok(())
}

#[cfg(windows)]
fn schedule_windows_replacement(staged: &Path, destination: &Path) -> Result<std::process::Child> {
    let old = destination.with_extension(format!("old-{}.exe", std::process::id()));
    let quote = |path: &Path| -> Result<String> {
        let text = path.to_str().context("update path is not valid Unicode")?;
        if text.contains('"') || text.contains('\r') || text.contains('\n') {
            bail!("update path contains unsupported characters");
        }
        Ok(format!("'{}'", text.replace('\'', "''")))
    };
    let staged = quote(staged)?;
    let destination = quote(destination)?;
    let old = quote(&old)?;
    // The child must outlive this executable. PowerShell's delayed loop waits
    // until the image handle is released, then performs a recoverable swap.
    let script = format!(
        "$ErrorActionPreference='Stop'; if(Test-Path -LiteralPath {destination}){{for($i=0;$i -lt 100;$i++){{try{{Move-Item -LiteralPath {destination} -Destination {old}; break}}catch{{Start-Sleep -Milliseconds 100}}}}; if(-not(Test-Path -LiteralPath {old})){{Remove-Item -LiteralPath {staged} -Force -ErrorAction SilentlyContinue; exit 1}}; try{{Move-Item -LiteralPath {staged} -Destination {destination}}}catch{{if(-not(Test-Path -LiteralPath {destination})){{Move-Item -LiteralPath {old} -Destination {destination}}}; Remove-Item -LiteralPath {staged} -Force -ErrorAction SilentlyContinue; exit 1}}; Remove-Item -LiteralPath {old} -Force -ErrorAction SilentlyContinue}}else{{try{{Move-Item -LiteralPath {staged} -Destination {destination}}}catch{{Remove-Item -LiteralPath {staged} -Force -ErrorAction SilentlyContinue; exit 1}}}}; exit 0"
    );
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .spawn()
        .context("start deferred Windows executable replacement")
}

struct TempFileCleanup(Option<PathBuf>);
impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn extract_binary(archive: &[u8]) -> Result<(Vec<u8>, u32)> {
    #[cfg(windows)]
    return extract_zip_binary(archive);
    #[cfg(not(windows))]
    extract_tar_binary(archive)
}

fn extract_tar_binary(archive: &[u8]) -> Result<(Vec<u8>, u32)> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(LimitedReader::new(decoder, MAX_EXTRACTED_ARCHIVE));
    let mut binary = None;
    for entry in archive.entries().context("read release archive")? {
        let entry = entry.context("read release archive entry")?;
        let path = entry.path().context("read release archive path")?;
        if path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            bail!("release archive contains an unsafe path");
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            bail!("release archive contains links");
        }
        if path.file_name().is_some_and(|name| name == "kodade-cli") {
            if !kind.is_file() || binary.is_some() {
                bail!("release archive must contain exactly one regular kodade-cli executable");
            }
            let mode = entry
                .header()
                .mode()
                .context("read executable permissions")?;
            if mode & 0o111 == 0 {
                bail!("release archive kodade-cli is not executable");
            }
            let mut bytes = Vec::new();
            entry
                .take((MAX_BINARY + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_BINARY {
                bail!("release archive executable exceeds 64 MiB");
            }
            binary = Some((bytes, mode));
        }
    }
    // Consume the gzip stream so an archive cannot hide an oversized payload
    // after the tar member we selected.
    let mut decoder = archive.into_inner();
    io::copy(&mut decoder, &mut io::sink()).context("read release archive")?;
    binary.context("release archive has no kodade-cli executable")
}

#[cfg(windows)]
fn extract_zip_binary(archive: &[u8]) -> Result<(Vec<u8>, u32)> {
    let mut archive = zip::ZipArchive::new(Cursor::new(archive)).context("read release zip")?;
    let mut binary = None;
    let mut extracted = 0usize;
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).context("read release zip entry")?;
        let name = entry.name();
        let path = Path::new(name);
        if path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            bail!("release zip contains an unsafe path");
        }
        if !names.insert(path.to_owned()) {
            bail!("release zip contains duplicate paths");
        }
        let mode = entry.unix_mode().unwrap_or(0);
        if mode & 0o170000 == 0o120000 {
            bail!("release zip contains links");
        }
        extracted = extracted.saturating_add(entry.size() as usize);
        if extracted > MAX_EXTRACTED_ARCHIVE {
            bail!("release zip exceeds 96 MiB extracted");
        }
        if path
            .file_name()
            .is_some_and(|file| file == "kodade-cli.exe")
        {
            if binary.is_some() || entry.is_dir() {
                bail!("release zip must contain exactly one regular kodade-cli.exe");
            }
            let mut bytes = Vec::new();
            entry
                .take((MAX_BINARY + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_BINARY {
                bail!("release zip executable exceeds 64 MiB");
            }
            binary = Some((bytes, 0));
        }
    }
    binary.context("release zip has no kodade-cli.exe executable")
}

#[cfg_attr(windows, allow(dead_code))]
struct LimitedReader<R> {
    inner: R,
    remaining: usize,
}

#[cfg_attr(windows, allow(dead_code))]
impl<R> LimitedReader<R> {
    fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

#[cfg_attr(windows, allow(dead_code))]
impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::other("release archive expands beyond 96 MiB"));
        }
        let limit = buffer.len().min(self.remaining);
        let read = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= read;
        Ok(read)
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    fs::File::open(parent)
        .context("open update destination directory")?
        .sync_all()
        .context("sync update destination directory")?;
    Ok(())
}

pub fn fetch(url: &str) -> Result<Vec<u8>> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-A",
            "kodade-cli-updater",
            "-H",
            "Accept: application/vnd.github+json",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "67108864",
            url,
        ])
        .output()
        .context("run curl")?;
    if !output.status.success() {
        bail!("download failed: {url}");
    }
    if output.stdout.len() > MAX_DOWNLOAD {
        bail!("release archive exceeds 64 MiB");
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_requires_named_valid_digest() {
        let sum = "a".repeat(64) + "  asset.tar.gz\n";
        assert_eq!(checksum(&sum, "asset.tar.gz").unwrap(), "a".repeat(64));
        let binary_marker = "b".repeat(64) + " *asset.tar.gz\n";
        assert_eq!(
            checksum(&binary_marker, "asset.tar.gz").unwrap(),
            "b".repeat(64)
        );
        assert!(checksum("bad  asset.tar.gz", "asset.tar.gz").is_err());
    }
    #[test]
    fn verifier_rejects_tampering() {
        let hash = format!("{:x}", Sha256::digest(b"ok"));
        verify(b"ok", &hash).unwrap();
        assert!(verify(b"no", &hash).is_err());
    }

    #[test]
    fn release_fixtures_select_published_channel_releases() {
        let stable = select_release("stable", r#"{"tag_name":"v1.2.3"}"#).unwrap();
        assert_eq!(stable.tag_name, "v1.2.3");
        assert!(
            select_release("stable", r#"{"tag_name":"v2.0.0-rc.1","prerelease":true}"#).is_err()
        );
        let preview = select_release(
            "preview",
            r#"[
            {"tag_name":"v3.0.0-rc.1","prerelease":true},
            {"tag_name":"v4.0.0-rc.1","prerelease":true,"draft":true}
        ]"#,
        )
        .unwrap();
        assert_eq!(preview.tag_name, "v3.0.0-rc.1");
        assert!(select_release("preview", "[]").is_err());
        assert!(select_release("stable", r#"{"tag_name":""}"#).is_err());
        assert!(select_release("stable", r#"{"tag_name":"latest"}"#).is_err());
    }

    #[test]
    fn release_fixture_uses_published_asset_url() {
        let release = select_release(
            "stable",
            r#"{"tag_name":"v1.2.3","assets":[{"name":"SHA256SUMS","browser_download_url":"https://example.test/sums"}]}"#,
        )
        .unwrap();
        assert_eq!(
            release_asset_url(&release, "SHA256SUMS").unwrap(),
            "https://example.test/sums"
        );
        assert!(release_asset_url(&release, "missing").is_err());
    }

    #[test]
    fn remote_platform_assets_match_unix_release_names() {
        assert_eq!(
            platform_asset_for("1.2.3", "linux", "aarch64").unwrap(),
            "kodade-cli-1.2.3-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            platform_asset_for("1.2.3", "windows", "x86_64").unwrap(),
            "kodade-cli-1.2.3-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn only_newer_semantic_versions_are_installed() {
        assert!(release_is_newer("v1.2.4", "1.2.3"));
        assert!(!release_is_newer("v1.2.3", "1.2.3"));
        assert!(!release_is_newer("v1.2.2", "1.2.3"));
        assert!(release_is_newer("unparseable", "1.2.3"));
    }

    #[test]
    fn channel_setting_preserves_other_config_files() {
        let root = std::env::temp_dir().join(format!("kodade-channel-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("update.toml");
        write_channel(&path, "preview").unwrap();
        assert_eq!(read_channel(&path).unwrap(), "preview");
        assert!(write_channel(&path, "nightly").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_paths_receive_guidance() {
        assert_eq!(
            package_upgrade_command(Path::new("/opt/homebrew/bin/kodade-cli")),
            Some("brew upgrade kodade-cli")
        );
        assert_eq!(
            package_upgrade_command(Path::new("/usr/bin/kodade-cli")),
            Some("upgrade Ködade with the package manager that installed it")
        );
        assert!(package_upgrade_command(Path::new("/usr/local/bin/kodade-cli")).is_none());
        assert!(package_upgrade_command(Path::new("/tmp/kodade-cli")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn verified_archive_replaces_an_isolated_destination() {
        use std::os::unix::fs::PermissionsExt;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("kodade-update-fixture-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let archive = fixture_archive(&[("package/kodade-cli", b"new binary", 0o755)]);
        let digest = format!("{:x}", Sha256::digest(&archive));
        let destination = root.join("installed-kodade-cli");
        fs::write(&destination, b"old binary").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();

        install_archive(&archive, &digest, &destination).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new binary");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let target = root.join("symlink-target");
        let link = root.join("symlinked-kodade-cli");
        fs::write(&target, b"old target").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        install_archive(&archive, &digest, &link).unwrap();
        assert!(link.is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"new binary");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_update_preserves_executable_and_removes_staging() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("kodade-update-rejected-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let destination = root.join("kodade-cli");
        fs::write(&destination, b"working executable").unwrap();
        let archive = fixture_archive(&[("package/kodade-cli", b"replacement", 0o755)]);
        assert!(install_archive(&archive, &"0".repeat(64), &destination).is_err());
        let truncated = &archive[..archive.len() / 2];
        let digest = format!("{:x}", Sha256::digest(truncated));
        assert!(install_archive(truncated, &digest, &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"working executable");
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            1,
            "staging file leaked"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_deferred_replacement_waits_for_a_running_executable() {
        use std::time::{Duration, Instant};

        let root =
            std::env::temp_dir().join(format!("kodade-update-windows-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let destination = root.join("kodade-cli.exe");
        fs::copy(std::env::var_os("COMSPEC").unwrap(), &destination).unwrap();
        let mut running = Command::new(&destination)
            .args(["/C", "ping", "127.0.0.1", "-n", "3", ">", "NUL"])
            .spawn()
            .unwrap();
        let archive = fixture_zip(&[("package/kodade-cli.exe", b"updated executable")]);
        let digest = format!("{:x}", Sha256::digest(&archive));

        install_archive(&archive, &digest, &destination).unwrap();
        running.wait().unwrap();
        // The detached helper retries a busy executable for up to ten seconds.
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline
            && !matches!(fs::read(&destination), Ok(bytes) if bytes == b"updated executable")
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(fs::read(&destination).unwrap(), b"updated executable");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_failed_replacement_restores_the_old_executable() {
        let root =
            std::env::temp_dir().join(format!("kodade-update-restore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let destination = root.join("kodade-cli.exe");
        fs::write(&destination, b"working executable").unwrap();

        let mut helper =
            schedule_windows_replacement(&root.join("missing-update"), &destination).unwrap();
        assert!(!helper.wait().unwrap().success());
        assert_eq!(fs::read(&destination).unwrap(), b"working executable");
        assert!(!destination
            .with_extension(format!("old-{}.exe", std::process::id()))
            .exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_rejects_links_duplicates_nonexecutables_and_large_binaries() {
        let noexec = fixture_archive(&[("package/kodade-cli", b"not executable", 0o644)]);
        assert!(extract_binary(&noexec).is_err());
        let duplicate = fixture_archive(&[
            ("package/kodade-cli", b"one", 0o755),
            ("other/kodade-cli", b"two", 0o755),
        ]);
        assert!(extract_binary(&duplicate).is_err());
        let oversized = fixture_archive(&[("package/kodade-cli", &vec![0; MAX_BINARY + 1], 0o755)]);
        assert!(extract_binary(&oversized).is_err());

        for kind in [tar::EntryType::Symlink, tar::EntryType::Link] {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            {
                let mut tar = tar::Builder::new(&mut encoder);
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(kind);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_cksum();
                tar.append_link(&mut header, "package/link", "/outside")
                    .unwrap();
                tar.finish().unwrap();
            }
            assert!(extract_binary(&encoder.finish().unwrap()).is_err());
        }
    }

    fn fixture_archive(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut tar = tar::Builder::new(&mut encoder);
            for (path, bytes, mode) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                tar.append_data(&mut header, path, *bytes).unwrap();
            }
            tar.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[cfg(windows)]
    fn fixture_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, bytes) in entries {
            archive
                .start_file(*path, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }
}
