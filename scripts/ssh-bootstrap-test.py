#!/usr/bin/env python3
"""Prove remote bootstrap against a disposable localhost OpenSSH server.

This deliberately uses the shipped CLI.  A private ``curl`` shim serves a
checksum-verified fixture archive to the local updater; SSH, sshd, shell commands, and
the installed binary are all real.  The temporary remote HOME has spaces to
exercise the quoted remote install path.
"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/debug/kodade-cli").resolve()
SSHD = Path("/usr/bin/sshd")
SSH = Path("/usr/bin/ssh")
SSH_KEYGEN = Path("/usr/bin/ssh-keygen")


def require(path: Path) -> None:
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"required executable is unavailable: {path}")


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def executable(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(0o700)


def wait_for_ssh(config: Path) -> None:
    for _ in range(80):
        try:
            result = subprocess.run(
                [str(SSH), "-F", str(config), "bootstrap-smoke", "true"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=2,
            )
            if result.returncode == 0:
                return
        except subprocess.TimeoutExpired:
            pass
        time.sleep(0.05)
    raise RuntimeError("timed out waiting for disposable sshd")


def main() -> None:
    for command in (BINARY, SSHD, SSH, SSH_KEYGEN):
        require(command)
    version = subprocess.run(
        [str(BINARY), "--version"], check=True, text=True, capture_output=True
    ).stdout.split()[1]

    with tempfile.TemporaryDirectory(prefix="kodade-bootstrap-") as temporary:
        root = Path(temporary)
        remote_home = root / "remote home with spaces"
        local_home = root / "local-home"
        bin_dir = root / "bin"
        fixtures = root / "fixtures"
        for directory in (remote_home, local_home, bin_dir, fixtures):
            directory.mkdir(mode=0o700)

        archive = fixtures / "kodade-cli.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            info = tar.gettarinfo(BINARY, arcname="package/kodade-cli")
            info.mode = 0o755
            with BINARY.open("rb") as source:
                tar.addfile(info, source)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        architecture = subprocess.run(
            ["uname", "-m"], check=True, text=True, capture_output=True
        ).stdout.strip()
        target_arch = {
            "x86_64": "x86_64",
            "amd64": "x86_64",
            "aarch64": "aarch64",
            "arm64": "aarch64",
        }.get(architecture)
        if target_arch is None:
            raise RuntimeError(f"unsupported smoke-test architecture: {architecture}")
        asset = f"kodade-cli-{version}-{target_arch}-unknown-linux-gnu.tar.gz"
        metadata = fixtures / "metadata"
        sums = fixtures / "SHA256SUMS"
        metadata.write_text(json.dumps({"tag_name": f"v{version}", "assets": [
            {"name": "SHA256SUMS", "browser_download_url": "fixture://sums"},
            {"name": asset, "browser_download_url": "fixture://archive"},
        ]}))
        sums.write_text(f"{digest}  {asset}\n")
        mapping = fixtures / "mapping.json"
        mapping.write_text(json.dumps({
            "https://api.github.com/repos/Kodade/kodade-cli/releases/latest": str(metadata),
            "fixture://sums": str(sums),
            "fixture://archive": str(archive),
        }))

        # This shim affects only the CLI process under test.  It can also make
        # the real remote shell consume one byte, proving a partial upload
        # leaves its old destination in place.
        executable(bin_dir / "curl", "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "url = sys.argv[-1]\n"
            "with open(os.environ['KODADE_BOOTSTRAP_FIXTURES']) as source: paths = json.load(source)\n"
            "try:\n"
            "    with open(paths[url], 'rb') as source: sys.stdout.buffer.write(source.read())\n"
            "except KeyError:\n"
            "    print('unexpected fixture URL: ' + url, file=sys.stderr); sys.exit(22)\n")
        executable(bin_dir / "ssh", "#!/usr/bin/env python3\n"
            "import os, sys\n"
            f"args = ['{SSH}', '-F', '{root / 'ssh_config'}'] + sys.argv[1:]\n"
            "if os.environ.get('KODADE_BOOTSTRAP_TRUNCATE') == '1':\n"
            "    args = [arg.replace('cat >\\\"$tmp\\\"', 'head -c 1 >\\\"$tmp\\\"') for arg in args]\n"
            "os.execv(args[0], args)\n")

        host_key, client_key = root / "host_key", root / "client_key"
        for key in (host_key, client_key):
            subprocess.run([str(SSH_KEYGEN), "-q", "-N", "", "-t", "ed25519", "-f", str(key)], check=True, timeout=10)
        authorized = root / "authorized_keys"
        shutil.copyfile(client_key.with_suffix(".pub"), authorized)
        port = free_port()
        user = os.environ.get("USER", os.environ.get("LOGNAME", "nobody"))
        config = root / "sshd_config"
        config.write_text("\n".join([
            f"Port {port}", "ListenAddress 127.0.0.1", f"HostKey {host_key}",
            f"PidFile {root / 'sshd.pid'}", f"AuthorizedKeysFile {authorized}",
            "PasswordAuthentication no", "KbdInteractiveAuthentication no", "UsePAM no",
            "StrictModes no", "UseDNS no", "LogLevel ERROR",
            f'SetEnv HOME="{remote_home}"', "SetEnv PATH=/usr/bin:/bin",
        ]) + "\n")
        subprocess.run([str(SSHD), "-t", "-f", str(config)], check=True, text=True, capture_output=True)
        ssh_config = root / "ssh_config"
        ssh_config.write_text("\n".join([
            "Host bootstrap-smoke", "  HostName 127.0.0.1", f"  Port {port}", f"  User {user}",
            f"  IdentityFile {client_key}", "  IdentitiesOnly yes", "  StrictHostKeyChecking no",
            "  UserKnownHostsFile /dev/null", "  LogLevel ERROR",
        ]) + "\n")

        env = {
            **os.environ,
            "HOME": str(local_home),
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "KODADE_BOOTSTRAP_FIXTURES": str(mapping),
        }
        sshd = subprocess.Popen(
            [str(SSHD), "-D", "-e", "-f", str(config)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            wait_for_ssh(ssh_config)
            remote_binary = remote_home / ".local/bin/kodade-cli"
            if remote_binary.exists():
                raise RuntimeError("fixture host unexpectedly has kodade-cli before bootstrap")

            added = subprocess.run(
                [str(BINARY), "machine", "add", "bootstrap-smoke", "--label", "Bootstrap smoke", "--install"],
                env=env,
                text=True,
                capture_output=True,
                timeout=45,
            )
            if added.returncode != 0:
                raise RuntimeError(f"missing-binary bootstrap failed:\n{added.stderr}")
            profile = added.stdout.strip()
            installed = subprocess.run(
                [str(SSH), "-F", str(ssh_config), "bootstrap-smoke", '"$HOME/.local/bin/kodade-cli" --version'],
                text=True,
                capture_output=True,
                timeout=10,
            )
            if installed.returncode != 0 or installed.stdout.strip() != f"kodade-cli {version}":
                raise RuntimeError(f"installed binary is unusable: {installed.stdout!r} {installed.stderr!r}")

            # A checksum failure happens before SSH upload: retain a known,
            # incompatible prior executable as direct proof.
            prior = remote_binary.with_name("prior-kodade-cli")
            prior.write_text("#!/bin/sh\necho old remote binary\n")
            prior.chmod(0o755)
            os.replace(prior, remote_binary)
            sums.write_text(f"{'0' * 64}  {asset}\n")
            failed = subprocess.run(
                [str(BINARY), "machine", "prepare", profile, "--install"],
                env=env,
                text=True,
                capture_output=True,
                timeout=45,
            )
            if failed.returncode == 0 or remote_binary.read_text() != "#!/bin/sh\necho old remote binary\n":
                raise RuntimeError("bad checksum uploaded or replaced the prior remote binary")

            # A latest stable release that does not match the local exact
            # compatibility contract must fail before artifact retrieval/upload.
            mismatch = "0.0.0" if version != "0.0.0" else "0.0.1"
            metadata.write_text(json.dumps({"tag_name": f"v{mismatch}", "assets": []}))
            mismatched = subprocess.run(
                [str(BINARY), "machine", "prepare", profile, "--install"],
                env=env,
                text=True,
                capture_output=True,
                timeout=45,
            )
            if mismatched.returncode == 0 or remote_binary.read_text() != "#!/bin/sh\necho old remote binary\n":
                raise RuntimeError("incompatible release replaced the prior remote binary")

            # Restore a valid checksum then have the real remote shell read one
            # byte. Its staged validation fails, and mv never replaces old.
            metadata.write_text(json.dumps({"tag_name": f"v{version}", "assets": [
                {"name": "SHA256SUMS", "browser_download_url": "fixture://sums"},
                {"name": asset, "browser_download_url": "fixture://archive"},
            ]}))
            sums.write_text(f"{digest}  {asset}\n")
            env["KODADE_BOOTSTRAP_TRUNCATE"] = "1"
            truncated = subprocess.run(
                [str(BINARY), "machine", "prepare", profile, "--install"],
                env=env,
                text=True,
                capture_output=True,
                timeout=45,
            )
            if truncated.returncode == 0 or remote_binary.read_text() != "#!/bin/sh\necho old remote binary\n":
                raise RuntimeError("truncated upload replaced the prior remote binary")
            print(
                "SSH bootstrap passed: missing binary, verified install, checksum and version refusal, "
                "truncated upload, spaced HOME"
            )
        finally:
            if sshd.poll() is None:
                sshd.terminate()
                try:
                    sshd.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    sshd.kill()


if __name__ == "__main__":
    main()
