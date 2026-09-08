#!/usr/bin/env python3
"""Exercise workspace environments and worktree ownership with real Git and PTYs."""
import json, os, subprocess, sys, tempfile, time
from pathlib import Path

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/kodade-cli").resolve()

def wait(label, predicate):
    end = time.monotonic() + 10
    while time.monotonic() < end:
        value = predicate()
        if value: return value
        time.sleep(.05)
    raise RuntimeError("timed out: " + label)

with tempfile.TemporaryDirectory(prefix="kc-workspace-env-") as tmp:
    root = Path(tmp); repo = root / "repo"; linked = root / "linked"; custom = root / "custom-checkout"
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=tmp, XDG_RUNTIME_DIR=str(root / "run"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh", TERM="xterm-256color")
    def run(*args, check=True):
        result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=15)
        if check and result.returncode: raise RuntimeError(result.stderr)
        return result
    def cli(*args): return run(str(BINARY), "-s", "workspace-env", *args).stdout.strip()
    def pane_output(pane, marker):
        return wait(marker, lambda: marker in cli("pane", "read", str(pane)))
    try:
        run("git", "init", "-q", str(repo)); run("git", "-C", str(repo), "config", "user.email", "fixture@example.test")
        run("git", "-C", str(repo), "config", "user.name", "Fixture")
        (repo / "tracked").write_text("fixture\n"); run("git", "-C", str(repo), "add", "tracked"); run("git", "-C", str(repo), "commit", "-qm", "initial")
        run("git", "-C", str(repo), "worktree", "add", "-qb", "linked", str(linked))
        cli("workspace", "new", "main", str(repo), "--env", "ISSUE52_SCOPE=main")
        cli("workspace", "new", "other", str(repo), "--env", "ISSUE52_SCOPE=other")
        # Runs create tabs, then a direct split and NewTab exercise every
        # future-pane spawn route with the selected workspace environment.
        main_run = int(cli("run", "-w", "main", "--", "sh", "-c", "printf MAIN:$ISSUE52_SCOPE; sleep 1")); pane_output(main_run, "MAIN:main")
        other_run = int(cli("run", "-w", "other", "--", "sh", "-c", "printf OTHER:$ISSUE52_SCOPE; sleep 1")); pane_output(other_run, "OTHER:other")
        split = int(cli("split", "-p", str(main_run), "--", "sh", "-c", "printf SPLIT:$ISSUE52_SCOPE; sleep 1")); pane_output(split, "SPLIT:main")
        tab = int(cli("new-tab", "-w", "main")); cli("send", str(tab), "printf TAB:$ISSUE52_SCOPE") ; pane_output(tab, "TAB:main")
        default = int(cli("run", "-w", "default", "--", "sh", "-c", "printf DEFAULT:${ISSUE52_SCOPE-unset}; sleep 1")); pane_output(default, "DEFAULT:unset")
        # --base and a relative --path use the selected workspace's repository.
        cli("workspace", "select", "main")
        cli("worktree", "add", "custom", "--base", "HEAD", "--path", "../custom-checkout")
        assert custom.is_dir() and (custom / ".git").exists()
        # Opening a linked checkout works; an arbitrary repository is rejected.
        cli("workspace", "select", "main"); cli("worktree", "open", "../linked")
        foreign = root / "foreign"; run("git", "init", "-q", str(foreign))
        rejected = run(str(BINARY), "-s", "workspace-env", "worktree", "open", str(foreign), "-w", "main", check=False)
        assert rejected.returncode and "not a worktree" in rejected.stderr, rejected.stderr
        # Dirty protection leaves the registered checkout untouched.
        (custom / "dirty").write_text("keep\n")
        removed = run(str(BINARY), "-s", "workspace-env", "worktree", "remove", "custom", check=False)
        assert removed.returncode and (custom / "dirty").exists(), removed.stderr
    finally:
        subprocess.run([str(BINARY), "-s", "workspace-env", "kill-session"], env=env, capture_output=True)
        subprocess.run(["git", "-C", str(repo), "worktree", "remove", "--force", str(linked)], env=env, capture_output=True)
        subprocess.run(["git", "-C", str(repo), "worktree", "remove", "--force", str(custom)], env=env, capture_output=True)
print("Workspace environment and worktree smoke passed")
