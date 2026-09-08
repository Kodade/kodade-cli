#!/usr/bin/env python3
"""Exercise real PTY graphics media and verify exact normalized image bytes."""
import base64
import ctypes
import json
import mmap
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import zlib

binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
libc = ctypes.CDLL(None, use_errno=True)
# shm_open is variadic on Darwin/arm64; declare its fixed arguments so libffi
# passes mode_t in the variadic area instead of an ordinary argument register.
libc.shm_open.argtypes = [ctypes.c_char_p, ctypes.c_int]
libc.shm_open.restype = ctypes.c_int
libc.shm_unlink.argtypes = [ctypes.c_char_p]
shm_name = f"/kodade-media-{os.getpid()}".encode()
with tempfile.TemporaryDirectory(prefix="km-") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=directory, XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh")
    def cli(*args):
        result = subprocess.run([str(binary), "-s", "media", *args], env=env, text=True, capture_output=True, timeout=15)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result.stdout.strip()
    def request(message):
        paths = list((root / "run").rglob("media.sock"))
        assert len(paths) == 1, paths
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(str(paths[0]))
            connection.sendall(json.dumps(message).encode() + b"\n")
            return json.loads(connection.makefile("rb").readline())
    pixels = bytes([255, 0, 128, 0, 255, 64])
    source = root / "pixels.rgb"
    temporary = root / "tty-graphics-protocol-pixels.rgb"
    source.write_bytes(b"prefix" + pixels + b"suffix")
    temporary.write_bytes(pixels)
    fd = libc.shm_open(shm_name, os.O_CREAT | os.O_EXCL | os.O_RDWR, ctypes.c_uint(0o600))
    assert fd >= 0, os.strerror(ctypes.get_errno())
    os.ftruncate(fd, len(pixels))
    shared = mmap.mmap(fd, len(pixels))
    shared[:] = pixels
    frames = []
    media = [("t=d", pixels), ("t=d,o=z", zlib.compress(pixels)),
             ("t=f,O=6,S=6", str(source).encode()), ("t=t", str(temporary).encode()),
             ("t=s,S=6", shm_name)]
    for image_id, (options, payload) in enumerate(media, 1):
        frames.append(f"\x1b_Ga=T,f=24,s=2,v=1,i={image_id},p=1,c=2,r=1,C=1,{options};".encode()
                      + base64.b64encode(payload) + b"\x1b\\")
    try:
        cli("new", "--workspace", "media", str(root))
        emitter = f"import os,time;os.write(1,{b''.join(frames)!r});time.sleep(60)"
        pane_id = int(cli("run", "--name", "media", "--", "python3", "-c", emitter))
        deadline = time.monotonic() + 8
        while True:
            pane = request({"Query": {"Pane": pane_id}})["Pane"]
            placements = pane["screen"].get("graphics", [])
            if len(placements) == 5:
                break
            assert time.monotonic() < deadline, pane
            time.sleep(.025)
        for placement in placements:
            response = request({"Query": {"Image": {"pane": pane_id, "id": placement["image"], "revision": placement["revision"]}}})
            image = response["Image"]["image"]
            assert image["format"] == 24 and image["width"] == 2 and image["height"] == 1, image
            assert base64.b64decode(image["data"]) == pixels, image
        assert source.exists() and not temporary.exists(), "file ownership was not preserved"
        reopened = libc.shm_open(shm_name, os.O_RDONLY, 0)
        if reopened >= 0:
            os.close(reopened)
        assert reopened == -1, "shared memory was not consumed"
    finally:
        subprocess.run([str(binary), "-s", "media", "kill-session"], env=env, capture_output=True, timeout=5)
        shared.close()
        os.close(fd)
        libc.shm_unlink(shm_name)
print("Graphics media passed: exact real PTY direct/zlib/file/range/temporary/shared-memory pixels and cleanup")
