use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const BIN: &str = env!("CARGO_BIN_EXE_kodade-cli");

struct Harness {
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    session: String,
    daemon: Child,
}

impl Harness {
    fn new() -> Self {
        // macOS TMPDIR paths alone can consume most of sockaddr_un.
        let root = PathBuf::from("/tmp").join(format!(
            "ka-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let runtime = root.join("runtime");
        let state = root.join("state");
        fs::create_dir_all(&runtime).expect("create runtime directory");
        fs::create_dir_all(&state).expect("create state directory");
        let session = "automation".to_owned();
        let daemon = Command::new(BIN)
            .env("HOME", &root)
            .env("SHELL", "/bin/sh")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state)
            .args(["-s", &session, "daemon"])
            .spawn()
            .expect("start daemon");
        let harness = Self {
            root,
            runtime,
            state,
            session,
            daemon,
        };
        for _ in 0..80 {
            if harness.command(["ls", "--json"]).status.success() {
                return harness;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("daemon did not accept requests");
    }

    fn command<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(BIN);
        command
            .env("HOME", &self.root)
            .env("SHELL", "/bin/sh")
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_STATE_HOME", &self.state)
            .arg("-s")
            .arg(&self.session)
            .args(args);
        run_bounded(&mut command, Duration::from_secs(8))
    }

    fn start(&self, name: &str, script: &str) {
        let output = self.command(["agent", "start", "--name", name, "--", "sh", "-c", script]);
        assert!(
            output.status.success(),
            "agent start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn wait_for_agent(&self, target: &str) {
        for _ in 0..80 {
            if self.command(["agent", "read", target]).status.success() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("agent {target} was not recognized");
    }

    fn pane_id(&self, target: &str) -> u64 {
        let output = self.command(["agent", "read", target, "--json"]);
        assert!(output.status.success(), "agent read must succeed");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
        value["pane"]["id"].as_u64().expect("pane id")
    }
}

/// Integration commands must not hang the test process if a daemon or client
/// regresses. Kill and reap before failing so temp state remains removable.
fn run_bounded(command: &mut Command, timeout: Duration) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI command");
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll CLI command").is_some() {
            return child
                .wait_with_output()
                .expect("collect CLI command output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI command exceeded {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn prompt_wait_requires_new_lifecycle_and_rejects_bad_targets() {
    let harness = Harness::new();
    // `KODADE_BIN`, `KODADE_PANE`, and `KODADE_SESSION` are injected by the
    // daemon. The fake has no network/API dependency but exercises the actual
    // child CLI hook path, PTY parser, guarded prompt protocol, and polling.
    harness.start(
        "fake-codex",
        "printf '\\033]0;Codex\\007'; while IFS= read -r line; do \"$KODADE_BIN\" -s \"$KODADE_SESSION\" agent report \"$KODADE_PANE\" working; printf 'reply:%s\\n' \"$line\"; \"$KODADE_BIN\" -s \"$KODADE_SESSION\" agent report \"$KODADE_PANE\" done; done",
    );
    harness.wait_for_agent("Codex");
    // Let the login shell finish its exec handoff before recording the guard.
    // The production guard deliberately rejects that handoff instead of writing
    // to an identity it did not inspect.
    thread::sleep(Duration::from_millis(2100));
    let pane = harness.pane_id("Codex");

    // A sticky Done baseline must not be accepted for the next prompt. The
    // fake reports Working then Done with no deliberate delay. The revision
    // makes that lifecycle visible even if both reports land between polls.
    assert!(harness
        .command(["agent", "report", &pane.to_string(), "done"])
        .status
        .success());
    let output = harness.command([
        "agent",
        "prompt",
        "Codex",
        "first",
        "--wait",
        "--timeout",
        "5",
    ]);
    assert!(
        output.status.success(),
        "prompt failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A blocked agent is rejected before any bytes are pasted into its PTY.
    assert!(harness
        .command(["agent", "report", &pane.to_string(), "blocked"])
        .status
        .success());
    let blocked = harness.command(["agent", "prompt", "Codex", "SENTINEL-MUST-NOT-ARRIVE"]);
    assert!(!blocked.status.success());
    let output = harness.command(["agent", "read", "Codex", "--scrollback"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SENTINEL-MUST-NOT-ARRIVE"));

    // A live agent that does not settle returns the documented timeout status.
    harness.start(
        "fake-claude",
        "printf '\\033]0;Claude\\007'; IFS= read -r ignored; sleep 10",
    );
    harness.wait_for_agent("Claude Code");
    thread::sleep(Duration::from_millis(2100));
    let started = Instant::now();
    let timed_out = harness.command([
        "agent",
        "prompt",
        "Claude Code",
        "timeout",
        "--wait",
        "--timeout",
        "1",
    ]);
    assert_eq!(timed_out.status.code(), Some(2));
    assert!(started.elapsed() < Duration::from_secs(3));

    // If the process changes its OSC identity after submission, the waiter
    // fails rather than treating a replacement shell's state as the agent's.
    harness.start(
        "replacement",
        "printf '\\033]0;OpenCode\\007'; IFS= read -r ignored; printf '\\033]0;shell\\007'; sleep 4",
    );
    harness.wait_for_agent("OpenCode");
    thread::sleep(Duration::from_millis(2100));
    let replaced = harness.command([
        "agent",
        "prompt",
        "OpenCode",
        "replace",
        "--wait",
        "--timeout",
        "5",
    ]);
    assert!(!replaced.status.success());
    assert!(String::from_utf8_lossy(&replaced.stderr).contains("replaced while waiting"));
}
