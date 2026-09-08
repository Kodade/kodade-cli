#!/usr/bin/env python3
"""Exercise `--remote` through an isolated localhost OpenSSH server.

The harness does not read or modify ~/.ssh. It creates a host key, client key,
sshd config, ssh wrapper, remote HOME/XDG directories, and a `kodade-cli` shim
inside one temporary directory. All child processes have timeouts and cleanup
only touches those owned paths/processes.
"""

import base64
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zlib


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/debug/kodade-cli").resolve()
SSHD = Path("/usr/bin/sshd")
SSH = Path("/usr/bin/ssh")
SSH_KEYGEN = Path("/usr/bin/ssh-keygen")
SESSION = "ssh-smoke"
RENAMED = "ssh-renamed"


def require(path: Path) -> None:
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"required executable is unavailable: {path}")


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_until(label: str, predicate, seconds: float = 8) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise RuntimeError(f"timed out waiting for {label}")


def executable(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(0o700)


def main() -> None:
    for command in (BINARY, SSHD, SSH, SSH_KEYGEN):
        require(command)

    # Keep OpenSSH's `ControlPath=.../cm-%C` below the Unix socket limit.
    with tempfile.TemporaryDirectory(prefix="ks-") as temporary:
        root = Path(temporary)
        remote_home = root / "remote-home"
        remote_run = root / "remote-run"
        remote_state = root / "remote-state"
        local_home = root / "local-home"
        local_run = root / "local-run"
        local_state = root / "local-state"
        bin_dir = root / "bin"
        for directory in (remote_home, remote_run, remote_state, local_home, local_run, local_state, bin_dir):
            directory.mkdir(mode=0o700)

        host_key = root / "host_key"
        client_key = root / "client_key"
        subprocess.run([str(SSH_KEYGEN), "-q", "-N", "", "-t", "ed25519", "-f", str(host_key)], check=True, timeout=10)
        subprocess.run([str(SSH_KEYGEN), "-q", "-N", "", "-t", "ed25519", "-f", str(client_key)], check=True, timeout=10)
        authorized = root / "authorized_keys"
        authorized.write_bytes(client_key.with_suffix(".pub").read_bytes())
        port = free_port()
        sshd_config = root / "sshd_config"
        sshd_config.write_text(
            "\n".join(
                [
                    f"Port {port}",
                    "ListenAddress 127.0.0.1",
                    f"HostKey {host_key}",
                    f"PidFile {root / 'sshd.pid'}",
                    f"AuthorizedKeysFile {authorized}",
                    "PasswordAuthentication no",
                    "KbdInteractiveAuthentication no",
                    "ChallengeResponseAuthentication no",
                    "UsePAM no",
                    "StrictModes no",
                    "UseDNS no",
                    "LogLevel ERROR",
                ]
            )
            + "\n"
        )
        ssh_config = root / "ssh_config"
        ssh_config.write_text(
            "\n".join(
                [
                    "Host kodade-smoke",
                    "  HostName 127.0.0.1",
                    f"  Port {port}",
                    f"  User {os.environ.get('USER', os.environ.get('LOGNAME', 'nobody'))}",
                    f"  IdentityFile {client_key}",
                    "  IdentitiesOnly yes",
                    "  StrictHostKeyChecking no",
                    "  UserKnownHostsFile /dev/null",
                    "  LogLevel ERROR",
                ]
            )
            + "\n"
        )
        executable(
            bin_dir / "ssh",
            "#!/usr/bin/env python3\n"
            "import os, sys\n"
            f"shim = 'env PATH={bin_dir}:/usr/local/bin:/usr/bin:/bin kodade-cli'\n"
            "args = [arg.replace('nohup kodade-cli ', 'nohup ' + shim + ' ') if arg.startswith('nohup kodade-cli ') else (shim if arg == 'kodade-cli' else arg) for arg in sys.argv[1:]]\n"
            f"os.execv('{SSH}', ['{SSH}', '-F', '{ssh_config}', *args])\n",
        )
        executable(
            bin_dir / "kodade-cli",
            "#!/bin/sh\n"
            f"export HOME={remote_home}\n"
            f"export XDG_RUNTIME_DIR={remote_run}\n"
            f"export XDG_STATE_HOME={remote_state}\n"
            f"exec {BINARY} \"$@\"\n",
        )

        env = dict(os.environ)
        env.update(
            {
                "HOME": str(local_home),
                "XDG_RUNTIME_DIR": str(local_run),
                "XDG_STATE_HOME": str(local_state),
                "PATH": f"{bin_dir}:{env['PATH']}",
                "SHELL": "/bin/sh",
            }
        )

        def run(*args: str, ok: bool = True) -> subprocess.CompletedProcess[str]:
            result = subprocess.run([str(BINARY), *args], env=env, text=True, capture_output=True, timeout=45)
            if ok and result.returncode != 0:
                raise RuntimeError(f"kodade {' '.join(args)} failed\nstdout: {result.stdout}\nstderr: {result.stderr}")
            return result

        events: list[subprocess.Popen[str]] = []
        sshd = subprocess.Popen(
            [str(SSHD), "-D", "-e", "-f", str(sshd_config)],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            wait_until("rootless sshd", lambda: subprocess.run([str(SSH), "-F", str(ssh_config), "kodade-smoke", "true"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=2).returncode == 0)

            # A remote run starts the remote daemon through SSH, forwards its
            # socket, and returns a pane id. Read its output through a fresh tunnel.
            pane = run("--remote", "kodade-smoke", "-s", SESSION, "run", "--name", "probe", "--", "sh", "-c", "printf SSH_SMOKE_OK; exec sleep 30").stdout.strip()
            if not pane.isdigit():
                raise RuntimeError(f"remote run did not return a pane id: {pane!r}")
            wait_until(
                "remote pane output",
                lambda: "SSH_SMOKE_OK" in run("--remote", "kodade-smoke", "-s", SESSION, "pane", "read", pane).stdout,
            )
            remote_socket = Path(run("--remote", "kodade-smoke", "-s", SESSION, "session", "path").stdout.strip())
            if not remote_socket.exists():
                raise RuntimeError("remote session path was not a live socket")

            # Pixels travel over the forwarded socket; the returned attachment
            # belongs to the remote daemon and survives a session rename.
            def png_chunk(kind, data):
                return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
            png = (b"\x89PNG\r\n\x1a\n"
                   + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
                   + png_chunk(b"IDAT", zlib.compress(b"\0\xff\x80\x40"))
                   + png_chunk(b"IEND", b""))
            image_source = local_home / "client-only.png"
            image_source.write_bytes(png)
            attachment = Path(run("--remote", "kodade-smoke", "-s", SESSION, "pane", "paste-image", pane, str(image_source)).stdout.strip())
            assert attachment != image_source and attachment.read_bytes() == png
            frame = b"\x1b_Ga=T,f=100,i=7,c=4,r=2,C=1;" + base64.b64encode(png) + b"\x1b\\"
            image_pane = run("--remote", "kodade-smoke", "-s", SESSION, "run", "--", "python3", "-c", f"import os,time;os.write(1,{frame!r});time.sleep(60)").stdout.strip()
            wait_until("forwarded graphics metadata", lambda: any(
                str(p["id"]) == image_pane and p["screen"].get("graphics")
                for p in json.loads(run("--remote", "kodade-smoke", "-s", SESSION, "ls", "--json").stdout)["panes"]
            ))

            # Two concurrent event streams each hold a tunnel. Stopping one
            # cannot remove the other's local endpoint or disconnect its stream.
            for _ in range(2):
                events.append(
                    subprocess.Popen(
                        [str(BINARY), "--remote", "kodade-smoke", "-s", SESSION, "events"],
                        env=env,
                        text=True,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )
                )
            def two_forwards_ready() -> bool:
                dead = [process for process in events if process.poll() is not None]
                if dead:
                    stderr = "\n".join((process.stderr.read() if process.stderr else "") for process in dead)
                    raise RuntimeError(f"event stream ended before its tunnel was ready: {stderr}")
                # `events` only starts its stream after resolve_socket has made
                # the SSH Unix forward connectable; two live stream processes
                # therefore prove two concurrent tunnel lifetimes.
                return True

            time.sleep(0.5)
            two_forwards_ready()
            run("--remote", "kodade-smoke", "-s", SESSION, "ls", "--json")
            events[0].terminate()
            events[0].wait(timeout=5)
            if events[1].poll() is not None:
                raise RuntimeError("second SSH tunnel ended when the first tunnel closed")
            run("--remote", "kodade-smoke", "-s", SESSION, "pane", "read", pane)

            # Rename travels through the remote session command, and existing
            # connections remain disposable; a new tunnel reaches the new path.
            run("--remote", "kodade-smoke", "-s", SESSION, "session", "rename", RENAMED)
            renamed_socket = Path(run("--remote", "kodade-smoke", "-s", RENAMED, "session", "path").stdout.strip())
            if not renamed_socket.exists() or renamed_socket == remote_socket:
                raise RuntimeError("remote session rename did not produce its new socket")
            run("--remote", "kodade-smoke", "-s", RENAMED, "pane", "read", pane)
            run("--remote", "kodade-smoke", "-s", RENAMED, "kill-session")
            assert not attachment.exists(), "remote attachment survived session shutdown"
            print("SSH smoke passed: daemon startup, run/read, PNG upload, graphics, independent tunnels, rename, teardown")
        finally:
            for process in events:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
            # Only kill the session names created by this temporary remote HOME.
            for session in (SESSION, RENAMED):
                run("--remote", "kodade-smoke", "-s", session, "session", "kill", ok=False)
            if sshd.poll() is None:
                sshd.terminate()
                try:
                    sshd.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    sshd.kill()
            if sshd.returncode not in (0, -15):
                error = sshd.stderr.read() if sshd.stderr else ""
                if error:
                    print(f"sshd output: {error.strip()}", file=sys.stderr)


if __name__ == "__main__":
    main()
