//! Authenticated loopback plus SSH stdio connects Windows clients to Unix daemons.
use crate::cli;
use crate::remote_prepare;
use anyhow::{bail, Context, Result};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
};
const TIMEOUT: Duration = Duration::from_secs(10);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub struct Tunnel {
    task: tokio::task::JoinHandle<()>,
    record: PathBuf,
}
impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.record);
    }
}
pub(crate) fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("invalid SSH target: use an SSH host alias or USER@HOST");
    }
    Ok(())
}
fn word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_./:@%+-".contains(&c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
fn remote(args: &[&str]) -> String {
    format!("if [ -x \"$HOME/.local/bin/kodade-cli\" ]; then exec \"$HOME/.local/bin/kodade-cli\" {}; else exec kodade-cli {}; fi", args.iter().map(|v| word(v)).collect::<Vec<_>>().join(" "), args.iter().map(|v| word(v)).collect::<Vec<_>>().join(" "))
}
fn ssh_args(host: &str, command: &str) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        host.into(),
        command.into(),
    ]
}
async fn ssh_output(host: &str, command: &str) -> Result<std::process::Output> {
    tokio::time::timeout(
        TIMEOUT,
        Command::new("ssh")
            .args(ssh_args(host, command))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("SSH command timed out")?
    .context("run ssh")
}
fn probe(host: &str) -> Vec<String> {
    ssh_args(host, "uname -s; uname -m")
}
fn upload(host: &str, bytes: usize, checksum: &str, version: &str) -> Vec<String> {
    ssh_args(
        host,
        &remote_prepare::upload_command(bytes, checksum, version),
    )
}
pub async fn prepare_machine(host: &str, install: bool) -> Result<()> {
    validate_host(host)?;
    remote_prepare::prepare_machine(
        host,
        install,
        probe(host),
        ssh_args(host, &remote(&["--version"])),
        |bytes, checksum, version| upload(host, bytes, checksum, version),
        crate::update::fetch,
    )
    .await
}
pub async fn connect_endpoint(host: &str, session: &str) -> Result<(PathBuf, Tunnel)> {
    validate_host(host)?;
    crate::cli::session_name(session).map_err(anyhow::Error::msg)?;
    let probe = ssh_output(host, &remote(&["--version"])).await?;
    if !probe.status.success() {
        bail!(
            "could not run kodade-cli on {host}: {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        );
    }
    if !remote_prepare::version_is_compatible(&probe.stdout) {
        bail!("{host} has an incompatible kodade-cli; run `kodade-cli machine prepare {host} --install`");
    }
    let record = kodade_cli_daemon::socket_dir().join(format!(
        "bridge-{}-{}.sock",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let listener = kodade_cli_daemon::transport::bind(&record).await?;
    let task_host = host.to_owned();
    let task_session = session.to_owned();
    let cleanup = record.clone();
    let task = tokio::spawn(async move {
        relay(listener, task_host, task_session, cleanup).await;
    });
    Ok((record.clone(), Tunnel { task, record }))
}
async fn start_bridge(
    host: &str,
    session: &str,
) -> Result<(
    Child,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
)> {
    let mut child = Command::new("ssh")
        .args(ssh_args(host, &remote(&["bridge", "-s", session])))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start SSH stdio bridge")?;
    let stdin = child.stdin.take().context("SSH bridge stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("SSH bridge stdout unavailable")?;
    Ok((child, stdin, stdout))
}

async fn relay_connection(
    stream: kodade_cli_daemon::transport::Stream,
    mut child: Child,
    mut input: tokio::process::ChildStdin,
    mut output: tokio::process::ChildStdout,
) {
    let (mut read, mut write) = stream.into_split();
    tokio::select! {
        _ = tokio::io::copy(&mut read, &mut input) => {},
        _ = tokio::io::copy(&mut output, &mut write) => {},
        _ = child.wait() => {},
    }
    let _ = write.shutdown().await;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn relay(
    listener: kodade_cli_daemon::transport::Listener,
    host: String,
    session: String,
    record: PathBuf,
) {
    let mut relays = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let host = host.clone();
                    let session = session.clone();
                    relays.spawn(async move {
                        if let Ok((child, input, output)) = start_bridge(&host, &session).await {
                            relay_connection(stream, child, input, output).await;
                        }
                    });
                }
                Err(_) => break,
            },
            Some(_) = relays.join_next(), if !relays.is_empty() => {},
        }
    }
    relays.abort_all();
    while relays.join_next().await.is_some() {}
    let _ = std::fs::remove_file(record);
}

pub async fn resolve_socket(cli: &cli::Cli) -> Result<(PathBuf, Option<Tunnel>)> {
    match cli.remote.as_deref() {
        Some(host) => {
            let (path, tunnel) = connect_endpoint(host, &cli.session).await?;
            Ok((path, Some(tunnel)))
        }
        None => Ok((
            cli.socket
                .clone()
                .unwrap_or_else(|| kodade_cli_daemon::socket_path(&cli.session)),
            None,
        )),
    }
}
pub async fn run_session(host: &str, session: &str, command: &cli::SessionCommand) -> Result<()> {
    validate_host(host)?;
    let args = match command {
        cli::SessionCommand::Path => vec!["session", "path", "-s", session],
        cli::SessionCommand::Ls { json } => {
            if *json {
                vec!["session", "ls", "--json"]
            } else {
                vec!["session", "ls"]
            }
        }
        cli::SessionCommand::Kill { name } => {
            let mut a = vec!["session", "kill"];
            if let Some(n) = name {
                a.push(n)
            };
            a.extend(["-s", session]);
            a
        }
        cli::SessionCommand::Rename { name } => vec!["session", "rename", name, "-s", session],
    };
    let out = ssh_output(host, &remote(&args)).await?;
    if !out.status.success() {
        bail!(
            "remote session command failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    };
    print!("{}", String::from_utf8_lossy(&out.stdout));
    Ok(())
}
pub async fn run_doctor(host: &str, session: &str, json: bool) -> Result<()> {
    let args = if json {
        vec!["doctor", "-s", session, "--json"]
    } else {
        vec!["doctor", "-s", session]
    };
    let out = ssh_output(host, &remote(&args)).await?;
    if !out.status.success() {
        bail!(
            "remote diagnostics failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    };
    print!("{}", String::from_utf8_lossy(&out.stdout));
    Ok(())
}
