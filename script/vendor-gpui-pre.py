#!/usr/bin/env python3
"""Reproduce the patched gpui-pre snapshot, or verify it without changing files.

Requires Python 3.11+ and GNU patch (available as gpatch on macOS).
Update vendor/gpui-pre.json and the exact Cargo pins before changing snapshots.
Patch conflicts abort before the existing vendor directory is replaced.
"""

import argparse
import hashlib
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "vendor" / "gpui-pre"


def files(directory):
    return {
        path.relative_to(directory): path.read_bytes()
        for path in directory.rglob("*")
        if path.is_file()
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="verify the checked-in snapshot")
    args = parser.parse_args()
    metadata = json.loads((ROOT / "vendor" / "gpui-pre.json").read_text())
    version = metadata["version"]
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    # The workspace depends on the snapshot through its `gpui` name (the library the crate
    # publishes under); the manifest's `package` key names the crates.io package.
    dependency = workspace["workspace"]["dependencies"]["gpui"]
    if dependency.get("package") != "gpui-pre" or dependency["version"] != f"={version}":
        raise SystemExit("workspace gpui pin does not match vendor/gpui-pre.json")
    url = f"https://static.crates.io/crates/gpui-pre/gpui-pre-{version}.crate"
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != metadata["sha256"]:
        raise SystemExit("gpui-pre archive checksum mismatch")

    # Stage beside the destination so replacement stays on the same filesystem.
    with tempfile.TemporaryDirectory(prefix=".gpui-pre-", dir=ROOT / "vendor") as temporary:
        staging = Path(temporary)
        prefix = f"gpui-pre-{version}"
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
            # Published GPUI archives contain regular files and directories only.
            # Validate paths explicitly across the supported Python 3.11+ versions.
            for member in archive.getmembers():
                path = Path(member.name)
                if (
                    path.is_absolute()
                    or ".." in path.parts
                    or not path.parts
                    or path.parts[0] != prefix
                    or not (member.isfile() or member.isdir())
                ):
                    raise SystemExit(f"unexpected archive member: {member.name}")
            archive.extractall(staging)
        source = staging / prefix
        manifest = tomllib.loads((source / "Cargo.toml").read_text())
        snapshot = manifest["package"]["metadata"]["gpui-pre"]
        if snapshot["zed-rev"] != metadata["zed_rev"]:
            raise SystemExit("archive Zed revision does not match vendor/gpui-pre.json")
        # The publisher ships CRLF sources; keep source patches independent of it.
        for path in (source / "src").rglob("*.rs"):
            path.write_bytes(path.read_bytes().replace(b"\r\n", b"\n"))
        (source / "Cargo.lock").unlink(missing_ok=True)
        patch = shutil.which("gpatch") or shutil.which("patch")
        if patch is None:
            raise SystemExit("GNU patch is required")
        subprocess.run(
            [
                patch, "--batch", "--forward", "--fuzz=0", "--no-backup-if-mismatch",
                "-p1", "-i", str(ROOT / "vendor" / "patches" / "gpui-pre.patch"),
            ],
            cwd=source,
            check=True,
        )
        if args.check:
            expected, actual = files(source), files(VENDOR)
            changed = sorted(
                str(path) for path in expected.keys() | actual.keys()
                if expected.get(path) != actual.get(path)
            )
            if changed:
                raise SystemExit("vendored snapshot differs:\n" + "\n".join(changed))
            print(f"gpui-pre {version}: vendored snapshot matches archive plus patches")
        else:
            backup = staging / "previous"
            if VENDOR.exists():
                VENDOR.rename(backup)
            try:
                source.rename(VENDOR)
            except OSError:
                if backup.exists():
                    backup.rename(VENDOR)
                raise
            print(f"gpui-pre {version}: vendored at {VENDOR}")


if __name__ == "__main__":
    main()
