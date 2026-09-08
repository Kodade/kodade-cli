#!/usr/bin/env python3
"""Exercise configured command key, palette, and pane actions through a TTY."""
import fcntl, json, os, pty, signal, struct, subprocess, sys, tempfile, termios, threading, time
from pathlib import Path

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()

def wait(label, predicate):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if predicate(): return
        time.sleep(.05)
    raise RuntimeError("timed out: " + label)

def controlling_terminal():
    os.setsid(); fcntl.ioctl(0, termios.TIOCSCTTY, 0)

with tempfile.TemporaryDirectory(prefix="kc-configured-command-") as tmp:
    root = Path(tmp)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=tmp, XDG_CONFIG_HOME=str(root / ".config"), XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh", TERM="xterm-256color")
    config_dir = root / ".config" / "kodade-cli"; config_dir.mkdir(parents=True)
    key_out, palette_out, pane_out, pane_id, pane_path = (root / name for name in ("key.json", "palette.json", "pane.json", "pane-id", "pane-path"))
    (config_dir / "config.toml").write_text(f'''[[commands]]
label = "TUI key Zzkey"
key = "prefix+F6"
command = 'cp "$KODADE_PLUGIN_CONTEXT" {key_out}'
contexts = ["workspace"]

[[commands]]
label = "TUI palette Zzpalette"
key = "prefix+F7"
command = 'cp "$KODADE_PLUGIN_CONTEXT" {palette_out}'
contexts = ["workspace"]

[[commands]]
label = "TUI pane Zzpane"
key = "prefix+F8"
command = 'cp "$KODADE_PLUGIN_CONTEXT" {pane_out}; printf "%s" "$KODADE_PANE" > {pane_id}; printf "%s" "$KODADE_PLUGIN_CONTEXT" > {pane_path}; sleep 30'
pane = true
contexts = ["workspace"]
''')
    def cli(*args):
        result = subprocess.run([str(BINARY), "-s", "configured", *args], env=env, capture_output=True, text=True, timeout=12)
        if result.returncode: raise RuntimeError(result.stderr)
        return result.stdout
    master = slave = tui = None
    try:
        cli("run", "--", "sh")
        master, slave = pty.openpty(); fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        tui = subprocess.Popen([str(BINARY), "-s", "configured"], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=controlling_terminal)
        transcript = bytearray()
        def drain():
            try:
                while True: transcript.extend(os.read(master, 65536))
            except OSError: pass
        threading.Thread(target=drain, daemon=True).start()
        wait("attach", lambda: b"?2004h" in transcript)
        panes = json.loads(cli("pane", "ls", "--json")); focused = next(item["id"] for item in panes if item["focused"])
        # Bound command executes from the live TUI prefix path.
        os.write(master, b"\x02\x1b[17~"); wait("key command", key_out.exists)
        # The command center discovers and activates the configured command.
        os.write(master, b"\x02 "); time.sleep(.2); os.write(master, b"zzpalette\r")
        wait("palette command", palette_out.exists)
        # A configured pane action gets a daemon-owned context file. It stays
        # alive until explicitly killed, then its private file is removed.
        os.write(master, b"\x02\x1b[19~"); wait("pane command", pane_out.exists); wait("pane id", pane_id.exists)
        for path in (key_out, palette_out, pane_out):
            context = json.loads(path.read_text())
            assert context["endpoint"] == "local" and context["workspace"] and context["tab"] and context["pane"] and context["cwd"], context
        private = Path(pane_path.read_text())
        assert private.exists(), private
        # Killing the pane ends the slow command and removes its daemon-owned
        # private context file.
        cli("pane", "kill", pane_id.read_text())
        wait("pane cleanup", lambda: not private.exists())
        os.write(master, b"\x02d"); assert tui.wait(timeout=5) == 0
    finally:
        if tui and tui.poll() is None: tui.kill(); tui.wait()
        if slave: os.close(slave)
        if master: os.close(master)
        subprocess.run([str(BINARY), "-s", "configured", "kill-session"], env=env, capture_output=True)
print("Configured command TUI smoke passed")
