//! Verified bootstrap of a Unix Ködade host over SSH, shared by native clients.

use std::{process::Stdio, time::Duration};

use anyhow::{bail, Context, Result};
use sha2::Digest;
use tokio::process::Command;

use crate::update;

pub async fn prepare_machine<F, U>(
    host: &str,
    install: bool,
    probe_args: Vec<String>,
    version_args: Vec<String>,
    upload_args: U,
    fetch: F,
) -> Result<()>
where
    F: Fn(&str) -> Result<Vec<u8>>,
    U: FnOnce(usize, &str, &str) -> Vec<String>,
{
    let probe = ssh_output(&probe_args).await?;
    if !probe.status.success() {
        bail!(
            "could not probe {host}: {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        );
    }
    let probe_text = String::from_utf8_lossy(&probe.stdout);
    let mut fields = probe_text.lines();
    let os = fields
        .next()
        .context("remote probe did not report an operating system")?;
    let arch = fields
        .next()
        .context("remote probe did not report an architecture")?;
    let version = ssh_output(&version_args).await?;
    if version.status.success() && version_is_compatible(&version.stdout) {
        return Ok(());
    }
    if !install {
        bail!("{host} has no compatible kodade-cli; run `kodade-cli machine prepare {host} --install` (or add with --install)");
    }
    let (binary, release_version) = verified_release_binary(os, arch, fetch)?;
    let checksum = format!("{:x}", sha2::Sha256::digest(&binary));
    upload(
        upload_args(binary.len(), &checksum, &release_version),
        &binary,
    )
    .await?;
    let installed = ssh_output(&version_args).await?;
    if !installed.status.success() || !version_is_compatible(&installed.stdout) {
        bail!("remote install on {host} did not produce a compatible kodade-cli");
    }
    Ok(())
}

pub fn version_is_compatible(stdout: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return false;
    };
    let Some(remote) = text.trim().strip_prefix("kodade-cli ") else {
        return false;
    };
    let Ok(remote) = semver::Version::parse(remote) else {
        return false;
    };
    remote == semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("package version is semver")
}

pub(crate) fn verified_release_binary<F>(
    os: &str,
    arch: &str,
    fetch: F,
) -> Result<(Vec<u8>, String)>
where
    F: Fn(&str) -> Result<Vec<u8>>,
{
    let metadata = String::from_utf8(fetch(update::metadata_url("stable"))?)?;
    let release = update::select_release("stable", &metadata)?;
    let version = release.tag_name.trim_start_matches('v').to_owned();
    if version != env!("CARGO_PKG_VERSION") {
        bail!(
            "published release {} does not match local kodade-cli {}",
            release.tag_name,
            env!("CARGO_PKG_VERSION")
        );
    }
    let asset = update::platform_asset_for(&version, os_to_target(os)?, arch_to_target(arch)?)?;
    let sums = String::from_utf8(fetch(update::release_asset_url(&release, "SHA256SUMS")?)?)?;
    let archive = fetch(update::release_asset_url(&release, &asset)?)?;
    Ok((
        update::verified_remote_binary(&archive, &update::checksum(&sums, &asset)?)?,
        version,
    ))
}

pub(crate) fn os_to_target(value: &str) -> Result<&'static str> {
    match value {
        "Linux" => Ok("linux"),
        "Darwin" => Ok("macos"),
        _ => bail!("remote OS {value:?} has no standalone preparation artifact"),
    }
}
pub(crate) fn arch_to_target(value: &str) -> Result<&'static str> {
    match value {
        "x86_64" | "amd64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        _ => bail!("remote architecture {value:?} has no standalone preparation artifact"),
    }
}

async fn upload(args: Vec<String>, binary: &[u8]) -> Result<()> {
    let child = Command::new("ssh")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start remote install")?;
    upload_to_child(child, binary, Duration::from_secs(45)).await
}

// Drain diagnostics while uploading: a remote process may fill stderr before
// reading stdin. One deadline covers both backpressure and process completion.
async fn upload_to_child(
    mut child: tokio::process::Child,
    binary: &[u8],
    deadline: Duration,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut stdin = child
        .stdin
        .take()
        .context("remote install stdin unavailable")?;
    let write = async move {
        stdin
            .write_all(binary)
            .await
            .context("upload verified remote binary")?;
        drop(stdin);
        Ok::<_, anyhow::Error>(())
    };
    let wait = async move {
        child
            .wait_with_output()
            .await
            .context("wait for remote install")
    };
    let (_, output) = tokio::time::timeout(deadline, async { tokio::try_join!(write, wait) })
        .await
        .context("remote install timed out")??;
    if !output.status.success() {
        bail!(
            "remote install failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn ssh_output(args: &[String]) -> Result<std::process::Output> {
    tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("ssh")
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("SSH command timed out after 30s")?
    .context("run ssh")
}

/// All interpolated values come from a checksum-verified local release.
pub fn upload_command(
    expected_bytes: usize,
    expected_sha256: &str,
    expected_version: &str,
) -> String {
    format!(
        "set -eu; umask 077; dest=\"$HOME/.local/bin/kodade-cli\"; dir=\"${{dest%/*}}\"; mkdir -p \"$dir\"; tmp=$(mktemp \"$dir/.kodade-cli.XXXXXX\"); trap 'rm -f \"$tmp\"' EXIT HUP INT TERM; cat >\"$tmp\"; bytes=$(wc -c <\"$tmp\" | tr -d '[:space:]'); [ \"$bytes\" = \"{expected_bytes}\" ]; if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum \"$tmp\" | awk '{{print $1}}'); else actual=$(shasum -a 256 \"$tmp\" | awk '{{print $1}}'); fi; [ \"$actual\" = \"{expected_sha256}\" ]; chmod 755 \"$tmp\"; LC_ALL=C \"$tmp\" --version | grep -Fx \"kodade-cli {expected_version}\" >/dev/null; mv -f \"$tmp\" \"$dest\"; trap - EXIT"
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn process(command: &str) -> tokio::process::Child {
        Command::new("sh")
            .args(["-c", command])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn upload_deadline_includes_a_peer_that_never_reads() {
        let child = process("exec sleep 30");
        let start = std::time::Instant::now();
        let error = upload_to_child(child, &vec![0; 2 * 1024 * 1024], Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn upload_drains_stderr_before_the_peer_reads_stdin() {
        let child = process("dd if=/dev/zero bs=65536 count=8 1>&2 2>/dev/null; cat >/dev/null");
        upload_to_child(child, &vec![0; 2 * 1024 * 1024], Duration::from_secs(5))
            .await
            .unwrap();
    }
}
