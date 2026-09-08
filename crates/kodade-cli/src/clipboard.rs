//! Bounded clipboard writes owned by the attached client.

use anyhow::{Context, Result};
use std::{io::Write, process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;

use crate::mode::{osc52, OSC52_LIMIT};

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn copy(text: String, remote: bool) -> Result<()> {
    let (text, _) = limit(&text);
    if use_native(remote) && native_copy(text).await.is_ok() {
        return Ok(());
    }
    let (sequence, _) = osc52(text);
    let mut stdout = std::io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn use_native(remote: bool) -> bool {
    !remote && std::env::var_os("SSH_CONNECTION").is_none() && std::env::var_os("SSH_TTY").is_none()
}

fn limit(text: &str) -> (&str, bool) {
    if text.len() <= OSC52_LIMIT {
        (text, false)
    } else {
        (&text[..text.floor_char_boundary(OSC52_LIMIT)], true)
    }
}

fn command() -> (&'static str, &'static [&'static str]) {
    if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if cfg!(windows) {
        ("clip.exe", &[])
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        ("wl-copy", &["--type", "text/plain;charset=utf-8"])
    } else {
        ("xclip", &["-selection", "clipboard", "-in"])
    }
}

async fn native_copy(text: &str) -> Result<()> {
    let (program, args) = command();
    run_native(program, args, text, CLIPBOARD_TIMEOUT).await
}

async fn run_native(program: &str, args: &[&str], text: &str, timeout: Duration) -> Result<()> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("native clipboard needs {program}"))?;
    let mut stdin = child.stdin.take().expect("piped clipboard stdin");
    tokio::time::timeout(timeout, async {
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin);
        let status = child.wait().await?;
        anyhow::ensure!(status.success(), "{program} failed");
        Ok(())
    })
    .await
    .context("native clipboard write timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_at_a_character_boundary() {
        let text = format!("{}é", "x".repeat(OSC52_LIMIT));
        let (limited, truncated) = limit(&text);
        assert!(truncated);
        assert_eq!(limited.len(), OSC52_LIMIT);
    }

    #[test]
    fn remote_copy_never_selects_a_native_program() {
        assert!(!use_native(true));
        // The process environment is deliberately not mutated here: the
        // remote endpoint decision is deterministic and SSH is checked above.
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_backend_writes_the_exact_stdin_payload() {
        let path = std::env::temp_dir().join(format!("kodade-clipboard-{}", std::process::id()));
        let script = format!("cat > {}", path.display());
        run_native(
            "sh",
            &["-c", &script],
            "copied text",
            Duration::from_secs(1),
        )
        .await
        .expect("clipboard command");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "copied text");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_backend_failure_and_timeout_return_errors() {
        assert!(
            run_native("sh", &["-c", "exit 1"], "x", Duration::from_secs(1))
                .await
                .is_err()
        );
        assert!(
            run_native("sh", &["-c", "sleep 1"], "x", Duration::from_millis(10))
                .await
                .is_err()
        );
    }
}
