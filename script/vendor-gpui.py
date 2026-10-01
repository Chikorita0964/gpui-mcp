#!/usr/bin/env python3
"""Reproduce a patched crates.io GPUI crate, or verify the vendored one without changing files.

Supports gpui-pre (GPUI Kit, gpui-component) and gpui-ce (the community fork).
Requires Python 3.11+ and GNU patch (available as gpatch on macOS).
Every supported release of a crate is listed in vendor/<crate>.json with its own
patch series under vendor/patches/<crate>/<version>/. Without --output, the
vendored release is written to vendor/<crate>. With --output, any supported
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
# Each crate's name on crates.io, mapped to its workspace dependency key.
CRATES = {"gpui-pre": "gpui_pre", "gpui-ce": "gpui_ce"}
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


def check_dependency_range(crate, versions, patches):
    """The bridge must accept exactly the releases that have a patch series."""
    key = CRATES[crate]
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    dependency = workspace["workspace"]["dependencies"][key]
    ordered = sorted(versions, key=version_key)
    if len(ordered) == 1:
        expected = f"={ordered[0]}"
    else:
        expected = f">={ordered[0]}, <={ordered[-1]}"
    if dependency["version"] != expected:
        raise SystemExit(
            f"workspace {key} requirement {dependency['version']!r} does not match "
            f"the versions in vendor/{crate}.json; expected {expected!r}"
        )
    for version in ordered:
        for name in SERIES:
            if not (patches / version / f"{name}.patch").is_file():
                raise SystemExit(f"missing vendor/patches/{crate}/{version}/{name}.patch")


def is_crate(directory, crate):
    manifest = directory / "Cargo.toml"
    if not manifest.is_file():
        return False
    try:
        return tomllib.loads(manifest.read_text())["package"]["name"] == crate
    except (tomllib.TOMLDecodeError, KeyError):
        return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crate", choices=CRATES, default="gpui-pre", help="crate to patch (default: gpui-pre)")
    parser.add_argument("--check", action="store_true", help="verify the checked-in snapshot")
    parser.add_argument("--version", help="supported release (default: the vendored one)")
    parser.add_argument("--output", type=Path, help="write the patched crate here instead of vendor/<crate>")
    parser.add_argument(
        "--without", action="append", default=[], choices=OPTIONAL,
        help="leave out an optional patch (repeatable)",
    )
    args = parser.parse_args()
    crate = args.crate
    vendor = ROOT / "vendor" / crate
    patches = ROOT / "vendor" / "patches" / crate
    metadata = json.loads((ROOT / "vendor" / f"{crate}.json").read_text())
    supported = metadata["versions"]
    check_dependency_range(crate, supported, patches)
    version = args.version or metadata["vendored"]
    if version not in supported:
        raise SystemExit(
            f"{crate} {version} has no patch series; supported: "
            + ", ".join(sorted(supported, key=version_key))
        )
    if args.output is None and (version != metadata["vendored"] or args.without):
        parser.error(f"vendor/{crate} holds the full series for the vendored release; use --output")
    if args.check and args.output is not None:
        parser.error(f"--check verifies vendor/{crate} and cannot be combined with --output")
    destination = (args.output or vendor).resolve()
    if destination.exists() and not is_crate(destination, crate):
        raise SystemExit(f"refusing to replace {destination}: it is not a {crate} crate")
    release = supported[version]

    url = f"https://static.crates.io/crates/{crate}/{crate}-{version}.crate"
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != release["sha256"]:
        raise SystemExit(f"{crate} archive checksum mismatch")

    # Stage beside the destination so replacement stays on the same filesystem.
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f".{crate}-", dir=destination.parent) as temporary:
        staging = Path(temporary)
        prefix = f"{crate}-{version}"
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
        # gpui-pre records the Zed commit it was cut from; gpui-ce does not.
        if "zed_rev" in release:
            manifest = tomllib.loads((source / "Cargo.toml").read_text())
            snapshot = manifest["package"]["metadata"]["gpui-pre"]
            if snapshot["zed-rev"] != release["zed_rev"]:
                raise SystemExit(f"archive Zed revision does not match vendor/{crate}.json")
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
                    "-p1", "-i", str(patches / version / f"{name}.patch"),
                ],
                cwd=source,
                check=True,
            )
        if args.check:
            expected, actual = files(source), files(vendor)
            changed = sorted(
                str(path) for path in expected.keys() | actual.keys()
                if expected.get(path) != actual.get(path)
            )
            if changed:
                raise SystemExit("vendored snapshot differs:\n" + "\n".join(changed))
            print(f"{crate} {version}: vendored snapshot matches archive plus patches")
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
            print(f"{crate} {version} ({', '.join(applied)}): written to {destination}")


if __name__ == "__main__":
    main()
