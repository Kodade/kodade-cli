#!/usr/bin/env python3
"""Exercise opt-in cold screen replay with real daemon processes and PTYs."""

import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = (Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target/debug/kodade-cli").resolve()
SESSION = "history-smoke"


def wait_until(label, check, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {label}")


with tempfile.TemporaryDirectory(prefix="kodade-history-smoke-") as temp:
    root = Path(temp)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(
        HOME=str(root / "home"),
        XDG_RUNTIME_DIR=str(root / "run"),
        XDG_STATE_HOME=str(root / "state"),
        SHELL="/bin/sh",
        SMOKE_MARKER=f"history-{secrets.token_hex(10)}",
    )
    for directory in (Path(env["HOME"]), Path(env["XDG_RUNTIME_DIR"])):
        directory.mkdir(parents=True)
    config = Path(env["HOME"]) / ".config/kodade-cli/config.toml"
    config.parent.mkdir(parents=True)
    config.write_text("[session]\npane_history = true\n")

    def run(*args, check=True):
        return subprocess.run(
            [str(BINARY), "-s", SESSION, *args], env=env, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=check, timeout=10,
        )

    daemon = None

    def start_daemon():
        global daemon
        daemon = subprocess.Popen(
            [str(BINARY), "-s", SESSION, "daemon"], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        wait_until("daemon startup", lambda: run("ls", "--json", check=False).returncode == 0)

    def stop_daemon():
        global daemon
        assert daemon is not None
        daemon.send_signal(signal.SIGTERM)
        daemon.wait(timeout=8)
        if daemon.returncode != 0:
            raise RuntimeError(f"daemon exited {daemon.returncode}: {daemon.stderr.read()}")
        daemon = None

    try:
        start_daemon()
        pane = run("run", "--name", "history-marker", "--", "sh", "-c",
                   'printf "%s pid=$$\\n" "$SMOKE_MARKER"; exec sleep 60').stdout.strip()
        marker = env["SMOKE_MARKER"]
        wait_until("real PTY marker", lambda: marker in run("pane", "read", pane).stdout)
        original = run("pane", "read", pane).stdout
        old_pid = int(original.split("pid=")[1].split()[0])
        old_start = Path(f"/proc/{old_pid}/stat").read_text().split()[21]
        # A real independent client makes the saved pane wider and taller than
        # the cold daemon's initial 80x24 dimensions.
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(5)
            client.connect(str(Path(env["XDG_RUNTIME_DIR"]) / "kodade-cli/history-smoke.sock"))
            reader = client.makefile("rb")
            client.sendall(json.dumps({"Query": "Version"}).encode() + b"\n")
            protocol = json.loads(reader.readline())["Version"]["version"]
            client.sendall(json.dumps({"Hello": {"cols": 140, "rows": 45, "version": protocol}}).encode() + b"\n")
            while "Welcome" not in json.loads(reader.readline()):
                pass
            reader.close()
        history = Path(env["XDG_STATE_HOME"]) / "kodade-cli/sessions/history-smoke.history.json"
        def wide_history():
            if not history.exists():
                return False
            saved = json.loads(history.read_text())
            return any(item["pane"] == int(pane) and len(item["screen"]["rows"]) > 24
                       for item in saved["panes"])
        wait_until("private wide history file", wide_history)
        assert marker in history.read_text(), "history did not retain terminal output"
        assert json.loads(history.read_text())["session"] == json.loads(history.with_name("history-smoke.json").read_text())

        # SIGTERM reaches only our explicitly started daemon. Do not use
        # kill-session: it intentionally deletes the state being tested.
        stop_daemon()
        if Path(f"/proc/{old_pid}").exists():
            os.kill(old_pid, signal.SIGTERM)
        start_daemon()
        restored_layout = json.loads(run("ls", "--json").stdout)
        restored = next(item["id"] for item in restored_layout["panes"] if item["title"] == "history-marker")
        wait_until("replayed marker", lambda: marker in run("pane", "read", str(restored)).stdout)

        # This command is a fresh child, so a surviving old PTY cannot satisfy
        # the smoke proof by merely echoing its prior output.
        fresh = run("run", "--name", "fresh-child", "--", "sh", "-c",
                    'printf "fresh pid=$$\\n"; exec sleep 60').stdout.strip()
        wait_until("fresh child marker", lambda: "fresh pid=" in run("pane", "read", fresh).stdout)
        fresh_text = run("pane", "read", fresh).stdout
        fresh_pid = int(fresh_text.split("pid=")[1].split()[0])
        fresh_start = Path(f"/proc/{fresh_pid}/stat").read_text().split()[21]
        assert (fresh_pid, fresh_start) != (old_pid, old_start), "cold restart reused the old child"

        run("session", "rename", "history-renamed")
        renamed_history = history.with_name("history-renamed.history.json")
        assert renamed_history.exists() and not history.exists(), "rename did not move history"
        SESSION = "history-renamed"
        stop_daemon()

        # Starting with the setting off actively removes a prior private replay.
        config.write_text("[session]\npane_history = false\n")
        start_daemon()
        assert not renamed_history.exists(), "disabled history was not cleaned up"
        run("kill-session")
        wait_until("kill cleanup", lambda: not (Path(env["XDG_STATE_HOME"]) / "kodade-cli/sessions/history-renamed.json").exists())

        # A default session never stores terminal output, even after a real PTY writes.
        SESSION = "history-default"
        start_daemon()
        default = run("run", "--", "sh", "-c", 'printf "%s\\n" "$SMOKE_MARKER"; exec sleep 60').stdout.strip()
        wait_until("default PTY marker", lambda: marker in run("pane", "read", default).stdout)
        time.sleep(2.2)
        default_history = Path(env["XDG_STATE_HOME"]) / "kodade-cli/sessions/history-default.history.json"
        assert not default_history.exists(), "default session wrote terminal history"
        for path in (Path(env["XDG_STATE_HOME"]) / "kodade-cli").rglob("*.json"):
            assert marker not in path.read_text(errors="ignore"), f"default output leaked to {path}"
        run("kill-session")
        print("History smoke passed: opt-in replay, fresh child, rename/disable/kill cleanup, default privacy")
    finally:
        if daemon is not None:
            daemon.send_signal(signal.SIGTERM)
            try:
                daemon.wait(timeout=3)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=3)
