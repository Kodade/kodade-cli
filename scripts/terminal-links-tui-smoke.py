#!/usr/bin/env python3
"""Real daemon PTY + TUI proof for terminal links and synchronized output."""
import errno, fcntl, json, os
from pathlib import Path
import pty, select, socket, struct, subprocess, sys, tempfile, termios, time

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
OLD_URI = "https://example.test/osc8?exact=history"
FRESH_URI = "https://example.test/osc8?exact=fresh"
SESSION = "osc8"


def wait_for(label, predicate, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.03)
    raise RuntimeError(f"timed out waiting for {label}")


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="kodade-osc8-") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=directory, XDG_CONFIG_HOME=str(root / ".config"), XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh", TERM="xterm-256color")
    config_dir = root / ".config" / "kodade-cli"
    config_dir.mkdir(parents=True)
    opened, handler = root / "opened-uri", root / "record-uri"
    clear, fresh, overwrite = root / "clear", root / "fresh", root / "overwrite"
    sync, started, release, completed = root / "sync", root / "started", root / "release", root / "completed"
    handler.write_text(f"#!/bin/sh\nprintf '%s' \"$1\" > '{opened}'\n")
    handler.chmod(0o700)
    (config_dir / "config.toml").write_text(f"sidebar = false\n[ui]\nlink_command = \"{handler}\"\n")
    (config_dir / "state").write_text("help_seen = true\n")

    def command(*args):
        return subprocess.run([str(BINARY), "--session", SESSION, *args], env=env, check=True, capture_output=True, text=True)

    def pane_id():
        panes = json.loads(command("pane", "ls", "--json").stdout)
        return next(pane["id"] for pane in panes if pane["focused"])

    def screen():
        client = socket.socket(socket.AF_UNIX)
        client.connect(str(root / "run" / "kodade-cli" / f"{SESSION}.sock"))
        with client:
            client.sendall((json.dumps({"Query": {"Pane": pane_id()}}) + "\n").encode())
            reply = bytearray()
            while not reply.endswith(b"\n"):
                reply.extend(client.recv(65536))
        return json.loads(reply)["Pane"]["screen"]

    try:
        command("new", "--workspace", "main", str(root))
        emitter = f"""
sleep 1
printf '\\033]8;;{OLD_URI}\\033\\\\docs\\033]8;;\\033\\\\\\r\\n'
i=0; while [ $i -lt 40 ]; do printf 'line-%s\\r\\n' \"$i\"; i=$((i+1)); done
while [ ! -f '{clear}' ]; do sleep .02; done
printf '\\033[2J\\033[H\\033]8;;{FRESH_URI}\\033\\\\docs\\033]8;;\\033\\\\'
: > '{fresh}'
while [ ! -f '{overwrite}' ]; do sleep .02; done
printf '\\033[Hdocs'
: > '{overwrite}.done'
while [ ! -f '{sync}' ]; do sleep .02; done
printf '\\033[?2026hPARTIAL'
: > '{started}'
while [ ! -f '{release}' ]; do sleep .02; done
printf ' FINAL\\033[?2026l'
: > '{completed}'
sleep 10
"""
        command("run", "--", "sh", "-c", emitter)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        tui = subprocess.Popen([str(BINARY), "--session", SESSION], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=controlling_terminal)
        transcript = bytearray()

        def drain():
            while select.select([master], [], [], 0)[0]:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        return
                    raise
                if not chunk:
                    return
                transcript.extend(chunk)

        def ctrl_click_link(expect_open):
            for row in range(3, 5):
                for column in range(4, 16):
                    os.write(master, f"\x1b[<16;{column};{row}M".encode())
                    drain()
                    time.sleep(.04)
                    if opened.exists():
                        return
            if expect_open:
                raise RuntimeError("Ctrl-click did not reach a visible OSC 8 link")

        try:
            wait_for("TUI attach", lambda: (drain() is None) and b"?2004h" in transcript)

            def frame_presented_before_next_draw():
                drain()
                if b"osc8" not in transcript:
                    return False
                # A buffered END can appear immediately before the next BEGIN
                # forever. Foot needs synchronization released between draws.
                if transcript.rfind(b"\x1b[?2026l") <= transcript.rfind(b"\x1b[?2026h"):
                    return False
                return not select.select([master], [], [], .005)[0]

            wait_for("frame presented before next draw", frame_presented_before_next_draw, timeout=3)
            wait_for("daemon PTY scrollback", lambda: command("pane", "read", str(pane_id()), "--scrollback").stdout.startswith("docs"))
            for _ in range(12):
                os.write(master, b"\x1b[<68;5;3M")
                time.sleep(.04)
            wait_for("scrolled OSC 8 label", lambda: (drain() is None) and b"docs" in transcript)
            ctrl_click_link(True)
            wait_for("historical OSC 8 handler", opened.exists)
            assert opened.read_text() == OLD_URI

            clear.touch()
            wait_for("fresh link emitter", fresh.exists)
            for _ in range(12):
                os.write(master, b"\x1b[<69;5;3M")
                time.sleep(.04)
            wait_for("fresh live OSC 8 link", lambda: screen()["links"] == [{"row": 0, "start_col": 0, "end_col": 4, "uri": FRESH_URI}])
            opened.unlink()
            ctrl_click_link(True)
            wait_for("fresh OSC 8 handler", opened.exists)
            assert opened.read_text() == FRESH_URI

            overwrite.touch()
            wait_for("same-text overwrite", lambda: overwrite.with_name("overwrite.done").exists())
            wait_for("retired OSC 8 ranges", lambda: screen().get("links", []) == [])
            opened.unlink()
            ctrl_click_link(False)
            time.sleep(.2)
            assert not opened.exists(), "Ctrl-click recreated a retired OSC 8 target"

            baseline = screen()["contents"]
            sync.touch()
            wait_for("child synchronized-output begin", started.exists)
            before_partial = len(transcript)
            time.sleep(.3)
            drain()
            assert b"PARTIAL" not in transcript[before_partial:]
            assert screen()["contents"] == baseline

            release.touch()
            wait_for("child synchronized-output end", completed.exists)
            wait_for("completed synchronized frame", lambda: "PARTIAL FINAL" in screen()["contents"])
            wait_for("TUI completed synchronized frame", lambda: (drain() is None) and b"PARTIAL" in transcript[before_partial:] and b"FINAL" in transcript[before_partial:])

            os.write(master, b"\x02d")
            # Keep consuming the host PTY while cleanup writes its final mode
            # restores; macOS can otherwise block process exit on queued output.
            wait_for("TUI detach", lambda: (drain() is None) and tui.poll() is not None)
            assert tui.wait(timeout=5) == 0
        finally:
            if tui.poll() is None:
                tui.kill()
                os.close(master)
                master = None
                tui.wait(timeout=3)
            if master is not None:
                os.close(master)
            os.close(slave)
    finally:
        subprocess.run([str(BINARY), "--session", SESSION, "kill-session"], env=env, capture_output=True)

print("Terminal daemon PTY/TUI smoke passed: historical/live OSC 8 clicks, link retirement, and child synchronized frames")
