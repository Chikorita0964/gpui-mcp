#!/usr/bin/env python3
"""Check a standalone GPUI Kit consumer against a published GPUI MCP Git commit."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True, help="GPUI MCP Git URL")
    parser.add_argument("--rev", required=True, help="full Git commit SHA")
    parser.add_argument("--target-dir", type=Path, help="optional shared Cargo build cache")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-fA-F]{40}", args.rev):
        parser.error("--rev must be a full 40-character commit SHA")
    repository, revision = json.dumps(args.repository), json.dumps(args.rev)
    env = os.environ.copy()
    if args.target_dir is not None:
        env["CARGO_TARGET_DIR"] = str(args.target_dir.resolve())

    with tempfile.TemporaryDirectory(prefix="gpui-kit-consumer-") as temporary:
        consumer = Path(temporary)
        (consumer / "src").mkdir()
        (consumer / "Cargo.toml").write_text(f'''[package]
name = "gpui-kit-git-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.96"

[workspace]

[dependencies]
gpui-kit = "=0.7.0"
gpui-mcp = {{ git = {repository}, rev = {revision} }}

[patch.crates-io]
gpui-pre = {{ git = {repository}, rev = {revision} }}
''', encoding="utf-8")
        (consumer / "src/main.rs").write_text('''use gpui_mcp::{AppId, BridgeConfig, BridgeHandle};

fn install(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App)
    -> Result<BridgeHandle, Box<dyn std::error::Error>>
{
    Ok(BridgeHandle::install(
        window, cx, BridgeConfig::new(AppId::new("kit-consumer")?, "Kit consumer"),
    )?)
}

fn main() {
    // Type-check real bridge installation with Kit's re-exported GPUI types.
    let _ = install;
}
''', encoding="utf-8")
        subprocess.run(["cargo", "check"], cwd=consumer, env=env, check=True)
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--format-version=1", "--locked"],
            cwd=consumer, env=env, text=True,
        ))
        snapshots = [package for package in metadata["packages"] if package["name"] == "gpui-pre"]
        if len(snapshots) != 1:
            raise SystemExit(f"expected one gpui-pre package; found {len(snapshots)}")
        snapshot = snapshots[0]
        if snapshot["version"] != "0.3.7" or not (snapshot["source"] or "").endswith("#" + args.rev.lower()):
            raise SystemExit("gpui-pre did not resolve to the requested patched Git snapshot")
        resolved = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        names = {package["id"]: package["name"] for package in metadata["packages"]}
        owners = {"gpui-kit", "gpui-base", "gpui-component", "gpui-pre-platform", "gpui-mcp"}
        for owner in sorted(owners):
            packages = [package for package in metadata["packages"] if package["name"] == owner]
            if len(packages) != 1:
                raise SystemExit(f"expected one {owner} package; found {len(packages)}")
            dependencies = resolved[packages[0]["id"]]["deps"]
            gpui = [dep["pkg"] for dep in dependencies if names[dep["pkg"]] in {"gpui", "gpui-pre"}]
            if gpui != [snapshot["id"]]:
                raise SystemExit(f"{owner} does not use exactly the patched gpui-pre package: {gpui}")
        print(f"GPUI Kit 0.7.0: all {len(owners)} GPUI consumers use patched gpui-pre 0.3.7 at {args.rev}")


if __name__ == "__main__":
    main()
