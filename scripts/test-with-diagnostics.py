#!/usr/bin/env python3
"""Run tests with a bounded wait and macOS stacks if a PTY test stops progressing."""
import os
import subprocess
import sys

process = subprocess.Popen(["cargo", "test", "--", "--nocapture"])
try:
    sys.exit(process.wait(timeout=90))
except subprocess.TimeoutExpired:
    print("Tests are still running after 90 seconds; collecting process stacks", flush=True)
    if sys.platform == "darwin":
        listing = subprocess.check_output(["ps", "-axo", "pid,comm"], text=True)
        for line in listing.splitlines():
            fields = line.strip().split(None, 1)
            if len(fields) == 2 and "/kodade_cli_daemon-" in fields[1]:
                subprocess.run(["sample", fields[0], "1", "1"], timeout=15, check=False)
    try:
        sys.exit(process.wait(timeout=150))
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
        print("Tests exceeded four minutes", file=sys.stderr)
        sys.exit(1)
