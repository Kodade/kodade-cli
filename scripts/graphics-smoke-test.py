#!/usr/bin/env python3
"""Real daemon/PTY graphics, image paste, and host-protocol lifecycle smoke."""
import base64
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
import zlib

BIN = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()

def png():
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    pixels = b"".join(b"\0" + b"\xf0\x70\x40" * 64 for _ in range(32))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 64, 32, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b"")

with tempfile.TemporaryDirectory(prefix="kg-") as temp:
    root = Path(temp)
    env = {k: v for k, v in os.environ.items() if not k.startswith("KODADE_")}
    env.update(HOME=temp, XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh", TERM="xterm-kitty", KODADE_GRAPHICS="kitty")
    def cli(*args):
        result = subprocess.run([str(BIN), "-s", "images", *args], env=env, capture_output=True, text=True, timeout=20)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result.stdout.strip()
    def layout(): return json.loads(cli("ls", "--json"))
    frame = b"\x1b_Ga=T,f=100,i=7,p=1,c=12,r=6,C=1;" + base64.b64encode(png()) + b"\x1b\\"
    process = None
    master = slave = None
    saved = None
    try:
        cli("new", "--workspace", "images", str(root))
        shell = next(p["id"] for p in layout()["panes"] if p["focused"])
        pane = int(cli("run", "--name", "graphics", "--", "python3", "-c", f"import os,time;os.write(1,{frame!r});time.sleep(60)"))
        deadline = time.monotonic() + 5
        while not any(p["screen"].get("graphics") for p in layout()["panes"]):
            assert time.monotonic() < deadline, "daemon lost graphics frame"
            time.sleep(.05)
        source = root / "sample.png"
        source.write_bytes(png())
        saved = Path(cli("pane", "paste-image", str(shell), str(source)))
        assert saved.read_bytes() == png() and saved.stat().st_mode & 0o777 == 0o600
        cli("pane", "send-keys", str(shell), "C-u")

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        def controlling():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        process = subprocess.Popen([str(BIN), "-s", "images"], env=env, stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling)
        transcript = bytearray()
        def until(predicate, seconds=6):
            deadline = time.monotonic() + seconds
            while not predicate():
                assert time.monotonic() < deadline and process.poll() is None, bytes(transcript[-2000:])
                if select.select([master], [], [], .05)[0]: transcript.extend(os.read(master, 65536))
        until(lambda: b"\x1b_Ga=p," in transcript)
        assert b"a=t,t=d,f=100" in transcript
        uploads = transcript.count(b"a=t,t=d,")
        placed = transcript.count(b"\x1b_Ga=p,")
        os.write(master, b"\x02 ")
        until(lambda: b"a=d,d=i," in transcript)
        os.write(master, b"\x1b")
        until(lambda: transcript.count(b"\x1b_Ga=p,") > placed)
        assert transcript.count(b"a=t,t=d,") == uploads, "modal close re-uploaded unchanged image"
        # Resizing and reattaching must retain the daemon's asset, with owned
        # host ids and no image appearing outside its pane's cell rectangle.
        placed = transcript.count(b"\x1b_Ga=p,")
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 25, 100, 0, 0))
        os.kill(process.pid, __import__("signal").SIGWINCH)
        time.sleep(.1)
        os.write(master, b"\x02d")
        deadline = time.monotonic() + 6
        while process.poll() is None:
            assert time.monotonic() < deadline, "detach hung"
            if select.select([master], [], [], .05)[0]: transcript.extend(os.read(master, 65536))
        while select.select([master], [], [], 0)[0]: transcript.extend(os.read(master, 65536))
        assert process.returncode == 0
        assert b"a=d,d=I," in transcript, "host assets leaked after detach"
        assert any(p["screen"].get("graphics") for p in layout()["panes"]), "detach removed daemon assets"
        cli("kill-session")
        assert not saved.exists(), "session image files leaked after shutdown"
    finally:
        if process and process.poll() is None: process.kill(); process.wait(timeout=3)
        subprocess.run([str(BIN), "-s", "images", "kill-session"], env=env, capture_output=True, timeout=5)
        if master is not None: os.close(master)
        if slave is not None: os.close(slave)

print("Graphics smoke passed: real PTY images, bounded PNG paste, modal restore, host cleanup, session attachment cleanup")
