#!/usr/bin/env python3
"""Exercise the built CLI with real daemons in a disposable home and runtime."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
assert binary.is_file(), f"Build first: cargo build ({binary} missing)"

with tempfile.TemporaryDirectory(prefix="kodade-smoke-") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=str(root), XDG_RUNTIME_DIR=str(root / "run"),
               XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh")

    def run(*args, extra=None, success=True):
        result = subprocess.run([str(binary), *args], env=env | (extra or {}),
                                capture_output=True, text=True, timeout=12)
        if success:
            assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result

    try:
        # Read-only diagnostics must not start a daemon or initialize user config.
        report = json.loads(run("doctor", "--json").stdout)
        socket = Path(report["socket"])
        assert not socket.exists()
        run("config", "init")
        config = root / ".config/kodade-cli/config.toml"
        original = config.read_bytes()
        assert run("config", "init", success=False).returncode != 0
        assert config.read_bytes() == original
        run("config", "validate")

        # Creating a workspace is sufficient on a cold start; no attached TUI needed.
        workspace = run("new", "-w", "smoke", str(root)).stdout.strip()
        assert workspace.isdigit()
        assert socket.exists()
        pane = run("run", "--name", "probe", "--", "sh", "-c",
                   "printf 'KODADE_SMOKE_OK\\n'; exec sleep 30").stdout.strip()
        deadline = time.monotonic() + 4
        while "KODADE_SMOKE_OK" not in run("pane", "read", pane).stdout:
            assert time.monotonic() < deadline, "pane never produced output"
            time.sleep(0.05)
        report = json.loads(run("doctor", "--json").stdout)
        assert next(check for check in report["checks"] if check["name"] == "daemon")["status"] == "ok"
        assert run("-s", "../outside", "session", "path", success=False).returncode != 0

        # Agent start also creates a missing session, and `current` cannot cross sockets.
        cold = run("-s", "cold-agent", "agent", "start", "--name", "cold", "--",
                   "sh", "-c", "sleep 30").stdout.strip()
        assert cold.isdigit()
        cold_socket = run("-s", "cold-agent", "session", "path").stdout.strip()
        foreign = run("--socket", cold_socket, "agent", "read", "current",
                      extra={"KODADE_SESSION": "default", "KODADE_SOCKET": str(socket),
                             "KODADE_PANE": pane}, success=False)
        assert foreign.returncode != 0 and "different socket" in foreign.stderr

        # Inherited sockets remain authoritative after a live session rename.
        run("session", "rename", "renamed")
        renamed_socket = run("-s", "renamed", "session", "path").stdout.strip()
        inherited = {"KODADE_SESSION": "renamed", "KODADE_SOCKET": renamed_socket}
        assert run("session", "path", extra=inherited).stdout.strip() == renamed_socket
        assert json.loads(run("ls", "--json", extra=inherited).stdout)["workspaces"]
        explicit = run("-s", "elsewhere", "session", "path", extra=inherited).stdout.strip()
        assert explicit != renamed_socket
        assert not Path(explicit).exists()
        run("--socket", renamed_socket, "kill-session")
        deadline = time.monotonic() + 3
        while Path(renamed_socket).exists():
            assert time.monotonic() < deadline, "daemon did not release its socket"
            time.sleep(0.05)
    finally:
        # Cleanup is scoped to sessions created by this test, even after assertions fail.
        for session in ("default", "renamed", "cold-agent"):
            run("-s", session, "kill-session", success=False)

print("CLI smoke passed: cold start, real PTY output, diagnostics, config preservation, session context, rename, shutdown")
