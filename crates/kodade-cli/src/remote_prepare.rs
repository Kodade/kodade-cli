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
    let mut child = Command::new("ssh")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start remote install")?;
    use tokio::io::AsyncWriteExt;
    let mut stdin = child
        .stdin
        .take()
        .context("remote install stdin unavailable")?;
    stdin
        .write_all(binary)
        .await
        .context("upload verified remote binary")?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .context("remote install timed out")?
        .context("wait for remote install")?;
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
