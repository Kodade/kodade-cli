//! Remote mode: attach to a Ködade CLI daemon on another host over an
//! SSH-forwarded Unix socket (#23).
//!
//! No credential handling and no new dependencies: everything runs through the
//! user's `ssh` (their config, agent, and keys). A multiplexed control master
//! keeps the extra `ssh` round-trips cheap, and OpenSSH's Unix-to-Unix `-L`
//! forwarding bridges the remote socket to a local one that the rest of the CLI
//! treats exactly like a local daemon.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use sha2::Digest;
use tokio::{net::UnixStream, process::Command};

use crate::cli;
use crate::update;

/// Install one-liner shown when the remote host has no `kodade-cli` on its PATH.
/// Mirrors README's install section.
const INSTALL_HINT: &str =
    "curl -fsSL https://raw.githubusercontent.com/Kodade/kodade-cli/main/install.sh | sh";

/// How long to wait for the forwarded local socket to accept a connection.
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_INSTALL: &str = "\"$HOME/.local/bin/kodade-cli\"";

/// A live SSH forward. Dropping it stops forwarding and removes the local
/// socket file; the control master lingers (`ControlPersist`) so a reconnect is
/// cheap.
pub struct Tunnel {
    child: tokio::process::Child,
    endpoint: ForwardPath,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // End forwarding and clear the stale socket file. The persisted control
        // master is left for `ControlPersist` to reap so re-running `--remote`
        // reconnects without a fresh handshake.
        let _ = self.child.start_kill();
        // ForwardPath owns only this instance's exclusively allocated directory.
        let _ = &self.endpoint;
    }
}

/// Resolve the socket the client should connect to. Local sessions return the
/// daemon socket directly; `--remote` sets up (or reuses) an SSH forward and
/// returns the local end of it. Every socket-using code path funnels through
/// here so `--remote` applies uniformly (#23).
pub async fn resolve_socket(cli: &cli::Cli) -> Result<(PathBuf, Option<Tunnel>)> {
    match cli.remote.as_deref() {
        None => Ok((
            cli.socket
                .clone()
                .unwrap_or_else(|| kodade_cli_daemon::socket_path(&cli.session)),
            None,
        )),
        Some(host) => {
            let (socket, tunnel) = connect_endpoint(host, &cli.session).await?;
            Ok((socket, Some(tunnel)))
        }
    }
}

/// Directory Ködade CLI keeps its sockets and control paths in (the parent of
/// the local session socket).
fn runtime_dir() -> PathBuf {
    let normal = kodade_cli_daemon::socket_path("default")
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ssh");
    // OpenSSH expands %C to 40 bytes and appends a temporary suffix. Leave
    // room under both macOS's 104-byte and Linux's 108-byte sockaddr limits.
    if normal.as_os_str().len() <= 35 {
        normal
    } else {
        PathBuf::from(format!("/tmp/kodade-ssh-{}", unsafe { libc::geteuid() }))
    }
}

fn ensure_runtime_dir() -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let path = runtime_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)?;
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "SSH runtime directory {} must be owned by you with mode 700",
            path.display()
        );
    }
    Ok(())
}

static FORWARD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Every forward owns a short, private directory, even for the same host/session.
struct ForwardPath(PathBuf);

impl ForwardPath {
    fn allocate(host: &str, session: &str) -> Result<Self> {
        ensure_runtime_dir()?;
        for _ in 0..100 {
            let path = local_socket_path(host, session);
            let directory = path.parent().expect("socket directory");
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(directory) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("reserve SSH forwarding directory"),
            }
        }
        bail!("could not reserve an SSH forwarding socket")
    }
}

impl Drop for ForwardPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

fn local_socket_path(_host: &str, _session: &str) -> PathBuf {
    let sequence = FORWARD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    runtime_dir()
        .join(format!("remote-{}-{sequence}", std::process::id()))
        .join("s.sock")
}

/// OpenSSH joins remote argv into a shell command; quote nonliteral words.
fn remote_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"_./:@%+-".contains(&ch))
    {
        word.into()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

pub(crate) fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        bail!("invalid SSH target: use an SSH host alias or USER@HOST");
    }
    Ok(())
}

/// The SSH control-master socket template (`%C` expands to a per-connection
/// hash). Shared by every `ssh` call so they multiplex over one connection.
fn control_path() -> PathBuf {
    runtime_dir().join("cm-%C")
}

/// The shared control-master options every `ssh` invocation carries.
fn control_opts(control_path: &str) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={control_path}"),
        "-o".into(),
        "ControlPersist=60".into(),
    ]
}

/// `ssh <control-opts> HOST kodade-cli --version` — probes for the remote binary.
pub fn version_args(control_path: &str, host: &str) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.to_string());
    args.push(remote_binary_command(&["--version"]));
    args
}

fn remote_binary_command(args: &[&str]) -> String {
    let args = args
        .iter()
        .map(|arg| remote_word(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!("if [ -x {REMOTE_INSTALL} ]; then exec {REMOTE_INSTALL} {args}; else exec kodade-cli {args}; fi")
}

/// `ssh <control-opts> HOST kodade-cli session path -s NAME` — asks for the
/// remote daemon's socket path.
pub fn socket_path_args(control_path: &str, host: &str, session: &str) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.to_string());
    args.push(remote_binary_command(&["session", "path", "-s", session]));
    args
}

/// `ssh -f <control-opts> HOST kodade-cli daemon NAME` — starts the remote
/// daemon detached when one is not already running.
pub fn start_daemon_args(control_path: &str, host: &str, session: &str) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.to_string());
    args.push(format!(
        "nohup sh -c {} </dev/null >/dev/null 2>&1 &",
        remote_word(&remote_binary_command(&["daemon", session])),
    ));
    args
}

/// `ssh -N -L LOCAL:REMOTE <control-opts> HOST` — forwards the remote socket to
/// a local one (OpenSSH Unix-to-Unix forwarding).
pub fn tunnel_args(control_path: &str, host: &str, local: &str, remote: &str) -> Vec<String> {
    let mut args = vec![
        "-N".to_string(),
        "-L".to_string(),
        format!("{local}:{remote}"),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
    ];
    args.extend(control_opts(control_path));
    args.push(host.to_string());
    args
}

/// `ssh <control-opts> HOST kodade-cli <remote-args...>` — run a remote CLI
/// command over the control connection (used by remote `session` verbs).
pub fn run_args(control_path: &str, host: &str, remote_args: &[&str]) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.to_string());
    args.push(remote_binary_command(remote_args));
    args
}

fn probe_args(control_path: &str, host: &str) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.into());
    args.push("uname -s; uname -m; if [ -x \"$HOME/.local/bin/kodade-cli\" ]; then printf '%s\\n' \"$HOME/.local/bin/kodade-cli\"; else command -v kodade-cli || true; fi".into());
    args
}

fn upload_args(
    control_path: &str,
    host: &str,
    expected_bytes: usize,
    expected_sha256: &str,
    expected_version: &str,
) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.into());
    // Values originate from a verified local artifact / package metadata, and
    // are constrained to digits or a semantic version before interpolation.
    args.push(format!(
        "set -eu; umask 077; dest=\"$HOME/.local/bin/kodade-cli\"; dir=\"${{dest%/*}}\"; mkdir -p \"$dir\"; tmp=$(mktemp \"$dir/.kodade-cli.XXXXXX\"); trap 'rm -f \"$tmp\"' EXIT HUP INT TERM; cat >\"$tmp\"; bytes=$(wc -c <\"$tmp\" | tr -d '[:space:]'); [ \"$bytes\" = \"{expected_bytes}\" ]; if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum \"$tmp\" | awk '{{print $1}}'); else actual=$(shasum -a 256 \"$tmp\" | awk '{{print $1}}'); fi; [ \"$actual\" = \"{expected_sha256}\" ]; chmod 755 \"$tmp\"; LC_ALL=C \"$tmp\" --version | grep -Fx \"kodade-cli {expected_version}\" >/dev/null; mv -f \"$tmp\" \"$dest\"; trap - EXIT"
    ));
    args
}

/// Explicitly prepare a remote Unix host. No reconnect path calls this: it
/// only runs from `machine add --install` or `machine prepare --install`.
pub async fn prepare_machine(host: &str, install: bool) -> Result<()> {
    prepare_machine_with_fetch(host, install, update::fetch).await
}

/// Keeps release retrieval replaceable only inside this module's tests. Every
/// caller still flows through release metadata selection, SHA256 validation,
/// archive extraction, and remote staged verification.
async fn prepare_machine_with_fetch<F>(host: &str, install: bool, fetch: F) -> Result<()>
where
    F: Fn(&str) -> Result<Vec<u8>>,
{
    validate_host(host)?;
    ensure_runtime_dir()?;
    let control = control_path().to_string_lossy().into_owned();
    let probe = ssh_output(&probe_args(&control, host)).await?;
    if !probe.status.success() {
        bail!(
            "could not probe {host}: {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        );
    }
    let lines = String::from_utf8_lossy(&probe.stdout);
    let mut fields = lines.lines();
    let os = fields
        .next()
        .context("remote probe did not report an operating system")?;
    let arch = fields
        .next()
        .context("remote probe did not report an architecture")?;
    let version = ssh_output(&version_args(&control, host)).await?;
    if version.status.success() && remote_version_is_compatible(&version.stdout) {
        return Ok(());
    }
    if !install {
        bail!("{host} has no compatible kodade-cli; run `kodade-cli machine prepare {host} --install` (or add with --install)");
    }
    let (binary, release_version) = verified_release_binary(os, arch, fetch)?;
    let expected_sha256 = format!("{:x}", sha2::Sha256::digest(&binary));
    tokio::time::timeout(
        Duration::from_secs(45),
        upload_binary(&control, host, &binary, &expected_sha256, &release_version),
    )
    .await
    .context("remote install timed out")??;
    let installed = ssh_output(&version_args(&control, host)).await?;
    if !installed.status.success() || !remote_version_is_compatible(&installed.stdout) {
        bail!("remote install on {host} did not produce a compatible kodade-cli");
    }
    Ok(())
}

fn verified_release_binary<F>(os: &str, arch: &str, fetch: F) -> Result<(Vec<u8>, String)>
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
    let archive_url = update::release_asset_url(&release, &asset)
        .with_context(|| format!("published release is missing {asset}"))?;
    let archive = fetch(archive_url)?;
    Ok((
        update::verified_binary(&archive, &update::checksum(&sums, &asset)?)?,
        version,
    ))
}

async fn upload_binary(
    control: &str,
    host: &str,
    binary: &[u8],
    expected_sha256: &str,
    expected_version: &str,
) -> Result<()> {
    let mut child = Command::new("ssh")
        .args(upload_args(
            control,
            host,
            binary.len(),
            expected_sha256,
            expected_version,
        ))
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
    let output = child
        .wait_with_output()
        .await
        .context("wait for remote install")?;
    if !output.status.success() {
        bail!(
            "remote install failed on {host}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn remote_version_is_compatible(stdout: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return false;
    };
    let Some(remote) = text.trim().strip_prefix("kodade-cli ") else {
        return false;
    };
    let Ok(remote) = semver::Version::parse(remote) else {
        return false;
    };
    let local =
        semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("package version is semver");
    remote == local
}

fn os_to_target(value: &str) -> Result<&'static str> {
    match value {
        "Linux" => Ok("linux"),
        "Darwin" => Ok("macos"),
        _ => bail!("remote OS {value:?} has no standalone preparation artifact"),
    }
}
fn arch_to_target(value: &str) -> Result<&'static str> {
    match value {
        "x86_64" | "amd64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        _ => bail!("remote architecture {value:?} has no standalone preparation artifact"),
    }
}

/// Run an `ssh` invocation, returning its captured stdout on success.
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

/// Set up (or reuse) the SSH forward for `host`/`session` and return the local
/// socket plus the tunnel guard.
pub async fn connect_endpoint(host: &str, session: &str) -> Result<(PathBuf, Tunnel)> {
    validate_host(host)?;
    crate::cli::session_name(session).map_err(anyhow::Error::msg)?;
    ensure_runtime_dir()?;
    let control = control_path();
    let control = control.to_string_lossy().to_string();

    // (a) Verify the remote binary exists before anything else.
    let probe = ssh_output(&version_args(&control, host)).await?;
    if !probe.status.success() {
        bail!(
            "could not run kodade-cli on {host}: {}\nIf it is missing, install it there with:\n    {INSTALL_HINT}",
            String::from_utf8_lossy(&probe.stderr).trim()
        );
    }

    // Start the remote daemon if needed. It is a no-op / harmless error when one
    // already owns the session, so ignore the exit status.
    let _ = ssh_output(&start_daemon_args(&control, host, session)).await;

    // (b) Ask the remote for its socket path.
    let path_out = ssh_output(&socket_path_args(&control, host, session)).await?;
    if !path_out.status.success() {
        bail!("could not read the remote socket path from {host}");
    }
    let remote_socket = String::from_utf8_lossy(&path_out.stdout).trim().to_string();
    if !remote_socket.starts_with('/') || remote_socket.contains(['\n', '\r', ':']) {
        bail!("{host} returned an invalid Unix socket path");
    }

    // (c) Forward the remote socket to a fresh local one.
    let endpoint = ForwardPath::allocate(host, session)?;
    let local_socket = endpoint.0.clone();
    let child = Command::new("ssh")
        .args(tunnel_args(
            &control,
            host,
            &local_socket.to_string_lossy(),
            &remote_socket,
        ))
        .stdin(Stdio::null())
        .spawn()
        .context("start ssh tunnel")?;
    let mut tunnel = Tunnel { child, endpoint };

    // (d) Wait for the forwarded socket to accept a connection.
    let deadline = Instant::now() + TUNNEL_TIMEOUT;
    loop {
        if UnixStream::connect(&local_socket).await.is_ok() {
            return Ok((local_socket, tunnel));
        }
        if let Some(status) = tunnel.child.try_wait()? {
            bail!("SSH tunnel to {host} exited with {status}");
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for the SSH tunnel to {host} (socket {})",
                local_socket.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Handle `session` subcommands under `--remote`: run them on the host over the
/// control connection. `session ls` output is prefixed with `host:` so it is
/// clear which machine each session belongs to (#23, item 3).
pub async fn run_session(host: &str, session: &str, command: &cli::SessionCommand) -> Result<()> {
    validate_host(host)?;
    crate::cli::session_name(session).map_err(anyhow::Error::msg)?;
    ensure_runtime_dir()?;
    let control = control_path();
    let control = control.to_string_lossy().to_string();
    let (remote_args, prefix): (Vec<&str>, bool) = match command {
        // `-s NAME` keeps the remote path scoped to the requested session.
        cli::SessionCommand::Path => (vec!["session", "path", "-s", session], false),
        // `ls` describes the host's sessions, so prefix each line with it.
        cli::SessionCommand::Ls { json } => {
            let mut args = vec!["session", "ls"];
            if *json {
                args.push("--json");
            }
            (args, !json)
        }
        cli::SessionCommand::Kill { name } => {
            let mut args = vec!["session", "kill"];
            if let Some(name) = name {
                args.push(name);
            }
            args.extend(["-s", session]);
            (args, false)
        }
        cli::SessionCommand::Rename { name } => {
            (vec!["session", "rename", name, "-s", session], false)
        }
        cli::SessionCommand::Upgrade { binary } => {
            if binary.is_some() {
                bail!("--binary is only supported for a local daemon upgrade");
            }
            (vec!["session", "upgrade", "-s", session], false)
        }
    };
    let output = ssh_output(&run_args(&control, host, &remote_args)).await?;
    if !output.status.success() {
        bail!(
            "remote `session` command on {host} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if prefix {
            println!("{host}:{line}");
        } else {
            println!("{line}");
        }
    }
    Ok(())
}

/// Diagnose a remote endpoint without installing or starting its daemon.
pub async fn run_doctor(host: &str, session: &str, json: bool) -> Result<()> {
    validate_host(host)?;
    ensure_runtime_dir()?;
    let control = control_path().to_string_lossy().into_owned();
    let mut args = vec!["doctor", "-s", session];
    if json {
        args.push("--json");
    }
    let output = ssh_output(&run_args(&control, host, &args)).await?;
    use std::io::Write;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if !output.status.success() {
        bail!("remote diagnostics failed on {host}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_tunnels_never_share_a_local_socket() {
        assert_ne!(
            local_socket_path("user@host", "work"),
            local_socket_path("user@host", "work")
        );
    }

    #[test]
    fn remote_shell_preserves_session_arguments_literally() {
        let session = "work; printf injected";
        let args = socket_path_args("/tmp/cm-%C", "host", session);
        let remote = &args[args.iter().position(|arg| arg == "host").unwrap() + 1..];
        let rendered = remote.join(" ");
        assert!(rendered.contains("'work; printf injected'"), "{rendered}");
    }

    #[test]
    fn command_builders_carry_control_options_and_targets() {
        let cp = "/run/kodade-cli/cm-%C";
        let host = "user@host";

        let version = version_args(cp, host);
        assert_eq!(
            version,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/run/kodade-cli/cm-%C",
                "-o",
                "ControlPersist=60",
                "user@host",
                "if [ -x \"$HOME/.local/bin/kodade-cli\" ]; then exec \"$HOME/.local/bin/kodade-cli\" --version; else exec kodade-cli --version; fi",
            ]
        );

        assert_eq!(
            socket_path_args(cp, host, "work")
                .last()
                .map(String::as_str),
            Some("if [ -x \"$HOME/.local/bin/kodade-cli\" ]; then exec \"$HOME/.local/bin/kodade-cli\" session path -s work; else exec kodade-cli session path -s work; fi")
        );
        assert!(socket_path_args(cp, host, "work")
            .last()
            .unwrap()
            .contains("session path"));

        // The daemon starter forwards detached (`-f`) and names the session.
        let start = start_daemon_args(cp, host, "work");
        assert!(start.last().unwrap().contains("nohup sh -c"));
        assert!(start.last().unwrap().ends_with("2>&1 &"));

        // The forward is Unix-to-Unix: `-N -L local:remote`.
        let tunnel = tunnel_args(cp, host, "/tmp/local.sock", "/run/remote.sock");
        assert_eq!(tunnel[0], "-N");
        assert_eq!(tunnel[1], "-L");
        assert_eq!(tunnel[2], "/tmp/local.sock:/run/remote.sock");
        assert_eq!(tunnel.last().map(String::as_str), Some("user@host"));

        let run = run_args(cp, host, &["session", "ls"]);
        assert!(run.last().unwrap().contains("kodade-cli session ls"));
    }

    #[test]
    fn remote_install_upload_is_staged_and_never_touches_system_paths() {
        let command = upload_args("/tmp/cm-%C", "buildbox", 42, &"a".repeat(64), "0.2.1")
            .last()
            .unwrap()
            .clone();
        assert!(command.contains("dest=\"$HOME/.local/bin/kodade-cli\""));
        assert!(command.contains("mktemp \"$dir/.kodade-cli.XXXXXX\""));
        assert!(command.contains("[ \"$bytes\" = \"42\" ]"));
        assert!(command.contains("[ \"$actual\" = \""));
        assert!(command.contains("chmod 755 \"$tmp\""));
        assert!(command.contains("mv -f \"$tmp\" \"$dest\""));
        assert!(command.contains("--version | grep -Fx \"kodade-cli 0.2.1\""));
        assert!(!command.contains("/usr/bin"));
    }

    #[test]
    fn remote_platform_probe_accepts_released_unix_names() {
        assert_eq!(os_to_target("Linux").unwrap(), "linux");
        assert_eq!(arch_to_target("arm64").unwrap(), "aarch64");
        assert!(os_to_target("FreeBSD").is_err());
        assert!(arch_to_target("riscv64").is_err());
    }

    #[test]
    fn remote_version_requires_exact_local_cli_version() {
        let local = env!("CARGO_PKG_VERSION");
        assert!(remote_version_is_compatible(
            format!("kodade-cli {local}\n").as_bytes()
        ));
        assert!(!remote_version_is_compatible(b"foreign-tool 0.2.1\n"));
        assert!(!remote_version_is_compatible(
            format!("kodade-cli {local}\nextra output\n").as_bytes()
        ));
        assert!(!remote_version_is_compatible(b"kodade-cli unparseable\n"));
    }

    #[test]
    fn fixture_release_version_mismatch_stops_before_artifact_fetch() {
        use std::cell::Cell;

        let other = if env!("CARGO_PKG_VERSION") == "0.0.0" {
            "0.0.1"
        } else {
            "0.0.0"
        };
        let calls = Cell::new(0);
        let error = verified_release_binary("Linux", "x86_64", |url| {
            calls.set(calls.get() + 1);
            assert_eq!(url, update::metadata_url("stable"));
            Ok(format!(r#"{{"tag_name":"v{other}"}}"#).into_bytes())
        })
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match local kodade-cli"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn fixture_download_rejects_a_bad_checksum_before_remote_upload() {
        let metadata = br#"{"tag_name":"v0.2.1","assets":[{"name":"SHA256SUMS","browser_download_url":"sums"},{"name":"kodade-cli-0.2.1-x86_64-unknown-linux-gnu.tar.gz","browser_download_url":"archive"}]}"#;
        let error = verified_release_binary("Linux", "x86_64", |url| match url {
            "https://api.github.com/repos/Kodade/kodade-cli/releases/latest" => {
                Ok(metadata.to_vec())
            }
            "sums" => Ok(format!(
                "{}  kodade-cli-0.2.1-x86_64-unknown-linux-gnu.tar.gz\n",
                "0".repeat(64)
            )
            .into_bytes()),
            "archive" => Ok(b"fixture archive".to_vec()),
            _ => bail!("unexpected fixture URL {url}"),
        })
        .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn local_socket_path_is_host_and_session_scoped() {
        let a = local_socket_path("user@host", "work");
        let b = local_socket_path("user@host", "other");
        let c = local_socket_path("root@box:22", "work");
        assert_ne!(a, b);
        assert_ne!(a, c);
        // The filesystem-hostile characters are sanitized out of the file name.
        let name = c.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains('@'));
        assert!(!name.contains(':'));
        assert_eq!(name, "s.sock");
        assert!(c
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("remote-"));
    }
}
