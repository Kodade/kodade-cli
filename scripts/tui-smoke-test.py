#!/usr/bin/env python3
"""Exercise terminal restoration after detach and an actual socket write failure."""

import fcntl
import json
import os
from pathlib import Path
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()


def attached(env, args, interact):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
    original = termios.tcgetattr(slave)

    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen([str(BINARY), *args], stdin=slave, stdout=slave, stderr=slave,
                               env=env, preexec_fn=controlling_terminal)
    transcript = bytearray()

    def drain(seconds=0.05):
        if select.select([master], [], [], seconds)[0]:
            transcript.extend(os.read(master, 65536))

    try:
        interact(process, master, transcript, drain)
        deadline = time.monotonic() + 5
        while process.poll() is None:
            assert time.monotonic() < deadline, ("attached client did not exit", bytes(transcript[:2000]))
            drain()
        while select.select([master], [], [], 0)[0]:
            drain(0)
        assert termios.tcgetattr(slave) == original, "client left terminal modes changed"
        assert b"\x1b[?1049l" in transcript, "alternate screen was not restored"
        assert b"\x1b[?2004l" in transcript, "bracketed paste was not restored"
        assert b"\x1b[?25h" in transcript, "cursor was not restored"
        return process.returncode
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=3)
        os.close(master)
        os.close(slave)


with tempfile.TemporaryDirectory(prefix="kt-") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=directory, XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"),
               SHELL="/bin/sh", TERM="xterm-256color")
    config = root / ".config/kodade-cli/config.toml"
    config.parent.mkdir(parents=True)
    config.write_text('theme = "kodade-dark"\n')

    def detach(process, master, transcript, drain):
        deadline = time.monotonic() + 5
        while b"\x1b[?2004h" not in transcript:
            assert process.poll() is None and time.monotonic() < deadline, bytes(transcript)
            drain()
        # Let the first layout arrive, then dismiss its first-run guidance.
        ready = time.monotonic() + 0.5
        while time.monotonic() < ready:
            drain()
        os.write(master, b"\x1b")
        ready = time.monotonic() + 0.15
        while time.monotonic() < ready:
            drain()
        os.write(master, b"\x02d")

    try:
        assert attached(env, ["-s", "tui-smoke"], detach) == 0
    finally:
        subprocess.run([str(BINARY), "-s", "tui-smoke", "kill-session"], env=env,
                       capture_output=True, timeout=12)

    # The fake speaks a valid handshake, then drops the transport. Typing into
    # the broken connection exercises the error return through the real TUI.
    endpoint = root / "failed.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(endpoint))
        listener.listen(1)
        listener.settimeout(5)

        def disconnected(process, master, transcript, drain):
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(3)
                reader = connection.makefile("rb")
                assert "Hello" in json.loads(reader.readline())
                connection.sendall(b'{"Welcome":{"session":"failure","version":1}}\n')
                assert json.loads(reader.readline()) == "Subscribe"
                reader.close()
            deadline = time.monotonic() + 5
            while b"\x1b[?2004h" not in transcript:
                assert process.poll() is None and time.monotonic() < deadline, bytes(transcript)
                drain()
            os.write(master, b"x")

        assert attached(env, ["--socket", str(endpoint)], disconnected) != 0

    # A recoverable daemon Error must stay on the same connection. This catches
    # clients that treat a rejected action as an endpoint failure and silently
    # discard later interactive input.
    endpoint = root / "rejected.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(endpoint))
        listener.listen(1)
        listener.settimeout(5)

        def rejected_action(process, master, transcript, drain):
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(5)
                reader = connection.makefile("rb")
                assert "Hello" in json.loads(reader.readline())
                connection.sendall(b'{"Welcome":{"session":"rejected","version":1}}\n')
                assert json.loads(reader.readline()) == "Subscribe"
                layout = {"Layout": {"active_workspace": 1, "active_tab": 2,
                    "workspaces": [{"id": 1, "name": "main", "active": True, "state": "idle",
                        "root": None, "color": None, "branch": None, "parent": None,
                        "tabs": [{"id": 2, "name": "shell", "state": "idle", "agents": []}]}],
                    "tabs": [{"id": 2, "name": "shell", "active": True, "state": "idle"}],
                    "tree": {"Leaf": {"pane": 3}},
                    "panes": [{"id": 3, "title": "shell", "focused": True, "scroll_offset": 0,
                        "screen": {"contents": "ready", "cursor_row": 0, "cursor_col": 5,
                            "cursor_visible": True, "rows": [], "bracketed_paste": False, "mouse_reporting": False},
                        "agent": None, "agent_generation": 0, "activity_revision": 0, "state": "idle",
                        "state_reason": "", "state_age_secs": 0, "cwd": None}], "zoomed": False, "restored": False}}
                connection.sendall(json.dumps(layout).encode() + b"\n")
                deadline = time.monotonic() + 5
                while b"\x1b[?2004h" not in transcript:
                    assert process.poll() is None and time.monotonic() < deadline, bytes(transcript)
                    drain()
                # Default prefix+x is ClosePane. Reject it, then prove the
                # subsequent literal key reaches this exact socket.
                os.write(master, b"\x02x")
                assert json.loads(reader.readline()) == "ClosePane"
                connection.sendall(b'{"Error":{"message":"close rejected"}}\n')
                wait_until = time.monotonic() + 5
                while b"close rejected" not in transcript:
                    assert process.poll() is None and time.monotonic() < wait_until, bytes(transcript)
                    drain()
                os.write(master, b"z")
                assert json.loads(reader.readline()) == {"Input": {"bytes": [122]}}
                os.write(master, b"\x02d")
                deadline = time.monotonic() + 5
                while process.poll() is None:
                    assert time.monotonic() < deadline, bytes(transcript)
                    drain()
                reader.close()

        assert attached(env, ["--socket", str(endpoint)], rejected_action) == 0

print("TUI smoke passed: real PTY attach/detach, transport failure, recoverable action error, restored terminal modes and cursor")
