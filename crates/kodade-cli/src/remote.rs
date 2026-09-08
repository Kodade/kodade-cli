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

static FORWARD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Every forward owns a short, private directory, even for the same host/session.
struct ForwardPath(PathBuf);

impl ForwardPath {
    fn allocate(host: &str, session: &str) -> Result<Self> {
        std::fs::create_dir_all(runtime_dir()).context("create SSH runtime directory")?;
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

fn validate_host(host: &str) -> Result<()> {
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
        remote_word(session),
    ]);
    args
}

/// `ssh -f <control-opts> HOST kodade-cli daemon NAME` — starts the remote
/// daemon detached when one is not already running.
pub fn start_daemon_args(control_path: &str, host: &str, session: &str) -> Vec<String> {
    let mut args = control_opts(control_path);
    args.push(host.to_string());
    args.push(format!(
        "nohup kodade-cli daemon {} </dev/null >/dev/null 2>&1 &",
        remote_word(session)
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
    args.push("kodade-cli".into());
    args.extend(remote_args.iter().map(|arg| remote_word(arg)));
    args
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
async fn connect(host: &str, session: &str) -> Result<(PathBuf, Tunnel)> {
    validate_host(host)?;
    crate::cli::session_name(session).map_err(anyhow::Error::msg)?;
    std::fs::create_dir_all(runtime_dir()).context("create SSH runtime directory")?;
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
    std::fs::create_dir_all(runtime_dir()).context("create SSH runtime directory")?;
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

/// Diagnose a remote endpoint without installing or starting its daemon.
pub async fn run_doctor(host: &str, session: &str, json: bool) -> Result<()> {
    validate_host(host)?;
    std::fs::create_dir_all(runtime_dir()).context("create SSH runtime directory")?;
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
        assert!(rendered.ends_with("'work; printf injected'"), "{rendered}");
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
        assert!(start
            .last()
            .unwrap()
            .contains("nohup kodade-cli daemon work"));
        assert!(start.last().unwrap().ends_with("2>&1 &"));

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
