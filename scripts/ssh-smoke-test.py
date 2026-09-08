#!/usr/bin/env python3
"""Exercise `--remote` through an isolated localhost OpenSSH server.

The harness does not read or modify ~/.ssh. It creates a host key, client key,
sshd config, ssh wrapper, remote HOME/XDG directories, and a `kodade-cli` shim
inside one temporary directory. All child processes have timeouts and cleanup
only touches those owned paths/processes.
"""

import json
import os
import pty
import fcntl
import termios
import threading
import struct
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


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

        def pane_output(*args: str) -> str:
            """Transient tunnel reconnects are expected while TUI workers attach."""
            return run(*args, ok=False).stdout

        events: list[subprocess.Popen[str]] = []
        tui: subprocess.Popen[bytes] | None = None
        tui_drain_stop: threading.Event | None = None
        sshd = subprocess.Popen(
            [str(SSHD), "-D", "-e", "-f", str(sshd_config)],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            wait_until("rootless sshd", lambda: subprocess.run([str(SSH), "-F", str(ssh_config), "kodade-smoke", "true"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=2).returncode == 0)

            # Saved machines are client-local profiles. The normal interactive
            # attach starts these in independent workers; the catalog itself
            # must never touch the remote host or the local session socket.
            machine = run("machine", "add", "kodade-smoke", "--label", "Smoke host", "--remote-session", SESSION).stdout.strip()
            if not machine.startswith("m-"):
                raise RuntimeError(f"machine add did not return an id: {machine!r}")
            catalog = json.loads(run("machine", "list", "--json").stdout)
            if not any(item["id"] == machine and item["target"] == "kodade-smoke" and item["session"] == SESSION for item in catalog):
                raise RuntimeError("saved machine catalog did not preserve target/session")
            # This unreachable profile starts alongside the real one. The
            # local marker below proves its bounded connection attempts never
            # freeze normal attach or queue input for later replay.
            run("machine", "add", "missing-kodade-smoke-host", "--label", "Offline host")

            # A real controlling PTY exercises the ordinary local attach with
            # the saved profile loaded. Both daemons start pane id 1, so the
            # marker assertions also prove endpoint identity, not pane id.
            local_pane = run("-s", SESSION, "run", "--name", "local-probe", "--", "sh", "-c", "exec sleep 30").stdout.strip()
            if not local_pane.isdigit():
                raise RuntimeError(f"local run did not return a pane id: {local_pane!r}")
            local_layout = json.loads(run("-s", SESSION, "ls", "--json").stdout)
            local_pane = str(next(item["id"] for item in local_layout["panes"] if item["focused"]))

            # A remote run starts the remote daemon through SSH, forwards its
            # socket, and returns a pane id. Read its output through a fresh tunnel.
            pane = run("--remote", "kodade-smoke", "-s", SESSION, "run", "--name", "probe", "--", "sh", "-c", "printf SSH_SMOKE_OK; exec sleep 30").stdout.strip()
            if not pane.isdigit():
                raise RuntimeError(f"remote run did not return a pane id: {pane!r}")
            remote_layout = json.loads(run("--remote", "kodade-smoke", "-s", SESSION, "ls", "--json").stdout)
            pane = str(next(item["id"] for item in remote_layout["panes"] if item["focused"]))
            if pane != local_pane:
                raise RuntimeError("smoke setup requires colliding local/remote pane ids")
            wait_until(
                "remote pane output",
                lambda: "SSH_SMOKE_OK" in run("--remote", "kodade-smoke", "-s", SESSION, "pane", "read", pane).stdout,
            )
            remote_socket = Path(run("--remote", "kodade-smoke", "-s", SESSION, "session", "path").stdout.strip())
            if not remote_socket.exists():
                raise RuntimeError("remote session path was not a live socket")

            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
            def controlling_tty() -> None:
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            tui = subprocess.Popen(
                [str(BINARY), "-s", SESSION],
                env={**env, "TERM": "xterm-256color"},
                stdin=slave,
                stdout=slave,
                stderr=slave,
                close_fds=True,
                preexec_fn=controlling_tty,
            )
            os.close(slave)
            tui_drain_stop = threading.Event()
            def drain_tui() -> None:
                while not tui_drain_stop.is_set():
                    try:
                        if not os.read(master, 65536):
                            return
                    except OSError:
                        return
            threading.Thread(target=drain_tui, daemon=True).start()
            time.sleep(1.5)
            if tui.poll() is not None:
                raise RuntimeError(f"ordinary TUI attach exited early: {tui.returncode}")
            # Dismiss first-run onboarding and prove raw PTY input reaches the
            # local daemon before attempting endpoint selection.
            os.write(master, b"\x1b")
            time.sleep(0.2)
            os.write(master, b"printf LOCAL_BEFORE_SWITCH\r")
            wait_until(
                "local TUI marker before switch",
                lambda: "LOCAL_BEFORE_SWITCH" in pane_output("-s", SESSION, "pane", "read", local_pane),
            )
            # prefix M selects the saved endpoint; its input must reach remote
            # pane 1 only, despite local pane 1 existing too.
            os.write(master, b"\x02")
            time.sleep(0.15)
            os.write(master, b"M")
            time.sleep(0.15)
            os.write(master, b"printf REMOTE_TUI_MARKER\r")
            wait_until(
                "remote TUI marker",
                lambda: "REMOTE_TUI_MARKER" in pane_output("--remote", "kodade-smoke", "-s", SESSION, "pane", "read", pane),
                seconds=20,
            )
            if "REMOTE_TUI_MARKER" in pane_output("-s", SESSION, "pane", "read", local_pane):
                raise RuntimeError("remote-selected TUI input leaked to local pane")

            # Disable is observed by the attached client; it closes the remote
            # channel and returns selection to local without restarting TUI.
            run("machine", "disable", machine)
            time.sleep(1.5)
            os.write(master, b"printf LOCAL_TUI_MARKER\r")
            wait_until(
                "local TUI marker after disable",
                lambda: "LOCAL_TUI_MARKER" in pane_output("-s", SESSION, "pane", "read", local_pane),
            )
            if "LOCAL_TUI_MARKER" in pane_output("--remote", "kodade-smoke", "-s", SESSION, "pane", "read", pane):
                raise RuntimeError("disabled remote endpoint still received TUI input")
            os.write(master, b"\x02d")
            tui.wait(timeout=8)
            tui_drain_stop.set()
            os.close(master)
            tui = None

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
            print("SSH smoke passed: daemon startup, run/read, independent tunnels, rename, teardown")
        finally:
            if tui is not None and tui.poll() is None:
                tui.terminate()
                try:
                    tui.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    tui.kill()
            if tui_drain_stop is not None:
                tui_drain_stop.set()
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
