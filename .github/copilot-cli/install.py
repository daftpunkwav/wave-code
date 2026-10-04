"""Install the pinned GitHub Copilot CLI for the issue-summary workflow.

Reads integrity-pins.json, downloads each tarball, checks the sha512, and
extracts it under node_modules. The native binary is the linux-x64 build
the ubuntu runner executes. No lifecycle scripts run.
"""

from __future__ import annotations

import base64
import hashlib
import io
import json
import shutil
import sys
import tarfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PINS = ROOT / "integrity-pins.json"
MODULES = ROOT / "node_modules"
ATTEMPTS = 3


def main() -> int:
    pins = json.loads(PINS.read_text(encoding="utf-8"))
    packages = pins["packages"]
    if not packages:
        print("integrity-pins.json has no packages", file=sys.stderr)
        return 1
    if MODULES.exists():
        shutil.rmtree(MODULES)
    MODULES.mkdir(parents=True)
    for package in packages:
        install_package(package)
    link_launcher()
    print(f"installed {len(packages)} pinned packages into {MODULES}")
    return 0


def install_package(package: dict) -> None:
    name = package["name"]
    integrity = package["integrity"]
    blob = download(package["url"])
    verify(name, blob, integrity)
    dest = MODULES / name
    extract_npm_tarball(blob, dest)
    if name == "@github/copilot" and not (dest / "npm-loader.js").is_file():
        raise SystemExit("pinned @github/copilot tarball has no npm-loader.js")
    if name == "@github/copilot-linux-x64":
        binary = dest / "copilot"
        if not binary.is_file():
            raise SystemExit("pinned linux-x64 tarball has no copilot binary")
        binary.chmod(binary.stat().st_mode | 0o755)


def download(url: str) -> bytes:
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme != "https" or parsed.hostname != "registry.npmjs.org":
        raise SystemExit(f"refusing url outside registry.npmjs.org: {url}")
    request = urllib.request.Request(url, headers={"User-Agent": "wavecode-ci"})
    last_error: Exception | None = None
    for attempt in range(1, ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.read()
        except (urllib.error.URLError, TimeoutError) as exc:
            last_error = exc
            print(f"download attempt {attempt} failed for {url}: {exc}", file=sys.stderr)
    raise SystemExit(f"failed to download {url}: {last_error}")


def verify(name: str, blob: bytes, integrity: str) -> None:
    algo, separator, digest = integrity.partition("-")
    if separator != "-" or algo != "sha512" or not digest:
        raise SystemExit(f"{name}: unsupported integrity {integrity}")
    expected = base64.b64decode(digest)
    actual = hashlib.sha512(blob).digest()
    if actual != expected:
        raise SystemExit(f"{name}: sha512 does not match the pin")


def extract_npm_tarball(blob: bytes, dest: Path) -> None:
    dest.mkdir(parents=True)
    with tarfile.open(fileobj=io.BytesIO(blob), mode="r:gz") as archive:
        for member in archive.getmembers():
            relative = member_path(member)
            target = dest / relative
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile():
                raise SystemExit(f"refusing non-file tarball member {member.name}")
            source = archive.extractfile(member)
            if source is None:
                raise SystemExit(f"missing tarball payload for {member.name}")
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read())


def member_path(member: tarfile.TarInfo) -> Path:
    name = member.name
    if "\\" in name or name.startswith("/"):
        raise SystemExit(f"refusing tarball member {name}")
    path = Path(name)
    if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != "package":
        raise SystemExit(f"refusing tarball member {member.name}")
    relative = Path(*path.parts[1:])
    if not relative.parts:
        raise SystemExit(f"refusing tarball member {member.name}")
    return relative


def link_launcher() -> None:
    loader = MODULES / "@github/copilot/npm-loader.js"
    loader.chmod(loader.stat().st_mode | 0o755)
    bin_dir = MODULES / ".bin"
    bin_dir.mkdir()
    # A script, not a symlink: the runner executes `copilot` from PATH, and
    # the loader's import.meta.url stays the real npm-loader.js path.
    launcher = bin_dir / "copilot"
    launcher.write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        'dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)\n'
        'exec node "$dir/../@github/copilot/npm-loader.js" "$@"\n',
        encoding="utf-8",
        newline="\n",
    )
    launcher.chmod(0o755)


if __name__ == "__main__":
    sys.exit(main())
