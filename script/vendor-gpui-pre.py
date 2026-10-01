#!/usr/bin/env python3
"""Reproduce a patched gpui-pre snapshot, or verify the vendored one without changing files.

Requires Python 3.11+ and GNU patch (available as gpatch on macOS).
Every supported crates.io release is listed in vendor/gpui-pre.json with its own
patch series under vendor/patches/gpui-pre/<version>/. Without --output, the
vendored release is written to vendor/gpui-pre. With --output, any supported
release is written to that directory, for example a downstream vendor tree.
Patch conflicts abort before an existing destination is replaced.
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
PATCHES = ROOT / "vendor" / "patches" / "gpui-pre"
# Applied in this order. Only the patches named in OPTIONAL may be left out.
SERIES = ("automation", "font-fallback")
OPTIONAL = ("font-fallback",)


def files(directory):
    return {
        path.relative_to(directory): path.read_bytes()
        for path in directory.rglob("*")
        if path.is_file()
    }


def version_key(version):
    return tuple(int(part) for part in version.split("."))


def check_dependency_range(versions):
    """The bridge must accept exactly the releases that have a patch series."""
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    dependency = workspace["workspace"]["dependencies"]["gpui_pre"]
    ordered = sorted(versions, key=version_key)
    expected = f">={ordered[0]}, <={ordered[-1]}"
    if dependency["version"] != expected:
        raise SystemExit(
            f"workspace gpui_pre requirement {dependency['version']!r} does not match "
            f"the versions in vendor/gpui-pre.json; expected {expected!r}"
        )
    for version in ordered:
        for name in SERIES:
            if not (PATCHES / version / f"{name}.patch").is_file():
                raise SystemExit(f"missing vendor/patches/gpui-pre/{version}/{name}.patch")


def is_gpui_pre_crate(directory):
    manifest = directory / "Cargo.toml"
    if not manifest.is_file():
        return False
    try:
        return tomllib.loads(manifest.read_text())["package"]["name"] == "gpui-pre"
    except (tomllib.TOMLDecodeError, KeyError):
        return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="verify the checked-in snapshot")
    parser.add_argument("--version", help="supported gpui-pre release (default: the vendored one)")
    parser.add_argument("--output", type=Path, help="write the patched crate here instead of vendor/gpui-pre")
    parser.add_argument(
        "--without", action="append", default=[], choices=OPTIONAL,
        help="leave out an optional patch (repeatable)",
    )
    args = parser.parse_args()
    metadata = json.loads((ROOT / "vendor" / "gpui-pre.json").read_text())
    supported = metadata["versions"]
    check_dependency_range(supported)
    version = args.version or metadata["vendored"]
    if version not in supported:
        raise SystemExit(
            f"gpui-pre {version} has no patch series; supported: "
            + ", ".join(sorted(supported, key=version_key))
        )
    if args.output is None and (version != metadata["vendored"] or args.without):
        parser.error("vendor/gpui-pre holds the full series for the vendored release; use --output")
    if args.check and args.output is not None:
        parser.error("--check verifies vendor/gpui-pre and cannot be combined with --output")
    destination = (args.output or VENDOR).resolve()
    if destination.exists() and not is_gpui_pre_crate(destination):
        raise SystemExit(f"refusing to replace {destination}: it is not a gpui-pre crate")
    release = supported[version]

    url = f"https://static.crates.io/crates/gpui-pre/gpui-pre-{version}.crate"
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != release["sha256"]:
        raise SystemExit("gpui-pre archive checksum mismatch")

    # Stage beside the destination so replacement stays on the same filesystem.
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".gpui-pre-", dir=destination.parent) as temporary:
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
        if snapshot["zed-rev"] != release["zed_rev"]:
            raise SystemExit("archive Zed revision does not match vendor/gpui-pre.json")
        # The publisher ships CRLF sources; keep source patches independent of it.
        for path in (source / "src").rglob("*.rs"):
            path.write_bytes(path.read_bytes().replace(b"\r\n", b"\n"))
        (source / "Cargo.lock").unlink(missing_ok=True)
        patch = shutil.which("gpatch") or shutil.which("patch")
        if patch is None:
            raise SystemExit("GNU patch is required")
        applied = [name for name in SERIES if name not in args.without]
        for name in applied:
            subprocess.run(
                [
                    patch, "--batch", "--forward", "--fuzz=0", "--no-backup-if-mismatch",
                    "-p1", "-i", str(PATCHES / version / f"{name}.patch"),
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
            if destination.exists():
                destination.rename(backup)
            try:
                source.rename(destination)
            except OSError:
                if backup.exists():
                    backup.rename(destination)
                raise
            print(f"gpui-pre {version} ({', '.join(applied)}): written to {destination}")


if __name__ == "__main__":
    main()
