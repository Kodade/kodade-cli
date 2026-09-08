#!/usr/bin/env python3
"""Real daemon PTY + TUI Ctrl-click proof for a labeled OSC 8 link."""
import fcntl, json, os
from pathlib import Path
import pty, select, struct, subprocess, sys, tempfile, termios, time

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
URI = "https://example.test/osc8?exact=uri"
SESSION = "osc8"

def wait_for(label, predicate):
    deadline = time.monotonic() + 10
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
    opened, handler = root / "opened-uri", root / "record-uri"
    handler.write_text(f"#!/bin/sh\nprintf '%s' \"$1\" > '{opened}'\n"); handler.chmod(0o700)
    (config_dir / "config.toml").write_text(f"sidebar = false\n[ui]\nlink_command = \"{handler}\"\n")
    (config_dir / "state").write_text("help_seen = true\n")
    try:
        subprocess.run([str(BINARY), "--session", SESSION, "new", "--workspace", "main", str(root)], env=env, check=True, capture_output=True)
        subprocess.run([
            str(BINARY), "--session", SESSION, "run", "--", "sh", "-c",
            f"sleep 2; printf '\\033]8;;{URI}\\033\\\\docs\\033]8;;\\033\\\\\\r\\n'; i=0; while [ $i -lt 40 ]; do printf 'line-%s\\r\\n' \"$i\"; i=$((i+1)); done; sleep 15",
        ], env=env, check=True, capture_output=True)
        master, slave = pty.openpty(); fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        tui = subprocess.Popen([str(BINARY), "--session", SESSION], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=controlling_terminal)
        transcript = bytearray()
        def drain():
            while select.select([master], [], [], 0)[0]: transcript.extend(os.read(master, 65536))
        try:
            wait_for("TUI attach", lambda: (drain() is None) and b"?2004h" in transcript)
            def daemon_has_scrollback():
                panes = json.loads(subprocess.run([str(BINARY), "--session", SESSION, "pane", "ls", "--json"], env=env, check=True, capture_output=True, text=True).stdout)
                pane = next(pane for pane in panes if pane["focused"])
                text = subprocess.run([str(BINARY), "--session", SESSION, "pane", "read", str(pane["id"]), "--scrollback"], env=env, check=True, capture_output=True, text=True).stdout
                return text.startswith("docs")
            wait_for("daemon PTY scrollback", daemon_has_scrollback)
            # Shift-wheel opens local history. The link is no longer live, so
            # this also proves the daemon carries the original URI with it.
            for _ in range(12):
                os.write(master, b"\x1b[<68;5;3M")
                time.sleep(.05)
            wait_for("scrolled OSC 8 label", lambda: (drain() is None) and b"docs" in transcript)
            assert b"\x1b[?2026h" in transcript and b"\x1b[?2026l" in transcript
            # Pane geometry varies with terminal capabilities. Probe visible pane cells;
            # the handler proves the single OSC 8 target received its exact URI.
            for row in range(3, 4):
                for column in range(4, 13):
                    os.write(master, f"\x1b[<16;{column};{row}M".encode())
                    drain()
                    time.sleep(.05)
                    if opened.exists(): break
                if opened.exists(): break
            wait_for("OSC 8 handler", opened.exists)
            assert opened.read_text() == URI
            os.write(master, b"\x02d"); assert tui.wait(timeout=5) == 0
        finally:
            if tui.poll() is None: tui.kill(); tui.wait(timeout=3)
            os.close(master); os.close(slave)
    finally:
        subprocess.run([str(BINARY), "--session", SESSION, "kill-session"], env=env, capture_output=True)

print("OSC 8 daemon PTY/TUI smoke passed: labeled link opened its exact URI through the configured handler")
