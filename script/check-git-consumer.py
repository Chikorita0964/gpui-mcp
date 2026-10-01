#!/usr/bin/env python3
"""Check a standalone app that installs the bridge from a published GPUI MCP Git commit.

The app follows the README's recipe for one backend: GPUI Kit on gpui-pre, or
gpui-ce with its platform crate. It type-checks BridgeHandle::install with that
backend's Window and App, and verifies that the app's GPUI crates and the bridge
all share the one patched GPUI package from the requested commit.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


# For each backend: the app's dependencies, the crates.io GPUI package that the
# Git patch replaces, the path the app reaches GPUI's types through, and every
# package that must link that one patched GPUI.
BACKENDS = {
    "gpui-kit": {
        "dependencies": 'gpui-kit = "=0.7.0"',
        "gpui": ("gpui-pre", "0.3.7"),
        "types": "gpui_kit",
        "owners": {"gpui-kit", "gpui-base", "gpui-component", "gpui-pre-platform", "gpui-mcp"},
    },
    "gpui-ce": {
        "dependencies": 'gpui-ce = "=0.2.2"\ngpui_ce_platform = "=0.1.0"',
        "gpui": ("gpui-ce", "0.2.2"),
        "types": "gpui",
        "owners": {"gpui_ce_platform", "gpui-mcp"},
    },
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=BACKENDS, default="gpui-kit", help="which recipe to check")
    parser.add_argument("--repository", required=True, help="GPUI MCP Git URL")
    parser.add_argument("--rev", required=True, help="full Git commit SHA")
    parser.add_argument("--target-dir", type=Path, help="optional shared Cargo build cache")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-fA-F]{40}", args.rev):
        parser.error("--rev must be a full 40-character commit SHA")
    backend = BACKENDS[args.backend]
    gpui_package, gpui_version = backend["gpui"]
    feature = "gpui-ce" if args.backend == "gpui-ce" else "gpui-pre"
    repository, revision = json.dumps(args.repository), json.dumps(args.rev)
    env = os.environ.copy()
    if args.target_dir is not None:
        env["CARGO_TARGET_DIR"] = str(args.target_dir.resolve())

    with tempfile.TemporaryDirectory(prefix=f"{args.backend}-consumer-") as temporary:
        consumer = Path(temporary)
        (consumer / "src").mkdir()
        (consumer / "Cargo.toml").write_text(f'''[package]
name = "{args.backend}-git-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.96"

[workspace]

[dependencies]
{backend["dependencies"]}
gpui-mcp = {{ git = {repository}, rev = {revision}, default-features = false, features = ["{feature}"] }}

[patch.crates-io]
{gpui_package} = {{ git = {repository}, rev = {revision} }}
''', encoding="utf-8")
        types = backend["types"]
        (consumer / "src/main.rs").write_text(f'''use gpui_mcp::{{AppId, BridgeConfig, BridgeHandle}};

fn install(window: &mut {types}::Window, cx: &mut {types}::App)
    -> Result<BridgeHandle, Box<dyn std::error::Error>>
{{
    Ok(BridgeHandle::install(
        window, cx, BridgeConfig::new(AppId::new("git-consumer")?, "Git consumer"),
    )?)
}}

fn main() {{
    // Type-check real bridge installation with the backend's GPUI types.
    let _ = install;
}}
''', encoding="utf-8")
        subprocess.run(["cargo", "check"], cwd=consumer, env=env, check=True)
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--format-version=1", "--locked"],
            cwd=consumer, env=env, text=True,
        ))
        snapshots = [package for package in metadata["packages"] if package["name"] == gpui_package]
        if len(snapshots) != 1:
            raise SystemExit(f"expected one {gpui_package} package; found {len(snapshots)}")
        snapshot = snapshots[0]
        if snapshot["version"] != gpui_version or not (snapshot["source"] or "").endswith("#" + args.rev.lower()):
            raise SystemExit(f"{gpui_package} did not resolve to the requested patched Git snapshot")
        resolved = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        names = {package["id"]: package["name"] for package in metadata["packages"]}
        gpui_names = {"gpui", "gpui-pre", "gpui-ce"}
        owners = backend["owners"]
        for owner in sorted(owners):
            packages = [package for package in metadata["packages"] if package["name"] == owner]
            if len(packages) != 1:
                raise SystemExit(f"expected one {owner} package; found {len(packages)}")
            dependencies = resolved[packages[0]["id"]]["deps"]
            gpui = [dep["pkg"] for dep in dependencies if names[dep["pkg"]] in gpui_names]
            if gpui != [snapshot["id"]]:
                raise SystemExit(f"{owner} does not use exactly the patched {gpui_package} package: {gpui}")
        print(
            f"{args.backend}: all {len(owners)} GPUI consumers use patched "
            f"{gpui_package} {gpui_version} at {args.rev}"
        )


if __name__ == "__main__":
    main()
