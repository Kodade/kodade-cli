#!/usr/bin/env python3
"""Prove enhanced keyboard input through a real controlling-TTY TUI."""
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time


BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
SESSION = "keyboard-tui"


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


with tempfile.TemporaryDirectory(prefix="kodade-keyboard-") as directory:
    root = Path(directory)
    # macOS limits Unix-domain socket paths to 104 bytes; its default temporary
    # directory is already long enough to exceed that once the session socket
    # suffix is added.
    runtime = Path(tempfile.mkdtemp(prefix="kodade-kb-", dir="/tmp"))
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=directory, XDG_CONFIG_HOME=str(root / ".config"),
               XDG_RUNTIME_DIR=str(runtime), XDG_STATE_HOME=str(root / "state"),
               SHELL="/bin/sh", TERM="xterm-256color")
    config = root / ".config" / "kodade-cli"
    config.mkdir(parents=True)
    (config / "config.toml").write_text("sidebar = false\n")
    (config / "state").write_text("help_seen = true\n")

    def command(*args):
        return subprocess.run([str(BINARY), "--session", SESSION, *args], env=env,
                              check=True, capture_output=True, text=True)

    def panes():
        return json.loads(command("pane", "ls", "--json").stdout)

    # The recorder is deliberately a real child PTY program: it sets /dev/tty
    # raw, negotiates Kitty flags, and publishes both the exact bytes and hex
    # outside the terminal.  This avoids testing daemon Input messages directly.
    def recorder(name, negotiate=True):
        raw, hexed, ready, stop = (root / f"{name}.raw", root / f"{name}.hex",
                                   root / f"{name}.ready", root / "stop")
        source = f"""
import os, select, termios, tty
raw = {str(raw)!r}; hexed = {str(hexed)!r}; ready = {str(ready)!r}; stop = {str(stop)!r}
frame = {str(root / f"{name}.frame")!r}
fd = os.open('/dev/tty', os.O_RDWR)
old = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    {"os.write(fd, b'\\x1b[>3u')" if negotiate else "pass"}
    open(ready, 'w').close()
    captured = bytearray()
    while not os.path.exists(stop):
        if os.path.exists(frame):
            with open(frame, 'rb') as marker:
                os.write(fd, b'\\r\\n' + marker.read() + b'\\r\\n')
            os.unlink(frame)
        readable, _, _ = select.select([fd], [], [], .05)
        if readable:
            captured.extend(os.read(fd, 4096))
            open(raw, 'wb').write(captured)
            open(hexed, 'w').write(captured.hex())
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
"""
        return [sys.executable, "-c", source], raw, hexed, ready

    def raw(path):
        return path.read_bytes() if path.exists() else b""

    def expect_bytes(label, path, expected, context=None, progress=None):
        hex_path = path.with_suffix(".hex")

        def received():
            if progress is not None:
                progress()
            # The recorder publishes the raw and hex views in separate writes.
            # Wait until both report the same complete capture instead of
            # accepting raw bytes while its matching hex write is still racing.
            return raw(path) == expected and hex_path.exists() and hex_path.read_text() == expected.hex()

        try:
            wait_for(label, received)
        except RuntimeError as error:
            details = f"{error}: got {raw(path).hex()}, expected {expected.hex()}"
            if context is not None:
                details += f"; {context()}"
            raise RuntimeError(details) from error

    master = slave = None
    tui = None
    try:
        command("new", "--workspace", "main", str(root))
        first_cmd, first_raw, first_hex, first_ready = recorder("first")
        # The first pane starts as a shell; replace it with the persistent child.
        first = next(pane for pane in panes() if pane["focused"])
        command("send", str(first["id"]), "exec " + shlex.join(first_cmd))
        wait_for("first recorder ready", first_ready.exists)
        wait_for("first Kitty negotiation", lambda: next(p for p in panes() if p["id"] == first["id"])["screen"]["keyboard"]["kitty_flags"] == 3)

        second_cmd, second_raw, second_hex, second_ready = recorder("second")
        second_id = int(command("split", "--", *second_cmd).stdout.strip())
        wait_for("second recorder ready", second_ready.exists)
        wait_for("second Kitty negotiation", lambda: next(p for p in panes() if p["id"] == second_id)["screen"]["keyboard"]["kitty_flags"] == 3)
        command("pane", "focus", str(first["id"]))

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        original = termios.tcgetattr(slave)
        tui = subprocess.Popen([str(BINARY), "--session", SESSION], stdin=slave, stdout=slave,
                               stderr=slave, env=env, preexec_fn=controlling_terminal)
        transcript = bytearray()
        replied = [False]

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
                # Crossterm follows Kitty's documented detection handshake.
                if not replied[0] and b"\x1b[?u\x1b[c" in transcript:
                    os.write(master, b"\x1b[?0u\x1b[?1;2c")
                    replied[0] = True

        def host_key(sequence):
            os.write(master, sequence)
            drain()

        def keyboard_context():
            try:
                state = [
                    {
                        "id": pane["id"],
                        "focused": pane["focused"],
                        "keyboard": pane["screen"]["keyboard"],
                    }
                    for pane in panes()
                ]
            except Exception as error:
                state = f"pane query failed: {error}"
            return f"transcript tail={transcript[-1024:].hex()}, panes={state}"

        wait_for("Crossterm keyboard query", lambda: (drain() is None) and replied[0])
        wait_for("Crossterm enhanced push", lambda: (drain() is None) and b"\x1b[>11u" in transcript)
        wait_for("TUI attach", lambda: (drain() is None) and b"?2004h" in transcript)
        # TerminalModes enters raw mode before App::run receives the opening
        # daemon layout. On macOS the first host input can arrive in that gap.
        # The rendered status bar proves the client has both attached and drawn
        # the session it is about to drive.
        wait_for("initial TUI frame", lambda: (drain() is None) and b"keyboard-tui \xc2\xb7 main" in transcript)

        # Ctrl character, repeat, and release cross host -> TUI -> daemon -> real child once.
        host_key(b"\x1b[99;5u")
        host_key(b"\x1b[99;5:2u")
        host_key(b"\x1b[99;5:3u")
        expect_bytes("Ctrl-c press/repeat/release", first_raw,
                     b"\x1b[99;5u\x1b[99;5:2u\x1b[99;5:3u", keyboard_context, drain)

        # Unmodified text and Shift+Enter retain their documented flag-3 behavior:
        # text is plain, neither text nor Enter gets a release without flag 8.
        host_key(b"\x1b[120;1u")
        host_key(b"\x1b[120;1:3u")
        host_key(b"\x1b[13;2u")
        host_key(b"\x1b[13;2:3u")
        expect_bytes("plain text and Shift-Enter", first_raw,
                     b"\x1b[99;5u\x1b[99;5:2u\x1b[99;5:3ux\x1b[13;2u", progress=drain)

        # A press belongs to its original pane even when an actual mouse click
        # switches focus before the release arrives.
        host_key(b"\x1b[1;5A")
        host_key(b"\x1b[1;5:2A")
        for row in range(3, 29, 3):
            for column in range(5, 120, 5):
                os.write(master, f"\x1b[<0;{column};{row}M".encode())
                time.sleep(.03)
                drain()
            # Client focus is private to this TUI connection, so `pane ls`
            # cannot observe it.  The following child-byte assertion does.
        host_key(b"\x1b[1;5:3A")
        expect_bytes("arrow release stays with original pane", first_raw,
                     b"\x1b[99;5u\x1b[99;5:2u\x1b[99;5:3ux\x1b[13;2u\x1b[1;5A\x1b[1;5:2A\x1b[1;5:3A",
                     progress=drain)
        assert raw(second_raw) == b"", "focus switch leaked the prior key release into the new pane"

        host_key(b"\x1b[113;5u")
        host_key(b"\x1b[113;5:3u")
        expect_bytes("actual mouse focus routes new key to second pane", second_raw,
                     b"\x1b[113;5u\x1b[113;5:3u", progress=drain)

        # Prefix commands and their modal keys are consumed. Their releases must
        # not become late PTY input after the mode changes.
        host_key(b"\x02")
        host_key(b"\x1b[114;1u")  # prefix+r: rename
        host_key(b"\x1b[114;1:3u")
        host_key(b"\x1b")         # cancel rename
        host_key(b"\x02")
        host_key(b"\x1b[32;1u")   # prefix+space: command palette
        host_key(b"\x1b[32;1:3u")
        host_key(b"\x1b")         # close palette
        time.sleep(.15)
        drain()
        assert raw(second_raw) == b"\x1b[113;5u\x1b[113;5:3u", "consumed prefix/palette/rename key release reached pane"

        # Foot reports Num Lock (128) and Caps Lock (64) in Kitty modifier
        # fields. Neither state may disable the prefix or its next command.
        for lock in (128, 64, 192):
            before_palette = len(transcript)
            host_key(f"\x1b[98;{5 + lock}u".encode())
            host_key(f"\x1b[98;{5 + lock}:3u".encode())
            host_key(f"\x1b[32;{1 + lock}u".encode())
            host_key(f"\x1b[32;{1 + lock}:3u".encode())
            wait_for(
                f"command palette with lock state {lock}",
                lambda: (drain() is None) and b"command center" in transcript[before_palette:],
            )
            host_key(f"\x1b[27;{1 + lock}u".encode())
            host_key(f"\x1b[27;{1 + lock}:3u".encode())
        time.sleep(.15)
        drain()
        assert raw(second_raw) == b"\x1b[113;5u\x1b[113;5:3u", "lock state disabled prefix/palette consumption"

        # A live upgrade must keep each pane's negotiated keyboard protocol.
        # Keep this attached real TTY through two replacement daemons, then
        # verify Shift+Enter still reaches the original focused child verbatim.
        for generation in (1, 2):
            command("session", "upgrade")
            wait_for(
                f"Kitty mode after upgrade {generation}",
                lambda: next(p for p in panes() if p["id"] == second_id)["screen"]["keyboard"]["kitty_flags"] == 3,
            )
            # Daemon readiness does not prove this attached client has
            # reconnected. A new child-output row must reach the actual TUI
            # before sending input, which is intentionally dropped offline.
            marker = f"UPGRADE-READY-{generation}".encode()
            (root / "second.frame").write_bytes(marker)
            wait_for(
                f"attached frame after upgrade {generation}",
                lambda: (drain() is None) and marker in transcript,
            )
        host_key(b"\x1b[13;2u")
        expect_bytes("Shift-Enter after two live upgrades", second_raw,
                     b"\x1b[113;5u\x1b[113;5:3u\x1b[13;2u", keyboard_context, drain)

        # A non-negotiating child still gets legacy bytes but never host releases.
        legacy_cmd, legacy_raw, legacy_hex, legacy_ready = recorder("legacy", negotiate=False)
        legacy_id = int(command("split", "--", *legacy_cmd).stdout.strip())
        wait_for("legacy recorder ready", legacy_ready.exists)
        # Focus belongs to this client, so use the same actual mouse path and
        # prove which pane received the probe rather than relying on a CLI view.
        for row in range(3, 29, 3):
            for column in range(5, 120, 5):
                os.write(master, f"\x1b[<0;{column};{row}M".encode())
                time.sleep(.02)
                host_key(b"\x1b[122;1u")
                host_key(b"\x1b[122;1:3u")
                try:
                    wait_for("legacy probe delivery", lambda: bool(raw(legacy_raw)), timeout=.2)
                    break
                except RuntimeError:
                    pass
            if raw(legacy_raw):
                break
        assert raw(legacy_raw), "actual mouse clicks could not reach the legacy pane"
        legacy_bytes = raw(legacy_raw)
        assert legacy_bytes == b"z" * len(legacy_bytes), "legacy pane received a host release"
        wait_for(
            "legacy recorder hex capture",
            lambda: (drain() is None) and legacy_hex.exists() and legacy_hex.read_text() == legacy_bytes.hex(),
        )

        os.write(master, b"\x02d")
        wait_for("TUI detach", lambda: (drain() is None) and tui.poll() is not None)
        assert tui.wait(timeout=5) == 0
        drain()
        try:
            restored_modes = termios.tcgetattr(slave)
        except termios.error as error:
            # macOS detaches the slave from the ended controlling session.
            # The master remains the same PTY and still exposes its termios.
            if error.args[0] != errno.ENOTTY:
                raise
            restored_modes = termios.tcgetattr(master)
        assert restored_modes == original, "detach left host terminal raw"
        assert b"\x1b[<1u" in transcript, "detach did not pop keyboard enhancement"
    finally:
        failed = sys.exc_info()[0] is not None
        cleanup_error = None
        (root / "stop").touch()
        if tui is not None and tui.poll() is None:
            # Target only the child we own. macOS can retain a defunct process
            # group that rejects killpg even after its leader has exited.
            try:
                tui.terminate()
            except (ProcessLookupError, PermissionError):
                pass
            try:
                tui.wait(timeout=1)
            except subprocess.TimeoutExpired:
                try:
                    tui.kill()
                except (ProcessLookupError, PermissionError):
                    pass
                try:
                    tui.wait(timeout=3)
                except subprocess.TimeoutExpired as error:
                    cleanup_error = error
        if master is not None:
            os.close(master)
        if slave is not None:
            os.close(slave)
        subprocess.run([str(BINARY), "--session", SESSION, "kill-session"], env=env,
                       capture_output=True)
        shutil.rmtree(runtime, ignore_errors=True)
        if cleanup_error is not None:
            if not failed:
                raise RuntimeError("could not terminate keyboard TUI") from cleanup_error
            print("keyboard TUI cleanup timed out after TERM/KILL", file=sys.stderr)

print("Terminal keyboard TUI smoke passed: detection, real PTY forwarding, two live upgrades, focus ownership, modal consumption, legacy releases, and detach restoration")
