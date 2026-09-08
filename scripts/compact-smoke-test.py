#!/usr/bin/env python3
"""Controlling-PTY proof that compact view projects and restores split panes."""
import fcntl, json, os, pty, select, struct, subprocess, sys, tempfile, termios, time
from pathlib import Path

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()

def wait(label, predicate):
    until = time.monotonic() + 8
    while time.monotonic() < until:
        if predicate(): return
        time.sleep(.05)
    raise RuntimeError(f"timed out waiting for {label}")

with tempfile.TemporaryDirectory(prefix="kc-") as directory:
    root = Path(directory)
    env = {k:v for k,v in os.environ.items() if not k.startswith("KODADE_")}
    env.update(HOME=directory, XDG_RUNTIME_DIR=str(root/'run'), XDG_STATE_HOME=str(root/'state'), SHELL='/bin/sh', TERM='xterm-256color')
    cfg = root/'.config/kodade-cli/config.toml'; cfg.parent.mkdir(parents=True)
    cfg.write_text('[sidebar]\ncompact_view = "auto"\n')
    def run(*args): return subprocess.run([str(BINARY), '-s', 'compact', *args], env=env, capture_output=True, text=True, timeout=12, check=True)
    try:
        run('run', '--', 'sh')
        panes = lambda: json.loads(run('ls', '--json').stdout)['panes']
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 70, 0, 0))
        def ctty(): os.setsid(); fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        tui = subprocess.Popen([str(BINARY), '-s', 'compact'], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=ctty)
        try:
            wait('TUI attach', lambda: select.select([master], [], [], .1)[0] or tui.poll() is not None)
            time.sleep(.3); os.write(master, b'\x02%')
            wait('split panes', lambda: len(panes()) == 2)
            ids = [str(p['id']) for p in panes()]
            os.write(master, b'printf NARROW_A; stty size\n')
            wait('narrow focused input', lambda: 'NARROW_A' in run('pane','read',ids[0]).stdout or 'NARROW_A' in run('pane','read',ids[1]).stdout)
            os.write(master, b'\x02o') # keyboard pane cycle remains accessible in compact view
            time.sleep(.2); os.write(master, b'printf NARROW_B\n')
            wait('switched focused input', lambda: any('NARROW_B' in run('pane','read',i).stdout for i in ids))
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 120, 0, 0))
            os.kill(tui.pid, 28) # SIGWINCH
            time.sleep(.3); os.write(master, b'printf WIDE; stty size\n')
            wait('wide input', lambda: any('WIDE' in run('pane','read',i).stdout for i in ids))
            assert len(panes()) == 2, 'compact projection destroyed a split PTY'
            # A separate one-shot (wide) client can still inspect both PTYs.
            assert len(panes()) == 2
            os.write(master, b'\x02d'); tui.wait(timeout=5)
        finally:
            if tui.poll() is None: tui.kill(); tui.wait()
            os.close(master); os.close(slave)
    finally:
        subprocess.run([str(BINARY), '-s','compact','kill-session'], env=env, capture_output=True, timeout=12)
print('Compact smoke passed: narrow focused PTY, keyboard switch, wide split restoration')
