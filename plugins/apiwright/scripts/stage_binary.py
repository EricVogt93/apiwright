#!/usr/bin/env python3
"""Build or download the host-native ApiWright MCP binary for the plugin."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
from urllib.request import Request, urlopen


PLUGIN_ROOT = Path(__file__).resolve().parents[1]
REPOSITORY_ROOT = PLUGIN_ROOT.parents[1]
REPOSITORY_URL = "https://github.com/EricVogt93/apiwright"


def local_build() -> Path:
    build = subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "-p",
            "forge-cli",
            "--message-format=json-render-diagnostics",
        ],
        cwd=REPOSITORY_ROOT,
        stdout=subprocess.PIPE,
        text=True,
        check=True,
    )
    source = None
    for line in build.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            message.get("reason") == "compiler-artifact"
            and message.get("target", {}).get("name") == "apiwright"
            and message.get("executable")
        ):
            source = Path(message["executable"])
    if source is None:
        raise RuntimeError("Cargo did not report the apiwright executable")
    return source


def release_target(system: str, machine: str) -> tuple[str, str]:
    normalized = (system.lower(), machine.lower())
    targets = {
        ("linux", "x86_64"): ("linux-x86_64", "apiwright"),
        ("linux", "amd64"): ("linux-x86_64", "apiwright"),
        ("windows", "x86_64"): ("windows-x86_64", "apiwright.exe"),
        ("windows", "amd64"): ("windows-x86_64", "apiwright.exe"),
        ("darwin", "arm64"): ("macOS-arm64", "apiwright"),
        ("darwin", "aarch64"): ("macOS-arm64", "apiwright"),
    }
    try:
        return targets[normalized]
    except KeyError as error:
        raise RuntimeError(
            f"No ApiWright release binary for {system} {machine}"
        ) from error


def plugin_version() -> str:
    manifest = json.loads(
        (PLUGIN_ROOT / ".codex-plugin" / "plugin.json").read_text(encoding="utf-8")
    )
    return manifest["version"].split("+", 1)[0]


def expected_digest(checksums: str, asset: str) -> str:
    for line in checksums.splitlines():
        fields = line.split(maxsplit=1)
        if len(fields) == 2 and fields[1].lstrip("*") == asset:
            return fields[0]
    raise RuntimeError(f"{asset} is missing from SHA256SUMS.txt")


def fetch(url: str):
    return urlopen(Request(url, headers={"User-Agent": "ApiWright plugin"}), timeout=30)


def download_release() -> Path:
    version = plugin_version()
    target, executable = release_target(platform.system(), platform.machine())
    suffix = ".exe" if executable.endswith(".exe") else ""
    asset = f"ApiWright-{version}-cli-{target}{suffix}"
    release = f"{REPOSITORY_URL}/releases/download/v{version}"
    with fetch(f"{release}/SHA256SUMS.txt") as response:
        digest = expected_digest(response.read().decode("utf-8"), asset)

    destination = PLUGIN_ROOT / "bin" / executable
    destination.parent.mkdir(exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{destination.name}.", dir=destination.parent
    )
    temporary = Path(temporary_name)
    actual = hashlib.sha256()
    try:
        with os.fdopen(descriptor, "wb") as output, fetch(f"{release}/{asset}") as response:
            while chunk := response.read(1024 * 1024):
                output.write(chunk)
                actual.update(chunk)
            output.flush()
            os.fsync(output.fileno())
        if actual.hexdigest() != digest:
            raise RuntimeError(f"Checksum mismatch for {asset}")
        temporary.chmod(temporary.stat().st_mode | 0o111)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def stage_local_build() -> Path:
    source = local_build()
    destination = PLUGIN_ROOT / "bin" / source.name
    destination.parent.mkdir(exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{destination.name}.", dir=destination.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output, source.open("rb") as input_file:
            shutil.copyfileobj(input_file, output)
            output.flush()
            os.fsync(output.fileno())
        temporary.chmod(source.stat().st_mode | 0o111)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--download",
        action="store_true",
        help="download and verify the matching GitHub release instead of building locally",
    )
    args = parser.parse_args()
    destination = download_release() if args.download else stage_local_build()
    print(f"Staged {destination}")


if __name__ == "__main__":
    main()
