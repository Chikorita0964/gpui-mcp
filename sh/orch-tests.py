#!/usr/bin/env python3
"""gpui-mcp's test runner for the team orchestrator (ccb-team-kit lib/orch/runner.py, team design D41).

    python3 sh/orch-tests.py [--files F...] [SUITE...]     no arguments: every suite
    ALL_PLAN=<file>         write {"suites": [{name, key, cached, secs}]} and run nothing
    ALL_RESULT_JSON=<file>  run, then write {"suites": [...], "failed": [...], "flaky": [], "rc": 0|1}
    ALL_CACHE=0             do not reuse passes (they are still recorded)

The suites: one per workspace package (`cargo test -p <package>`, with that package's own CI
features), `clippy` (the CI command) and `fmt` (`cargo fmt --all -- --check`). `--files` selects the
packages whose directory holds a changed file, plus every package that depends on them, and
`clippy`/`fmt` for any Rust change; a workspace manifest, the lock file, the toolchain or vendor/
selects everything; docs select nothing; an unknown path selects everything.

A suite's key is a sha256 over its inputs' git blob hashes (its package and the packages it depends
on, the workspace manifests, vendor/'s tree); a pass is recorded per key under
~/.cache/gpui-mcp/orch-tests/, with the last 20 run times (the median is the plan's `secs`). Every
build shares ONE target directory, the main checkout's `target/` (CARGO_TARGET_DIR): a card's
worktree does not build gpui from scratch (target/ is ~28 GB), and cargo's own lock serializes
concurrent builds. Each suite is stopped past five times its median (at least 600 s; 3600 s
without history). Standard library only.
"""
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path.cwd()
CACHE = Path(os.environ.get("ORCH_TESTS_CACHE") or Path.home() / ".cache" / "gpui-mcp" / "orch-tests")
CI_FEATURES = {"gpui-mcp": "test-support", "gpui-mcp-html": "dev-watch,visual-parity"}
EVERYTHING = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/", "vendor/", "sh/orch-tests.py")
NOTHING = ("docs/", "README.md", "LICENSE", "SECURITY.md", "CLAUDE.md", ".github/")
MIN_BOUND, NO_HISTORY_BOUND = 600, 3600


def git(*args, **kw):
    return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True, **kw)


def main_checkout() -> Path:
    r = git("rev-parse", "--path-format=absolute", "--git-common-dir")
    return Path(r.stdout.strip()).parent if r.returncode == 0 and r.stdout.strip() else ROOT


def packages() -> dict:
    """{name: {"dir": relative dir, "deps": {workspace package names it depends on}}}."""
    r = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"],
                       cwd=str(ROOT), capture_output=True, text=True, timeout=120)
    data = json.loads(r.stdout)
    members = set(data["workspace_members"])
    out = {}
    for p in data["packages"]:
        if p["id"] in members:
            d = Path(p["manifest_path"]).parent.relative_to(ROOT).as_posix()
            out[p["name"]] = {"dir": d, "deps": {x["name"] for x in p["dependencies"] if x.get("path")}}
    for p in out.values():
        p["deps"] &= set(out)
    return out


def closure(pkgs, name) -> set:
    """The package and everything it depends on inside the workspace."""
    seen, todo = set(), [name]
    while todo:
        n = todo.pop()
        if n not in seen:
            seen.add(n)
            todo += pkgs[n]["deps"]
    return seen


def dependents(pkgs, names) -> set:
    return {n for n in pkgs if closure(pkgs, n) & set(names)}


def select(pkgs, files) -> list:
    every = sorted(pkgs) + ["clippy", "fmt"]
    hit = set()
    for f in files:
        if f.startswith(EVERYTHING) or f in EVERYTHING:
            return every
        if f.startswith(NOTHING) or f in NOTHING:
            continue
        owner = next((n for n, p in pkgs.items() if f.startswith(p["dir"] + "/")), None)
        if owner is None:
            return every          # a path the map does not know: never skip what it might break
        hit |= dependents(pkgs, [owner])
        if f.endswith(".rs"):
            hit |= {"clippy", "fmt"}
    return [s for s in every if s in hit]


def blob_hashes(paths) -> dict:
    files = [p for p in git("ls-files", "-co", "--exclude-standard", "--", *paths).stdout.splitlines()
             if (ROOT / p).is_file()]
    if not files:
        return {}
    h = subprocess.run(["git", "-C", str(ROOT), "hash-object", "--stdin-paths"], input="\n".join(files),
                       capture_output=True, text=True).stdout.split()
    return dict(zip(files, h))


def key(pkgs, suite) -> str:
    if suite in pkgs:
        paths = [pkgs[n]["dir"] for n in sorted(closure(pkgs, suite))]
    else:
        paths = [p["dir"] for p in pkgs.values()]
    paths += ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "sh/orch-tests.py"]
    vendor = git("rev-parse", "HEAD:vendor").stdout.strip() + git("status", "--porcelain", "--", "vendor").stdout
    digest = hashlib.sha256(f"{suite}\n{vendor}\n".encode())
    for p, h in sorted(blob_hashes(paths).items()):
        digest.update(f"{p} {h}\n".encode())
    return digest.hexdigest()[:24]


def median(suite):
    try:
        v = sorted(float(x) for x in (CACHE / suite / "durations").read_text().split())
    except (OSError, ValueError):
        return None
    return v[(len(v) - 1) // 2] if v else None


def command(pkgs, suite) -> list:
    if suite == "fmt":
        return ["cargo", "fmt", "--all", "--", "--check"]
    if suite == "clippy":
        feats = ",".join(f"{p}/{f}" for p, fs in CI_FEATURES.items() for f in fs.split(","))
        return ["cargo", "clippy", "--workspace", "--all-targets", "--features", feats, "--locked", "--",
                "-D", "warnings"]
    cmd = ["cargo", "test", "-p", suite, "--locked"]
    if suite in CI_FEATURES:
        cmd += ["--features", CI_FEATURES[suite]]
    return cmd


def run_suite(pkgs, suite, k) -> dict:
    m = median(suite)
    bound = max(MIN_BOUND, 5 * m) if m else NO_HISTORY_BOUND
    env = {**os.environ}
    env.setdefault("CARGO_TARGET_DIR", str(main_checkout() / "target"))
    t0 = time.time()
    try:
        r = subprocess.run(command(pkgs, suite), cwd=str(ROOT), env=env, capture_output=True, text=True,
                           timeout=bound)
        rc, out = r.returncode, r.stdout + r.stderr
    except subprocess.TimeoutExpired:
        rc, out = 124, f"TIMEOUT: {suite} ran past {bound:.0f}s"
    secs = round(time.time() - t0, 1)
    d = CACHE / suite
    d.mkdir(parents=True, exist_ok=True)
    if rc != 124:
        hist = (d / "durations").read_text().split()[-19:] if (d / "durations").is_file() else []
        (d / "durations").write_text("\n".join(hist + [str(secs)]) + "\n")
    if rc == 0:
        (d / k).write_text("1\n")
    checks = out.count("test result: ok") if suite in pkgs else int(rc == 0)
    print(f"  {suite:<22} {'ok' if rc == 0 else f'FAIL (exit {rc})':<16} {secs:>7}s", flush=True)
    if rc != 0:
        print("\n".join(out.splitlines()[-40:]), flush=True)
    return {"name": suite, "rc": int(rc != 0), "checks": checks, "secs": secs, "cached": False}


def main(argv) -> int:
    pkgs = packages()
    if argv[:1] == ["--files"]:
        suites = select(pkgs, argv[1:])
    else:
        suites = [s for s in argv if s in pkgs or s in ("clippy", "fmt")] or sorted(pkgs) + ["clippy", "fmt"]
    use_cache = os.environ.get("ALL_CACHE", "1") != "0"
    keys = {s: key(pkgs, s) for s in suites}
    cached = {s: use_cache and (CACHE / s / keys[s]).is_file() for s in suites}
    if os.environ.get("ALL_PLAN"):
        Path(os.environ["ALL_PLAN"]).write_text(json.dumps({"suites": [
            {"name": s, "key": keys[s], "cached": cached[s], "secs": median(s)} for s in suites]}))
        return 0
    rows = []
    for s in suites:
        if cached[s]:
            print(f"  {s:<22} ok (cached)")
            rows.append({"name": s, "rc": 0, "checks": 1, "secs": 0.0, "cached": True})
        else:
            rows.append(run_suite(pkgs, s, keys[s]))
    failed = [r["name"] for r in rows if r["rc"]]
    if os.environ.get("ALL_RESULT_JSON"):
        Path(os.environ["ALL_RESULT_JSON"]).write_text(json.dumps(
            {"suites": rows, "failed": failed, "flaky": [], "rc": int(bool(failed))}))
    print(f"sh/orch-tests.py: {len(rows)} suites, {len(failed)} failed")
    return int(bool(failed))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
