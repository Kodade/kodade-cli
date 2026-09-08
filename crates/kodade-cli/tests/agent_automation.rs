use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const BIN: &str = env!("CARGO_BIN_EXE_kodade-cli");
static NEXT_HARNESS: AtomicU64 = AtomicU64::new(0);

struct Harness {
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    node_hook_ready: PathBuf,
    session: String,
    daemon: Child,
}

impl Harness {
    fn new() -> Self {
        // macOS TMPDIR paths alone can consume most of sockaddr_un.
        let root = PathBuf::from("/tmp").join(format!(
            "ka-{}-{}-{}",
            std::process::id(),
            NEXT_HARNESS.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let runtime = root.join("runtime");
        let state = root.join("state");
        fs::create_dir_all(&runtime).expect("create runtime directory");
        fs::create_dir_all(&state).expect("create state directory");
        let node_hook_ready = root.join("node-hook-ready");
        let session = "automation".to_owned();
        let daemon = Command::new(BIN)
            .env("HOME", &root)
            .env("SHELL", "/bin/sh")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state)
            .env("KODADE_TEST_NODE_HOOK_READY", &node_hook_ready)
            .args(["-s", &session, "daemon"])
            .spawn()
            .expect("start daemon");
        let harness = Self {
            root,
            runtime,
            state,
            node_hook_ready,
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
        let panes = self.command(["pane", "ls", "--json"]);
        panic!(
            "agent {target} was not recognized: {}",
            String::from_utf8_lossy(&panes.stdout)
        );
    }

    fn wait_for_node_hook_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(result) = fs::read_to_string(&self.node_hook_ready) {
                assert_eq!(result, "ready", "initial Node hook report failed: {result}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "initial Node hook report did not finish within 8 seconds"
            );
            thread::sleep(Duration::from_millis(25));
        }
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
        let _ = self.command(["kill-session"]);
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

fn node_hook_script(on_input: &str) -> String {
    format!(
        r#"exec node -e '
const {{ spawn, spawnSync }} = require("child_process");
const fs = require("fs");
const ready = process.env.KODADE_TEST_NODE_HOOK_READY;
const publish = value => {{
  fs.writeFileSync(`${{ready}}.tmp`, value);
  fs.renameSync(`${{ready}}.tmp`, ready);
}};
const report = state => spawnSync(
  process.env.KODADE_BIN,
  ["-s", process.env.KODADE_SESSION, "agent", "report", process.env.KODADE_PANE,
   state, "--source", "kodade:pi", "--native-agent", "pi"],
  {{ encoding: "utf8" }},
);
const initial = report("working");
if (initial.error || initial.status !== 0) {{
  publish(JSON.stringify({{
    status: initial.status,
    error: initial.error?.message,
    stdout: initial.stdout,
    stderr: initial.stderr,
  }}));
  process.exit(1);
}}
publish("ready");
{on_input}
'"#
    )
}

#[test]
fn hook_backed_node_agent_needs_no_osc_title() {
    let harness = Harness::new();
    // This is the same detached child-CLI report path generated by the Pi/OMP
    // adapters, but the foreground process is Node and never writes OSC.
    harness.start(
        "hook-pi",
        &node_hook_script(
            "process.stdin.on(\"data\", () => report(\"done\")); setTimeout(() => {}, 10000);",
        ),
    );
    harness.wait_for_node_hook_ready();
    harness.wait_for_agent("Pi");
    for _ in 0..2 {
        let upgraded = harness.command(["session", "upgrade"]);
        assert!(
            upgraded.status.success(),
            "upgrade failed: {}",
            String::from_utf8_lossy(&upgraded.stderr)
        );
        harness.wait_for_agent("Pi");
    }
    thread::sleep(Duration::from_millis(2100));
    let output = harness.command([
        "agent",
        "prompt",
        "Pi",
        "hooked",
        "--wait",
        "--timeout",
        "5",
    ]);
    assert!(
        output.status.success(),
        "hook-backed prompt failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hook_identity_does_not_survive_a_node_to_sleep_replacement() {
    let harness = Harness::new();
    // Pi's hook is emitted from Node without OSC. The Node process then exits
    // after handing the PTY to sleep, leaving the sticky hook state behind.
    // Guarded automation must not paste into that replacement process.
    harness.start(
        "hook-pi-replacement",
        &node_hook_script("process.stdin.once(\"data\", () => { spawn(\"sleep\", [\"10\"], { stdio: \"inherit\" }); process.exit(0); });"),
    );
    harness.wait_for_node_hook_ready();
    harness.wait_for_agent("Pi");
    let pane = harness.pane_id("Pi").to_string();
    assert!(harness
        .command(["pane", "send-keys", &pane, "retire", "Enter"])
        .status
        .success());
    let deadline = Instant::now() + Duration::from_secs(4);
    while harness.command(["agent", "read", "Pi"]).status.success() {
        assert!(
            Instant::now() < deadline,
            "retired Node process remained identified"
        );
        thread::sleep(Duration::from_millis(30));
    }
    let rejected = harness.command(["agent", "prompt", "Pi", "SENTINEL-MUST-NOT-ARRIVE"]);
    assert!(
        !rejected.status.success(),
        "stale hook identity accepted replacement: {}",
        String::from_utf8_lossy(&rejected.stderr)
    );
}
