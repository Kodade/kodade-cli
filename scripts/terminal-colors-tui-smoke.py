#!/usr/bin/env python3
"""Prove a live TUI reports its active theme to terminal color queries."""
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import struct
import subprocess
import sys
import tempfile
import termios
import time


BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
SESSION = "terminal-colors"
DARK = b"\x1b]10;rgb:e8e8/e8e8/e8e8\x1b\\\x1b]11;rgb:1818/1818/1818\x1b\\\x1b]12;rgb:e2e2/b8b8/6e6e\x1b\\\x1b]4;1;rgb:d9d9/7a7a/8080\x1b\\"
LIGHT = b"\x1b]10;rgb:3f3f/3b3b/3434\x1b\\\x1b]11;rgb:fafa/f9f9/f5f5\x1b\\\x1b]12;rgb:9d9d/5757/2929\x1b\\\x1b]4;1;rgb:b0b0/4a4a/4545\x1b\\"


def wait_for(label, predicate, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.02)
    raise RuntimeError(f"timed out waiting for {label}")


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="kodade-colors-", dir="/tmp") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.pop("NO_COLOR", None)
    env.update(HOME=directory, XDG_CONFIG_HOME=str(root / ".config"),
               XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"),
               SHELL="/bin/sh", TERM="xterm-256color")
    config = root / ".config" / "kodade-cli"
    config.mkdir(parents=True)
    (config / "config.toml").write_text('theme = "kodade-dark"\nsidebar = false\n')
    (config / "state").write_text("help_seen = true\n")

    response = root / "response"
    probe = root / "probe.py"
    probe.write_text(f"""
import os, select, termios, time, tty
fd = os.open('/dev/tty', os.O_RDWR)
old = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(fd, b'\\x1b]10;')
    os.write(fd, b'?\\x1b\\\\')
    os.write(fd, b'\\x1b]11;?\\x1b\\\\\\x1b]12;?\\x1b\\\\\\x1b]4;1;?\\x1b\\\\')
    captured = bytearray()
    deadline = time.monotonic() + 3
    while captured.count(b'\\x1b\\\\') < 4 and time.monotonic() < deadline:
        readable, _, _ = select.select([fd], [], [], .05)
        if readable:
            captured.extend(os.read(fd, 4096))
    open({str(response)!r}, 'wb').write(captured)
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
""")

    def command(*args):
        return subprocess.run([str(BINARY), "--session", SESSION, *args], env=env,
                              check=True, capture_output=True, text=True)

    master = slave = tui = None
    try:
        command("new", "--workspace", "main", str(root))
        probe_pane = next(pane for pane in json.loads(command("pane", "ls", "--json").stdout)
                          if pane["focused"])["id"]
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        original = termios.tcgetattr(slave)
        tui = subprocess.Popen([str(BINARY), "--session", SESSION], stdin=slave, stdout=slave,
                               stderr=slave, env=env, preexec_fn=controlling_terminal)
        transcript = bytearray()

        def drain():
            if select.select([master], [], [], 0)[0]:
                try:
                    transcript.extend(os.read(master, 65536))
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise

        def query(label, expected, through_tui=True):
            response.unlink(missing_ok=True)
            command_line = shlex.join([sys.executable, str(probe)])
            if through_tui:
                os.write(master, (command_line + "\r").encode())
            else:
                command("send", str(probe_pane), command_line)
            wait_for(label, lambda: (drain() is None) and response.exists())
            actual = response.read_bytes()
            assert actual == expected, f"{label}: got {actual!r}, expected {expected!r}"

        wait_for("initial attached frame", lambda: (drain() is None) and b"?2004h" in transcript)
        query("dark terminal colors", DARK)

        # prefix+s opens Settings, and Enter toggles its selected theme row.
        os.write(master, b"\x02s\r")
        wait_for("light theme frame", lambda: (drain() is None) and b"kodade-light" in transcript)
        query("live light terminal colors", LIGHT, through_tui=False)

        command("session", "upgrade")
        # Daemon readiness does not establish that this client has reconnected:
        # offline input is intentionally dropped. A fresh child-output row
        # proves the replacement daemon reached the actual attached TUI.
        marker = b"COLOR-UPGRADE-READY"
        command("send", str(probe_pane), f"printf '%s\\n' {marker.decode()}")
        wait_for("replacement attached frame", lambda: (drain() is None) and marker in transcript)
        query("reconnected light terminal colors", LIGHT, through_tui=False)

        os.write(master, b"\x1b")
        time.sleep(.05)
        drain()
        os.write(master, b"\x02d")
        wait_for("TUI detach", lambda: (drain() is None) and tui.poll() is not None)
        assert tui.wait(timeout=5) == 0
        try:
            restored = termios.tcgetattr(slave)
        except termios.error as error:
            if error.args[0] != errno.ENOTTY:
                raise
            restored = termios.tcgetattr(master)
        assert restored == original, "detach left host terminal raw"
    finally:
        if tui is not None and tui.poll() is None:
            tui.kill()
            tui.wait(timeout=3)
        for fd in (master, slave):
            if fd is not None:
                os.close(fd)
        subprocess.run([str(BINARY), "--session", SESSION, "kill-session"], env=env,
                       capture_output=True)

print("Terminal color TUI smoke passed: dark, live theme change, and handoff reconnect queries")
