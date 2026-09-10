#!/usr/bin/env python3
"""Keep a real attached client on its own pane through two daemon upgrades."""
import fcntl
import errno
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time

binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
with tempfile.TemporaryDirectory(prefix="kh-", dir="/tmp") as directory:
    root = Path(directory)
    env = {k: v for k, v in os.environ.items() if not k.startswith("KODADE_")}
    env.update(HOME=directory, XDG_RUNTIME_DIR=str(root / "run"),
               XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh", TERM="xterm-256color")
    config = root / ".config/kodade-cli/config.toml"
    config.parent.mkdir(parents=True)
    config.write_text('theme = "kodade-dark"\n')
    def run(*args, success=True):
        result = subprocess.run([str(binary), *args], env=env, text=True,
                                capture_output=True, timeout=40)
        if success:
            assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result
    def wait_file(path, drain=lambda: time.sleep(0.03)):
        deadline = time.monotonic() + 8
        while not path.exists() or not path.read_text():
            assert time.monotonic() < deadline, f"shell did not produce {path}"
            drain()
        return path.read_text()
    master = slave = None
    client = None
    try:
        run("new", "-w", "first", directory)
        first = run("run", "--name", "first-shell", "--", "/bin/sh").stdout.strip()
        run("new", "-w", "second", directory)
        second = run("run", "--name", "second-shell", "--", "/bin/sh").stdout.strip()
        identity = root / "identity"
        run("send", second, f"printf '%s' \"$$\" > {identity}")
        pid = wait_file(identity)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        modes = termios.tcgetattr(slave)
        def control_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        client = subprocess.Popen([str(binary), "-s", "default"], env=env, stdin=slave, stdout=slave,
                                  stderr=slave, preexec_fn=control_terminal)
        transcript = bytearray()
        def drain():
            assert client.poll() is None, ("TUI exited", client.returncode, bytes(transcript[-3000:]))
            if select.select([master], [], [], 0.03)[0]:
                transcript.extend(os.read(master, 65536))
        deadline = time.monotonic() + 5
        while b"\x1b[?2004h" not in transcript or b"second" not in transcript:
            assert time.monotonic() < deadline, bytes(transcript)
            drain()
        settle = time.monotonic() + 0.2
        while time.monotonic() < settle:
            drain()
        baseline = root / "baseline"
        os.write(master, f"printf '%s' \"$$\" > {baseline}\r".encode())
        assert wait_file(baseline, drain) == pid, "initial TUI focus was not ready"
        # One-shot scripts change the default selection, leaving the attached
        # client's view on second. Reconnect must restore that independent view.
        run("pane", "focus", first)
        for generation in (1, 2):
            run("session", "upgrade")
            settle = time.monotonic() + 0.7
            while time.monotonic() < settle:
                drain()
            result = root / f"result-{generation}"
            os.write(master, f"printf '%s:%s' \"$$\" \"$(({generation} * 173))\" > {result}\r".encode())
            assert wait_file(result, drain) == f"{pid}:{generation * 173}", "reconnect changed the focused shell"
        os.write(master, b"\x02d")
        deadline = time.monotonic() + 5
        while client.poll() is None:
            assert time.monotonic() < deadline, "TUI did not detach"
            if select.select([master], [], [], 0.03)[0]:
                transcript.extend(os.read(master, 65536))
        assert client.returncode == 0
        try:
            restored_modes = termios.tcgetattr(slave)
        except termios.error as error:
            # macOS detaches the slave from the ended controlling session.
            # The master remains the same PTY and still exposes its termios.
            if error.args[0] != errno.ENOTTY:
                raise
            restored_modes = termios.tcgetattr(master)
        assert restored_modes == modes, "TUI did not restore terminal modes"
        assert not list((root / "run/kodade-cli").glob(".up-*")), "handoff staging leaked"
        print("Live TUI handoff passed: two replacements, independent focus, original shell execution, terminal cleanup")
    finally:
        if "transcript" in locals():
            Path("/tmp/kodade-handoff-tui.log").write_bytes(transcript)
        if sys.exc_info()[0] is not None:
            for label in ("first", "second"):
                if label in locals():
                    print(label, run("pane", "read", locals()[label], success=False).stdout)
        if client and client.poll() is None:
            client.kill()
            client.wait(timeout=3)
        for fd in (master, slave):
            if fd is not None:
                os.close(fd)
        run("kill-session", success=False)
