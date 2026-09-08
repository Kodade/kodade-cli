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
    run_native(program, args, &native_payload(text), CLIPBOARD_TIMEOUT).await
}

fn native_payload(text: &str) -> Vec<u8> {
    if cfg!(windows) {
        // clip.exe consumes redirected input through the active console code
        // page. Its documented pipe interface therefore cannot promise that
        // UTF-8 survives unchanged. A BOM makes the UTF-16LE stream explicit.
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    } else {
        text.as_bytes().to_vec()
    }
}

async fn run_native(program: &str, args: &[&str], bytes: &[u8], timeout: Duration) -> Result<()> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A backend is allowed to fork. Give it its own group so a timeout
        // terminates the whole backend tree instead of orphaning a writer.
        unsafe {
            command.as_std_mut().pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("native clipboard needs {program}"))?;
    let mut stdin = child.stdin.take().expect("piped clipboard stdin");
    let result = tokio::time::timeout(timeout, async {
        stdin.write_all(bytes).await?;
        drop(stdin);
        let status = child.wait().await?;
        anyhow::ensure!(status.success(), "{program} failed");
        Ok(())
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // The child became its process-group leader in pre_exec.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
            anyhow::bail!("native clipboard write timed out")
        }
    }
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
            b"copied text",
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
            run_native("sh", &["-c", "exit 1"], b"x", Duration::from_secs(1))
                .await
                .is_err()
        );
        assert!(
            run_native("sh", &["-c", "sleep 1"], b"x", Duration::from_millis(10))
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_the_backend_process_group() {
        let path =
            std::env::temp_dir().join(format!("kodade-clipboard-child-{}", std::process::id()));
        let command = format!("sleep 30 & echo $! > {}; wait", path.display());
        assert!(
            run_native("sh", &["-c", &command], b"x", Duration::from_millis(100))
                .await
                .is_err()
        );
        let pid: i32 = std::fs::read_to_string(&path)
            .expect("backend recorded child pid")
            .trim()
            .parse()
            .expect("numeric child pid");
        let stopped = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if unsafe { libc::kill(pid, 0) } == -1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            stopped.is_ok(),
            "clipboard backend child {pid} survived timeout"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn native_payload_preserves_unicode() {
        let text = "Ködade 日本語 🚀";
        let payload = native_payload(text);
        #[cfg(windows)]
        {
            let mut expected = vec![0xff, 0xfe];
            expected.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            assert_eq!(payload, expected);
        }
        #[cfg(not(windows))]
        assert_eq!(payload, text.as_bytes());
    }
}
