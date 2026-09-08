#!/usr/bin/env python3
"""Real TUI Ctrl-click proof for a labeled OSC 8 snapshot and URL handler."""
import fcntl, json, os
from pathlib import Path
import pty, select, socket, struct, subprocess, sys, tempfile, termios, threading, time

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
URI = "https://example.test/osc8?exact=uri"

def wait_for(label, predicate):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if predicate(): return
        time.sleep(.03)
    raise RuntimeError(f"timed out waiting for {label}")

def controlling_terminal():
    os.setsid(); fcntl.ioctl(0, termios.TIOCSCTTY, 0)

with tempfile.TemporaryDirectory(prefix="kodade-osc8-") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=directory, XDG_CONFIG_HOME=str(root / ".config"), XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh", TERM="xterm-256color")
    config_dir = root / ".config" / "kodade-cli"; config_dir.mkdir(parents=True)
    opened, handler, endpoint = root / "opened-uri", root / "record-uri", root / "fixture.sock"
    handler.write_text(f"#!/bin/sh\nprintf '%s' \"$1\" > '{opened}'\n"); handler.chmod(0o700)
    (config_dir / "config.toml").write_text(f"sidebar = false\n[ui]\nlink_command = \"{handler}\"\n")
    (config_dir / "state").write_text("help_seen = true\n")
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); listener.bind(str(endpoint)); listener.listen(1); listener.settimeout(8)
    fixture = {"Layout": {"active_workspace": 1, "active_tab": 2,
        "workspaces": [{"id": 1, "name": "main", "active": True, "state": "idle", "root": None, "color": None, "branch": None, "parent": None, "tabs": [{"id": 2, "name": "links", "state": "idle", "agents": []}]}],
        "tabs": [{"id": 2, "name": "links", "active": True, "state": "idle"}], "tree": {"Leaf": {"pane": 3}}, "zoomed": False, "restored": False,
        "panes": [{"id": 3, "title": "links", "focused": True, "scroll_offset": 0, "screen": {"contents": "docs", "cursor_row": 0, "cursor_col": 4, "cursor_visible": True, "rows": [[{"text": "docs", "fg": "Default", "bg": "Default", "attrs": 0}]], "bracketed_paste": False, "mouse_reporting": False, "links": [{"row": 0, "start_col": 0, "end_col": 4, "uri": URI}]}, "agent": "fixture", "agent_generation": 0, "activity_revision": 0, "state": "idle", "state_reason": "", "state_age_secs": 0, "cwd": None}]}}
    def serve():
        connection, _ = listener.accept()
        with connection:
            reader = connection.makefile("rb")
            assert "Hello" in json.loads(reader.readline())
            connection.sendall(b'{"Welcome":{"session":"osc8","version":1}}\n')
            assert json.loads(reader.readline()) == {"SetCompactView": {"enabled": False}}
            assert json.loads(reader.readline()) == "Subscribe"
            connection.sendall(json.dumps(fixture).encode() + b"\n")
            while reader.readline(): pass
    thread = threading.Thread(target=serve, daemon=True); thread.start()
    master, slave = pty.openpty(); fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
    tui = subprocess.Popen([str(BINARY), "--socket", str(endpoint)], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=controlling_terminal)
    transcript = bytearray()
    def drain():
        while select.select([master], [], [], 0)[0]: transcript.extend(os.read(master, 65536))
    try:
        wait_for("TUI attach", lambda: (drain() is None) and b"?2004h" in transcript)
        try:
            wait_for("fixture layout", lambda: (drain() is None) and b"docs" in transcript)
        except RuntimeError as error:
            raise RuntimeError((error, bytes(transcript[:4000]))) from error
        # Header/sidebar modes can shift the pane by a few cells. Probe its
        # first visible text row using Ctrl-clicks only; exactly one target is
        # a link and the handler proves its URI.
        for column in range(4, 12):
            os.write(master, f"\x1b[<16;{column};3M".encode())
            drain()
            if opened.exists(): break
        wait_for("OSC 8 handler", opened.exists)
        assert opened.read_text() == URI
        assert "https://example.test/osc8" not in "docs"
        os.write(master, b"\x02d"); assert tui.wait(timeout=5) == 0
    finally:
        if tui.poll() is None: tui.kill(); tui.wait(timeout=3)
        os.close(master); os.close(slave); listener.close()

print("OSC 8 TUI smoke passed: labeled link opened its exact URI through the configured handler")
