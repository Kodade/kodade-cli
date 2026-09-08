#!/usr/bin/env python3
"""Real narrow TUI, mouse/keyboard switching, and independent wide client."""
import fcntl
import json
import os
from pathlib import Path
import pty
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()


def wait(label, predicate):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.05)
    raise RuntimeError(f"timed out waiting for {label}")


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="kc-") as directory:
    root = Path(directory)
    env = {k: v for k, v in os.environ.items() if not k.startswith("KODADE_")}
    env.update(HOME=directory, XDG_CONFIG_HOME=str(root / ".config"),
               XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"),
               SHELL="/bin/sh", TERM="xterm-256color")
    config = root / ".config/kodade-cli/config.toml"
    config.parent.mkdir(parents=True)
    config.write_text('[sidebar]\ncompact_view = "auto"\n')
    tui = None
    master = slave = None
    wide = None
    reader = None
    transcript = bytearray()

    def cli(*args):
        return subprocess.run([str(BINARY), "-s", "compact", *args], env=env,
                              capture_output=True, text=True, timeout=12, check=True).stdout.strip()

    def send(message):
        wide.sendall(json.dumps(message).encode() + b"\n")

    def reply(kind):
        while True:
            result = json.loads(reader.readline())
            assert "Error" not in result, result
            if kind in result:
                return result[kind]

    def measure(name, sender, expected_cols, expected_pid=None):
        destination = root / name
        command = f"printf '%s ' \"$$\" > {destination}; stty size >> {destination}\n"
        sender(command.encode())
        wait(name, lambda: destination.exists() and len(destination.read_text().split()) == 3)
        pid, rows, cols = map(int, destination.read_text().split())
        assert (rows, cols) == (26, expected_cols), (name, pid, rows, cols)
        if expected_pid is not None:
            assert pid == expected_pid, (name, pid, expected_pid)
        return pid

    try:
        cli("run", "--", "sh")
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        tui = subprocess.Popen([str(BINARY), "-s", "compact"], stdin=slave,
                               stdout=slave, stderr=slave, env=env, preexec_fn=controlling_terminal)

        def drain():
            try:
                while True:
                    data = os.read(master, 65536)
                    if not data:
                        return
                    transcript.extend(data)
            except OSError:
                return

        threading.Thread(target=drain, daemon=True).start()
        wait("TUI attach", lambda: b"\x1b[?2004h" in transcript)
        time.sleep(.2)
        os.write(master, b"\x02%")
        wait("two panes", lambda: len(json.loads(cli("ls", "--json"))["panes"]) == 2)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 40, 0, 0))
        os.kill(tui.pid, signal.SIGWINCH)
        wait("compact controls", lambda: b"[Switch]" in transcript)
        time.sleep(.15)
        narrow_input = lambda data: os.write(master, data)
        first = measure("narrow-first", narrow_input, 37)
        # x=15,y=1 is the rendered [>] hit box at 40 columns.
        os.write(master, b"\x1b[<0;15;1M\x1b[<0;15;1m")
        time.sleep(.2)
        second = measure("narrow-mouse-next", narrow_input, 37)
        assert first != second, "mouse control did not change focused PTY"
        before = len(transcript)
        os.write(master, b"\x1b[<0;7;1M\x1b[<0;7;1m")
        wait("mouse switcher", lambda: b"go to" in transcript[before:])
        os.write(master, b"\x1b")
        time.sleep(.15)
        os.write(master, b"\x02o")
        time.sleep(.2)
        measure("narrow-keyboard", narrow_input, 37, first)

        # A second Hello creates a genuinely independent interactive view. It
        # sees the split and controls a wide PTY while the first stays compact.
        wide = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        wide.settimeout(5)
        wide.connect(str(root / "run/kodade-cli/compact.sock"))
        reader = wide.makefile("rb")
        send({"Hello": {"cols": 120, "rows": 30, "version": 1}})
        reply("Welcome")
        send({"SetCompactView": {"enabled": False}})
        send({"Query": "Layout"})
        layout = reply("Layout")
        assert "Split" in layout["tree"] and len(layout["panes"]) == 2
        wide_input = lambda data: send({"Input": {"bytes": list(data)}})
        wide_pid = measure("wide-client", wide_input, 58)
        assert wide_pid in (first, second)
        measure("narrow-after-wide", narrow_input, 37, first)
        send({"Query": "Layout"})
        assert "Split" in reply("Layout")["tree"], "compact client changed another view"

        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        os.kill(tui.pid, signal.SIGWINCH)
        time.sleep(.3)
        measure("restored-split", narrow_input, 46, first)
        assert len(json.loads(cli("ls", "--json"))["panes"]) == 2
        for pid in (first, second):
            os.kill(pid, 0)
        os.write(master, b"\x02d")
        assert tui.wait(timeout=5) == 0
    finally:
        if reader is not None:
            reader.close()
        if wide is not None:
            wide.close()
        if tui is not None and tui.poll() is None:
            tui.kill()
            tui.wait(timeout=5)
        for descriptor in (slave, master):
            if descriptor is not None:
                os.close(descriptor)
        subprocess.run([str(BINARY), "-s", "compact", "kill-session"], env=env,
                       capture_output=True, timeout=12)

print("Compact smoke passed: 40-column PTYs, mouse switcher, keyboard cycling, independent wide view, restored split and original processes")
