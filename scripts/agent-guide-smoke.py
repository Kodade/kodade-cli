#!/usr/bin/env python3
"""Execute the bundled guide's shell workflow in an isolated real session."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
guide = Path(__file__).resolve().parents[1] / "docs/AGENT-GUIDE.md"
workflow = next(block for block in re.findall(r"```sh\n(.*?)```", guide.read_text(), re.S)
                if "BUILD_READY" in block)
with tempfile.TemporaryDirectory(prefix="kg-") as directory:
    root = Path(directory)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KODADE_")}
    env.update(HOME=directory, XDG_RUNTIME_DIR=str(root / "run"),
               XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh",
               PATH=f"{binary.parent}:{env['PATH']}")
    try:
        output = subprocess.run(["sh", "-eu", "-c", workflow], env=env, cwd=root,
                                text=True, capture_output=True, check=True, timeout=20)
        assert "BUILD_READY" in output.stdout
        assert not output.stderr, output.stderr
        print("Agent guide smoke passed: documented shell workflow executed in real PTYs")
    finally:
        subprocess.run([binary, "-s", "work", "kill-session"], env=env, cwd=root,
                       capture_output=True, timeout=10)
