#!/usr/bin/env python3
"""Verify launch-directory selection through real TUI clients and daemon PTYs."""
import errno
import fcntl
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

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="kd-start-", dir="/tmp") as directory:
    root = Path(directory).resolve()
    env = {k: v for k, v in os.environ.items() if not k.startswith("KODADE_")}
    env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
               XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"),
               SHELL="/bin/sh", TERM="xterm-256color")
    config = root / "config/kodade-cli"
    config.mkdir(parents=True)
    (config / "config.toml").write_text('theme = "kodade-dark"\n')
    (config / "state").write_text("help_seen = true\n")
    old, first, second = (root / name for name in ("old-project", "launch-project", "another-project"))
    for path in (old, first, second):
        path.mkdir()

    def command(*args):
        return subprocess.run([str(BINARY), *args], env=env, cwd=root,
                              capture_output=True, text=True, check=True, timeout=12).stdout

    def layout():
        return json.loads(command("ls", "--json"))

    clients = []

    class Client:
        def __init__(self, cwd, args=(), extra=None):
            self.master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
            self.process = subprocess.Popen([str(BINARY), *args], env=env | (extra or {}),
                                            cwd=cwd, stdin=slave, stdout=slave, stderr=slave,
                                            preexec_fn=controlling_terminal)
            os.close(slave)
            self.output = bytearray()
            clients.append(self)

        def drain(self):
            while select.select([self.master], [], [], 0)[0]:
                try:
                    data = os.read(self.master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        return
                    raise
                if not data:
                    return
                self.output.extend(data)

        def wait(self, label, predicate):
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                self.drain()
                if predicate():
                    return
                if self.process.poll() is not None:
                    # Detach can finish between the predicate and exit check.
                    if predicate():
                        return
                    raise AssertionError(self.output.decode(errors="replace"))
                time.sleep(.03)
            raise AssertionError(f"{label}: {self.output[-1500:]!r}")

        def ready(self, name):
            self.wait(f"workspace {name}", lambda: name.encode() in self.output and
                      self.output.rfind(b"\x1b[?2026l") > self.output.rfind(b"\x1b[?2026h"))

        def pwd(self, name, expected):
            marker = root / name
            os.write(self.master, f"pwd > '{marker}'\r".encode())
            self.wait("shell launch directory", lambda: marker.exists() and marker.read_text().strip() == str(expected))

        def close(self):
            if self.process.poll() is None:
                os.write(self.master, b"\x02d")
                self.wait("detach", lambda: self.process.poll() is not None)
                assert self.process.returncode == 0

    try:
        command("new", "-w", "saved-name", str(old))
        initial = layout()
        a = Client(first)
        a.ready("launch-project")
        a.pwd("first-pwd", first)
        created = layout()
        selected = next(w for w in created["workspaces"] if w["root"] == str(first))
        assert selected["name"] == "launch-project"
        assert created["active_workspace"] == initial["active_workspace"], "changed scripting selection"

        # Reuse by canonical path, even when launched through a symlink.
        alias = root / "alias"
        alias.symlink_to(first, target_is_directory=True)
        b = Client(alias)
        b.ready("launch-project")
        assert len(layout()["workspaces"]) == len(created["workspaces"])
        b.close()

        c = Client(second)
        c.ready("another-project")
        c.pwd("second-pwd", second)
        a.pwd("still-first-pwd", first)
        c.close()
        a.close()

        count = len(layout()["workspaces"])
        for args, extra in [(("-s", "default"), {}),
                            (("--socket", command("session", "path").strip()), {}),
                            ((), {"KODADE_SESSION": "default"})]:
            resumed = Client(second, args, extra)
            resumed.ready("saved-name")
            resumed.pwd(f"resume-{len(clients)}", old)
            resumed.close()
        assert len(layout()["workspaces"]) == count
        command("kill-session")
        cold = Client(first)
        cold.ready("launch-project")
        cold.pwd("cold-pwd", first)
        cold.close()
    finally:
        for client in clients:
            os.close(client.master)
            if client.process.poll() is None:
                client.process.kill()
        subprocess.run([str(BINARY), "kill-session"], env=env, capture_output=True, timeout=12)
        for client in clients:
            client.process.wait(timeout=10)

print("Startup directory TUI smoke passed: cwd, canonical reuse, independent clients, explicit and inherited resume")
