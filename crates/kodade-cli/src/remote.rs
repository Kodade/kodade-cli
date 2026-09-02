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
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use tokio::{net::UnixStream, process::Command};

use crate::cli;

/// Install one-liner shown when the remote host has no `kodade-cli` on its PATH.
/// Mirrors README's install section.
const INSTALL_HINT: &str =
    "curl -fsSL https://raw.githubusercontent.com/Kodade/kodade-cli/main/install.sh | sh";

/// How long to wait for the forwarded local socket to accept a connection.
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(10);

/// A live SSH forward. Dropping it stops forwarding and removes the local
/// socket file; the control master lingers (`ControlPersist`) so a reconnect is
/// cheap.
pub struct Tunnel {
    child: tokio::process::Child,
    local_socket: PathBuf,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // End forwarding and clear the stale socket file. The persisted control
        // master is left for `ControlPersist` to reap so re-running `--remote`
        // reconnects without a fresh handshake.
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.local_socket);
    }
}

/// Resolve the socket the client should connect to. Local sessions return the
/// daemon socket directly; `--remote` sets up (or reuses) an SSH forward and
/// returns the local end of it. Every socket-using code path funnels through
/// here so `--remote` applies uniformly (#23).
pub async fn resolve_socket(cli: &cli::Cli) -> Result<(PathBuf, Option<Tunnel>)> {
    match cli.remote.as_deref() {
        None => Ok((kodade_cli_daemon::socket_path(&cli.session), None)),
        Some(host) => {
            let (socket, tunnel) = connect(host, &cli.session).await?;
            Ok((socket, Some(tunnel)))
        }
    }
}

/// Directory Ködade CLI keeps its sockets and control paths in (the parent of
/// the local session socket).
fn runtime_dir() -> PathBuf {
    kodade_cli_daemon::socket_path("default")
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// A filesystem-safe token for a `user@host` string.
fn host_token(host: &str) -> String {
    host.replace(['@', '/', ':', ' '], "-")
}

/// Path of the forwarded local socket for a given host/session.
fn local_socket_path(host: &str, session: &str) -> PathBuf {
    runtime_dir().join(format!("remote-{}-{session}.sock", host_token(host)))
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
    args.extend(["kodade-cli".into(), "--version".into()]);
    args
}

/// `ssh <control-opts> HOST kodade-cli session path -s NAME` — asks for the
/// remote daemon's socket path.
pub fn socket_path_args(control_path: &str, host: &str, session: &str) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.to_string());
    args.extend([
        "kodade-cli".into(),
        "session".into(),
        "path".into(),
        "-s".into(),
        session.to_string(),
    ]);
    args
}

/// `ssh -f <control-opts> HOST kodade-cli daemon NAME` — starts the remote
/// daemon detached when one is not already running.
pub fn start_daemon_args(control_path: &str, host: &str, session: &str) -> Vec<String> {
    let mut args = vec!["-f".to_string()];
    args.extend(control_opts(control_path));
    args.push(host.to_string());
    args.extend(["kodade-cli".into(), "daemon".into(), session.to_string()]);
    args
}

/// `ssh -N -L LOCAL:REMOTE <control-opts> HOST` — forwards the remote socket to
/// a local one (OpenSSH Unix-to-Unix forwarding).
pub fn tunnel_args(control_path: &str, host: &str, local: &str, remote: &str) -> Vec<String> {
    let mut args = vec![
        "-N".to_string(),
        "-L".to_string(),
        format!("{local}:{remote}"),
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
    args.push("kodade-cli".into());
    args.extend(remote_args.iter().map(|arg| arg.to_string()));
    args
}

/// Run an `ssh` invocation, returning its captured stdout on success.
async fn ssh_output(args: &[String]) -> Result<std::process::Output> {
    Command::new("ssh")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("run ssh")
}

/// Set up (or reuse) the SSH forward for `host`/`session` and return the local
/// socket plus the tunnel guard.
async fn connect(host: &str, session: &str) -> Result<(PathBuf, Tunnel)> {
    let control = control_path();
    let control = control.to_string_lossy().to_string();

    // (a) Verify the remote binary exists before anything else.
    let probe = ssh_output(&version_args(&control, host)).await?;
    if !probe.status.success() {
        bail!(
            "kodade-cli not found on {host}. Install it there with:\n    {INSTALL_HINT}\n(phase 2 will auto-install; for now install manually)"
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
    if remote_socket.is_empty() {
        bail!("{host} returned an empty socket path");
    }

    // (c) Forward the remote socket to a fresh local one.
    let local_socket = local_socket_path(host, session);
    let _ = std::fs::remove_file(&local_socket);
    if let Some(parent) = local_socket.parent() {
        std::fs::create_dir_all(parent).context("create local socket directory")?;
    }
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
    let tunnel = Tunnel {
        child,
        local_socket: local_socket.clone(),
    };

    // (d) Wait for the forwarded socket to accept a connection.
    let deadline = Instant::now() + TUNNEL_TIMEOUT;
    loop {
        if UnixStream::connect(&local_socket).await.is_ok() {
            return Ok((local_socket, tunnel));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builders_carry_control_options_and_targets() {
        let cp = "/run/kodade-cli/cm-%C";
        let host = "user@host";

        let version = version_args(cp, host);
        assert_eq!(
            version,
            vec![
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/run/kodade-cli/cm-%C",
                "-o",
                "ControlPersist=60",
                "user@host",
                "kodade-cli",
                "--version",
            ]
        );

        assert_eq!(
            socket_path_args(cp, host, "work")
                .last()
                .map(String::as_str),
            Some("work")
        );
        assert!(socket_path_args(cp, host, "work")
            .windows(2)
            .any(|w| w == ["session", "path"]));

        // The daemon starter forwards detached (`-f`) and names the session.
        let start = start_daemon_args(cp, host, "work");
        assert_eq!(start.first().map(String::as_str), Some("-f"));
        assert!(start.windows(2).any(|w| w == ["daemon", "work"]));

        // The forward is Unix-to-Unix: `-N -L local:remote`.
        let tunnel = tunnel_args(cp, host, "/tmp/local.sock", "/run/remote.sock");
        assert_eq!(tunnel[0], "-N");
        assert_eq!(tunnel[1], "-L");
        assert_eq!(tunnel[2], "/tmp/local.sock:/run/remote.sock");
        assert_eq!(tunnel.last().map(String::as_str), Some("user@host"));

        let run = run_args(cp, host, &["session", "ls"]);
        assert_eq!(&run[run.len() - 3..], ["kodade-cli", "session", "ls"]);
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
        assert!(name.starts_with("remote-"));
        assert!(name.ends_with("work.sock"));
    }
}
