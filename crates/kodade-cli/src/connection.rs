//! Session selection and bounded local daemon startup.

use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use tokio::time::Instant;

const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Inherited pane context applies only when no explicit endpoint was selected.
pub fn inherited_context(
    cli: &mut crate::cli::Cli,
    explicit_session: bool,
    session: Option<String>,
    socket: Option<PathBuf>,
) -> Result<()> {
    if explicit_session || cli.remote.is_some() || cli.socket.is_some() {
        return Ok(());
    }
    if let Some(session) = session {
        crate::cli::session_name(&session).map_err(anyhow::Error::msg)?;
        cli.session = session;
    }
    cli.socket = socket;
    Ok(())
}

pub fn daemon_log(socket: &Path) -> PathBuf {
    socket.with_extension("log")
}

/// Connect to an existing daemon, optionally starting one on this local path.
pub async fn connect(
    socket: &Path,
    session: &str,
    autostart: bool,
) -> Result<crate::transport::Stream> {
    let mut command = Command::new(std::env::current_exe().context("locate Ködade binary")?);
    command.args(["daemon", session]);
    connect_with_command(socket, autostart, command, START_TIMEOUT).await
}

async fn connect_with_command(
    socket: &Path,
    autostart: bool,
    mut command: Command,
    timeout: Duration,
) -> Result<crate::transport::Stream> {
    match crate::transport::connect(socket).await {
        Ok(stream) => return Ok(stream),
        Err(error)
            if autostart
                && matches!(
                    error
                        .downcast_ref::<std::io::Error>()
                        .map(std::io::Error::kind),
                    Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
                ) => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot connect to {}; run `kodade-cli doctor` for diagnostics",
                    socket.display()
                )
            });
        }
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).context("create daemon socket directory")?;
    }
    let log_path = daemon_log(socket);
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options.open(&log_path).context("open daemon startup log")?;
    let mut child = command
        .env_remove("KODADE_SOCKET")
        .env_remove("KODADE_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("start Ködade daemon")?;
    let deadline = Instant::now() + timeout;
    loop {
        // Concurrent starters may race; connecting to the winner is success.
        if let Ok(stream) = crate::transport::connect(socket).await {
            return Ok(stream);
        }
        if let Some(status) = child.try_wait().context("check daemon startup")? {
            bail!("daemon exited with {status}; see {}", log_path.display());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "daemon did not start within {}s; see {}",
                timeout.as_secs_f32(),
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn explicit_endpoint_overrides_inherited_pane_context() {
        for args in [
            vec!["kodade-cli", "-s", "other"],
            vec!["kodade-cli", "--remote", "buildbox"],
        ] {
            let explicit = args.contains(&"-s");
            let mut cli = crate::cli::Cli::parse_from(args);
            inherited_context(
                &mut cli,
                explicit,
                Some("caller".into()),
                Some("/tmp/caller.sock".into()),
            )
            .unwrap();
            assert_ne!(cli.session, "caller");
            assert!(cli.socket.is_none());
        }
        let mut cli = crate::cli::Cli::parse_from(["kodade-cli", "ls"]);
        inherited_context(
            &mut cli,
            false,
            Some("caller".into()),
            Some("/tmp/caller.sock".into()),
        )
        .unwrap();
        assert_eq!(cli.session, "caller");
        assert_eq!(cli.socket, Some(PathBuf::from("/tmp/caller.sock")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_daemon_start_returns_its_exit_status() {
        let path =
            std::env::temp_dir().join(format!("kodade-start-exit-{}.sock", std::process::id()));
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let error = connect_with_command(&path, true, command, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exit status: 7"), "{error}");
        std::fs::remove_file(daemon_log(&path)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn silent_daemon_start_is_bounded() {
        let path =
            std::env::temp_dir().join(format!("kodade-start-timeout-{}.sock", std::process::id()));
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        let error = connect_with_command(&path, true, command, Duration::from_millis(60))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not start"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_file(daemon_log(&path)).unwrap();
    }
}
