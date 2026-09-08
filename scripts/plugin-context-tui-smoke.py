#!/usr/bin/env python3
"""Exercise plugin selection and pane context through a controlling TTY."""
import fcntl, json, os, pty, signal, struct, subprocess, sys, tempfile, termios, threading, time
from pathlib import Path

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()

def wait(label, predicate):
    end = time.monotonic() + 10
    while time.monotonic() < end:
        if predicate(): return
        time.sleep(.05)
    raise RuntimeError("timed out: " + label)

def controlling_terminal():
    os.setsid(); fcntl.ioctl(0, termios.TIOCSCTTY, 0)

def fixture_cell(cli, pane, marker):
    """Return the client mouse cell for marker from the daemon's live screen."""
    layout = json.loads(cli("ls", "--json"))
    screen = next(item["screen"] for item in layout["panes"] if item["id"] == pane)
    for row, runs in enumerate(screen["rows"]):
        text = "".join(run["text"] for run in runs)
        column = text.find(marker)
        if column >= 0:
            # The default sidebar is 24 cells wide and panes begin below the
            # two-row tab/header area. SGR mouse coordinates are one-based.
            return 26 + column, 3 + row
    raise RuntimeError(f"{marker!r} was absent from the live pane screen")

with tempfile.TemporaryDirectory(prefix="kc-plugin-context-") as tmp:
    root = Path(tmp); fixture = root / "fixture"; fixture.mkdir()
    env = {k:v for k,v in os.environ.items() if not k.startswith("KODADE_")}
    env.update(HOME=tmp, XDG_CONFIG_HOME=str(root / ".config"), XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh", TERM="xterm-256color")
    out, pane_path, pane_id = root / "action.json", root / "pane-path", root / "pane-id"
    (fixture / "kodade-plugin.toml").write_text(f'''manifest_version = 1
id = "context-fixture"
name = "Context fixture"
version = "0.1.0"
[[actions]]
id = "zzbackground"
name = "TUI context Zzbackground"
command = 'cp "$KODADE_PLUGIN_CONTEXT" {out}; sleep 1'
contexts = ["selection"]
[[actions]]
id = "zzpane"
name = "TUI context Zzpane"
command = 'printf "%s" "$KODADE_PLUGIN_CONTEXT" > {pane_path}; printf "%s" "$KODADE_PANE" > {pane_id}; sleep 30'
pane = true
contexts = ["selection"]
''')
    def cli(*args):
        result = subprocess.run([str(BINARY), "-s", "context", *args], env=env, capture_output=True, text=True, timeout=12)
        if result.returncode: raise RuntimeError(result.stderr)
        return result.stdout
    master = slave = tui = None
    try:
        cli("run", "--", "sh"); cli("plugin", "link", str(fixture))
        master, slave = pty.openpty(); fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        tui = subprocess.Popen([str(BINARY), "-s", "context"], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=controlling_terminal)
        transcript = bytearray()
        def drain():
            try:
                while True: transcript.extend(os.read(master, 65536))
            except OSError: pass
        threading.Thread(target=drain, daemon=True).start()
        wait("attach", lambda: b"?2004h" in transcript)
        time.sleep(.3)
        # Suppress shell echo and clear the pane after the command has run.
        # This leaves SELECT ME as the only selectable terminal text, so the
        # selection cannot accidentally include the command used to draw it.
        os.write(master, b"stty -echo; printf '\\033[2J\\033[5;10H\\123\\105\\114\\105\\103\\124\\040\\115\\105'; stty echo\r")
        panes = json.loads(cli("pane", "ls", "--json"))
        focused = next(p["id"] for p in panes if p["focused"])
        wait("pane text", lambda: "SELECT ME" in cli("pane", "read", str(focused)))
        # The CLI has observed the PTY update; give the attached client one
        # refresh interval before using that same screen coordinate.
        time.sleep(.3)
        start_x, start_y = fixture_cell(cli, focused, "SELECT ME")
        end_x = start_x + len("SELECT ME") - 1
        os.write(master, f"\x1b[<0;{start_x};{start_y}M\x1b[<32;{end_x};{start_y}M\x1b[<0;{end_x};{start_y}m".encode())
        os.write(master, b"\x02 ")
        time.sleep(.3); os.write(master, b"zzbackground\r")
        wait("background context", out.exists)
        context = json.loads(out.read_text())
        assert context["selected_text"] == "SELECT ME", context
        assert context["endpoint"] == "local" and context["pane"] and context["cwd"], context
        os.write(master, b"\x02 "); time.sleep(.3); os.write(master, b"zzpane\r")
        wait("pane context", pane_path.exists)
        wait("pane id", pane_id.exists)
        private = Path(pane_path.read_text())
        assert private.exists(), private
        # A computed PTY write must still run while the action pane sleeps.
        # This takes the daemon's public SendKeys route rather than relying on
        # whichever pane the new action focused in the TUI.
        cli("pane", "send-keys", str(focused), "printf 'RESPONSIVE\\n'", "Enter")
        wait("responsive input", lambda: "RESPONSIVE" in cli("pane", "read", str(focused)))
        cli("pane", "kill", pane_id.read_text())
        wait("pane cleanup", lambda: not private.exists())
        os.write(master, b"\x02d"); assert tui.wait(timeout=5) == 0
    finally:
        if tui and tui.poll() is None: tui.kill(); tui.wait()
        if slave: os.close(slave)
        if master: os.close(master)
        subprocess.run([str(BINARY), "-s", "context", "kill-session"], env=env, capture_output=True)
print("Plugin context TUI smoke passed")
