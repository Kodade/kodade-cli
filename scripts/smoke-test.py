#!/usr/bin/env python3
"""Exercise the built CLI with real daemons in a disposable home and runtime."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()
assert binary.is_file(), f"Build first: cargo build ({binary} missing)"

with tempfile.TemporaryDirectory(prefix="kodade-smoke-") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=str(root), XDG_RUNTIME_DIR=str(root / "run"),
               XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh")

    def run(*args, extra=None, success=True):
        result = subprocess.run([str(binary), *args], env=env | (extra or {}),
                                capture_output=True, text=True, timeout=12)
        if success:
            assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result

    def daemon_pid(session):
        deadline = time.monotonic() + 3
        while True:
            matches = []
            for proc in Path("/proc").glob("[0-9]*"):
                try:
                    environ = (proc / "environ").read_bytes()
                    command = (proc / "cmdline").read_bytes().replace(b"\0", b" ")
                except OSError:
                    continue
                if f"XDG_RUNTIME_DIR={env['XDG_RUNTIME_DIR']}".encode() in environ and \
                        f"kodade-cli daemon {session}".encode() in command:
                    matches.append(proc.name)
            if len(matches) == 1:
                return int(matches[0])
            assert time.monotonic() < deadline, (session, matches)
            time.sleep(0.05)

    try:
        # Read-only diagnostics must not start a daemon or initialize user config.
        report = json.loads(run("doctor", "--json").stdout)
        socket = Path(report["socket"])
        assert not socket.exists()
        run("config", "init")
        config = root / ".config/kodade-cli/config.toml"
        original = config.read_bytes()
        assert run("config", "init", success=False).returncode != 0
        assert config.read_bytes() == original
        run("config", "validate")

        # Creating a workspace is sufficient on a cold start; no attached TUI needed.
        workspace = run("new", "-w", "smoke", str(root)).stdout.strip()
        assert workspace.isdigit()
        assert socket.exists()
        assert json.loads(run("ls", "--json").stdout)["active_workspace"] == int(workspace)
        pane = run("run", "--name", "probe", "--", "sh", "-c",
                   "printf 'KODADE_SMOKE_OK\\n'; exec sleep 30").stdout.strip()
        deadline = time.monotonic() + 4
        while "KODADE_SMOKE_OK" not in run("pane", "read", pane).stdout:
            assert time.monotonic() < deadline, "pane never produced output"
            time.sleep(0.05)
        report = json.loads(run("doctor", "--json").stdout)
        assert next(check for check in report["checks"] if check["name"] == "daemon")["status"] == "ok"
        assert run("-s", "../outside", "session", "path", success=False).returncode != 0

        # Targeting across one-shot connections must preserve the CLI script selection.
        other = run("run", "--name", "other", "--", "/bin/sh").stdout.strip()
        identity_file = root / "pane-identity"
        run("send", other, f"printf '%s' \"$$\" > {identity_file}")
        deadline = time.monotonic() + 4
        while not identity_file.exists():
            assert time.monotonic() < deadline, "shell did not execute identity command"
            time.sleep(0.05)
        child = int(identity_file.read_text())
        start = Path(f"/proc/{child}/stat").read_text().rsplit(")", 1)[1].split()[19]
        old_daemon = daemon_pid("default")
        # A failed replacement leaves the source usable; successful replacements
        # preserve the original PTY process through two daemon PID changes.
        assert run("session", "upgrade", "--binary", "/no/such/kodade", success=False).returncode != 0
        run("ls", "--json")
        # Fail after the real importer owns staged panes, then after aliases
        # switch but before commit acknowledgement. Both must resume the source.
        broken_import = root / "broken-import"
        broken_import.write_text("#!/usr/bin/env python3\nimport os,sys\na=sys.argv[1:]\na[a.index('--staged-socket')+1]=" + repr(str(root / "missing/sub/socket")) + "\nos.execv(" + repr(str(binary)) + ", [" + repr(str(binary)) + "]+a)\n")
        broken_commit = root / "broken-commit"
        broken_commit.write_text("""#!/usr/bin/env python3
import os,socket,struct,sys
args=sys.argv[1:]
def arg(name): return args[args.index(name)+1]
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
s.connect(arg('--import'))
s.sendall(os.environ['KODADE_HANDOFF_TOKEN'].encode()+b'\\n')
def exact(n):
 out=b''
 while len(out)<n:
  chunk=s.recv(n-len(out))
  assert chunk
  out+=chunk
 return out
exact(struct.unpack('!I',exact(4))[0])
s.sendall(b'validated\\n')
marker,ancillary,flags,addr=s.recvmsg(1,socket.CMSG_SPACE(64*4))
assert marker==b'F' and ancillary
listener=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
listener.bind(arg('--staged-socket'))
listener.listen(1)
os.link(arg('--staged-socket'),arg('--staged-hook'))
s.sendall(b'ready\\n')
assert exact(7)==b'commit\\n'
# Deliberately exit without committed: descriptors close and source rolls back.
""")
        for phase, replacement in enumerate((broken_import, broken_commit)):
            replacement.chmod(0o700)
            assert run("session", "upgrade", "--binary", str(replacement), success=False).returncode != 0
            assert daemon_pid("default") == old_daemon
            check = root / f"rollback-{phase}"
            run("send", other, f"printf '%s:%s' \"$$\" \"$((23 * 17))\" > {check}")
            deadline = time.monotonic() + 4
            while not check.exists() or not check.read_text():
                assert time.monotonic() < deadline, "source did not resume after failed import/commit"
                time.sleep(0.05)
            assert check.read_text() == f"{child}:391"
            assert not list(socket.parent.glob(".up-*")), "rollback staging leaked"
        run("session", "upgrade")
        first_target = daemon_pid("default")
        assert first_target != old_daemon
        os.kill(child, 0)
        run("session", "upgrade")
        assert daemon_pid("default") not in (old_daemon, first_target)
        os.kill(child, 0)
        result_file = root / "upgrade-result"
        run("send", other, f"printf '%s:%s' \"$$\" \"$((173 * 29))\" > {result_file}")
        deadline = time.monotonic() + 4
        while not result_file.exists():
            assert time.monotonic() < deadline, "original shell did not execute after two upgrades"
            time.sleep(0.05)
        assert result_file.read_text() == f"{child}:5017"
        assert Path(f"/proc/{child}/stat").read_text().rsplit(")", 1)[1].split()[19] == start
        assert not list(socket.parent.glob(".up-*")), "handoff staging leaked"
        run("pane", "kill", pane)
        run("pane", "read", other)
        assert run("pane", "read", pane, success=False).returncode != 0

        # Agent start also creates a missing session, and `current` cannot cross sockets.
        cold = run("-s", "cold-agent", "agent", "start", "--name", "cold", "--",
                   "sh", "-c", "sleep 30").stdout.strip()
        assert cold.isdigit()
        cold_socket = run("-s", "cold-agent", "session", "path").stdout.strip()
        foreign = run("--socket", cold_socket, "agent", "read", "current",
                      extra={"KODADE_SESSION": "default", "KODADE_SOCKET": str(socket),
                             "KODADE_PANE": pane}, success=False)
        assert foreign.returncode != 0 and "different socket" in foreign.stderr

        # Inherited sockets remain authoritative after a live session rename.
        run("session", "rename", "renamed")
        renamed_socket = run("-s", "renamed", "session", "path").stdout.strip()
        inherited = {"KODADE_SESSION": "renamed", "KODADE_SOCKET": renamed_socket}
        assert run("session", "path", extra=inherited).stdout.strip() == renamed_socket
        assert json.loads(run("ls", "--json", extra=inherited).stdout)["workspaces"]
        explicit = run("-s", "elsewhere", "session", "path", extra=inherited).stdout.strip()
        assert explicit != renamed_socket
        assert not Path(explicit).exists()
        hook_check = root / "old-hook-alias"
        run("--socket", renamed_socket, "send", other,
            f'if "$KODADE_BIN" agent report "$KODADE_PANE" working --source handoff; then printf passed > {hook_check}; fi')
        deadline = time.monotonic() + 4
        while not hook_check.exists():
            assert time.monotonic() < deadline, "original pane hook did not follow two upgrades and rename"
            time.sleep(0.05)
        run("--socket", renamed_socket, "kill-session")
        deadline = time.monotonic() + 3
        while Path(renamed_socket).exists():
            assert time.monotonic() < deadline, "daemon did not release its socket"
            time.sleep(0.05)
    finally:
        # Cleanup is scoped to sessions created by this test, even after assertions fail.
        for session in ("default", "renamed", "cold-agent"):
            run("-s", session, "kill-session", success=False)

print("CLI smoke passed: cold start, PTY output, live daemon handoff, diagnostics, config preservation, session context, rename, shutdown")
