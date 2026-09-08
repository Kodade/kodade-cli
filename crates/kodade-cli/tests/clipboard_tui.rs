#![cfg(unix)]

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Tui(Child);
impl Drop for Tui {
    fn drop(&mut self) {
        // `script` owns a child TUI; clean the fixture's entire process group.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const BIN: &str = env!("CARGO_BIN_EXE_kodade-cli");

struct Harness {
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    fake_bin: PathBuf,
    copied: PathBuf,
    session: String,
    daemon: Child,
}

impl Harness {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root =
            PathBuf::from("/tmp").join(format!("kc-clipboard-{}-{nonce}", std::process::id()));
        let runtime = root.join("runtime");
        let state = root.join("state");
        let fake_bin = root.join("bin");
        let copied = root.join("clipboard");
        fs::create_dir_all(&runtime).expect("create runtime");
        fs::create_dir_all(&state).expect("create state");
        fs::create_dir_all(&fake_bin).expect("create fake backend directory");
        let session = "clipboard".to_owned();
        let mut daemon_command = Command::new(BIN);
        base_env(&mut daemon_command, &root, &runtime, &state, &fake_bin);
        let daemon = daemon_command
            .args(["-s", &session, "daemon"])
            .spawn()
            .expect("start daemon");
        let harness = Self {
            root,
            runtime,
            state,
            fake_bin,
            copied,
            session,
            daemon,
        };
        for _ in 0..80 {
            if harness
                .command(["ls", "--json"])
                .output()
                .expect("probe daemon")
                .status
                .success()
            {
                return harness;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("daemon did not accept requests");
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(BIN);
        base_env(
            &mut command,
            &self.root,
            &self.runtime,
            &self.state,
            &self.fake_bin,
        );
        command.arg("-s").arg(&self.session).args(args);
        command
    }

    fn backend(&self, body: &str) {
        let backend = self.fake_bin.join(if cfg!(target_os = "macos") {
            "pbcopy"
        } else {
            "wl-copy"
        });
        fs::write(&backend, format!("#!/bin/sh\n{body}\n")).expect("write fake backend");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&backend, fs::Permissions::from_mode(0o755))
            .expect("make fake backend executable");
    }

    fn start_tui(&self) -> (Tui, Arc<Mutex<Vec<u8>>>) {
        let mut command = Command::new("script");
        base_env(
            &mut command,
            &self.root,
            &self.runtime,
            &self.state,
            &self.fake_bin,
        );
        // `script` supplies the controlling PTY that crossterm requires; a
        // pipe would make the TUI reject raw mode before it can receive copies.
        let line = format!("stty cols 100 rows 30; exec {BIN} -s {}", self.session);
        if cfg!(target_os = "macos") {
            command.args(["-q", "/dev/null", "/bin/sh", "-c", &line]);
        } else {
            command.args(["-qfec", &line, "/dev/null"]);
        }
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let mut tui = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start TUI in controlling PTY");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let mut stdout = tui.stdout.take().expect("capture TUI output");
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            loop {
                let count = stdout.read(&mut buffer).expect("read TUI output");
                if count == 0 {
                    break;
                }
                sink.lock()
                    .expect("TUI output lock")
                    .extend_from_slice(&buffer[..count]);
            }
        });
        (Tui(tui), output)
    }

    fn focused_pane(&self) -> String {
        let output = self
            .command(["pane", "ls", "--json"])
            .output()
            .expect("list panes");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("pane list JSON")[0]
            ["id"]
            .as_u64()
            .expect("focused pane id")
            .to_string()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn base_env(command: &mut Command, root: &Path, runtime: &Path, state: &Path, fake_bin: &Path) {
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    command
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_TTY")
        .env("HOME", root)
        .env("SHELL", "/bin/sh")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .env("WAYLAND_DISPLAY", "clipboard-test")
        .env("PATH", path);
}

#[track_caller]
fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "condition was not met within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn host_tui_uses_native_clipboard_then_emits_osc52_on_backend_timeout() {
    let harness = Harness::new();
    harness.backend(&format!("cat > {}", harness.copied.display()));
    let (mut tui, output) = harness.start_tui();
    wait_for(Duration::from_secs(5), || {
        contains(&output.lock().unwrap(), b"\x1b[?1049h")
    });
    let pane = harness.focused_pane();

    let native = harness
        .command([
            "send",
            &pane,
            "printf '\\033]52;c;S8O2ZGFkZSDml6XmnKzoqp4g8J+agA==\\007'",
        ])
        .output()
        .expect("send native clipboard request");
    assert!(
        native.status.success(),
        "{}",
        String::from_utf8_lossy(&native.stderr)
    );
    wait_for(Duration::from_secs(3), || {
        fs::read_to_string(&harness.copied).ok().as_deref() == Some("Ködade 日本語 🚀")
    });
    assert_eq!(
        fs::read_to_string(&harness.copied).unwrap(),
        "Ködade 日本語 🚀"
    );

    // A stalled host backend must not freeze the terminal forever. The TUI
    // falls back to OSC 52 on its own controlling PTY after the two-second cap.
    let started = harness.root.join("backend-started");
    harness.backend(&format!("echo ready > {}; sleep 10", started.display()));
    let fallback = harness
        .command(["send", &pane, "printf '\\033]52;c;ZmFsbGJhY2s=\\007'"])
        .output()
        .expect("send fallback clipboard request");
    assert!(
        fallback.status.success(),
        "{}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    wait_for(Duration::from_secs(5), || started.exists());
    assert!(harness
        .command(["send", &pane, "printf 'RESPONSIVE_CANVAS\\n'"])
        .status()
        .unwrap()
        .success());
    wait_for(Duration::from_secs(1), || {
        let transcript = output.lock().unwrap();
        contains(&transcript, b"RESPONSIVE_CANVAS")
    });
    wait_for(Duration::from_secs(4), || {
        contains(&output.lock().unwrap(), b"\x1b]52;c;ZmFsbGJhY2s=")
    });
    assert!(harness
        .command(["kill-session"])
        .status()
        .unwrap()
        .success());
    wait_for(Duration::from_secs(5), || {
        tui.0.try_wait().unwrap().is_some()
    });
    wait_for(Duration::from_secs(1), || {
        contains(&output.lock().unwrap(), b"\x1b[?1049l")
    });
}
