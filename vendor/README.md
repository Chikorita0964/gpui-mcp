# Vendored dependencies

`gpui/` is GPUI 0.2.2 from Zed commit
`16c9aa7ea6d897a8044d9501cde1b295256722f2`. It carries the downstream source
changes inventoried in `gpui/PATCHES.md`. Those present on 2026-09-22 were
checked against upstream that day and were still unmerged there; the frame-cost
changes added on 2026-09-23 were written against the same commit.

The crate's Apache-2.0 license is retained in `gpui/LICENSE-APACHE`.

`gpui-pre/` is the crates.io `gpui-pre` 0.3.7 snapshot of Zed commit
`1a28cff4b409169bac058bca40dfbfeb7621d19b`, matching GPUI Kit 0.7.0. It carries
the same nine automation changes, applied with
`patches/gpui-pre.patch`. The accessibility activation patch retains this
snapshot's `is_enabled()` helper and adds `platform_is_active()` alongside it.
Both backends observe third-party accessibility elements and preserve wrapped
Div interaction provenance. Semantic focus honors AccessKit Focus listeners,
so Kit's outer input frame focuses its inner editor. Kit tests cover control
states, this focus redirection, Unicode text replacement, and redaction through
its test-support wrappers.
The published manifest, including its renamed snapshot dependencies and
`gpui` library name, is retained. Source line endings are normalized, the
published Cargo.lock is omitted, and the Apache-2.0 license is retained in
`gpui-pre/LICENSE-APACHE`.

## Updating the GPUI Kit snapshot

The version, archive SHA-256 and original Zed revision are recorded in
`gpui-pre.json`. Python 3.11+ and GNU patch reproduce the vendored copy:

```console
python3 script/vendor-gpui-pre.py --check
python3 script/vendor-gpui-pre.py
```

`--check` downloads the checksum-verified archive, applies patches in a
temporary directory, and compares every file with the vendor tree. The update
command replaces the vendor tree only after the patches apply successfully.

When Kit changes its exact GPUI pin, update `gpui-pre.json` from that crates.io
release, the workspace's `gpui_pre` dependency and the Kit demo's dependency
pin together. Rebase `patches/gpui-pre.patch` if needed, regenerate the vendor
tree and both lockfiles, then run:

```console
cargo check -p gpui-mcp --no-default-features --features gpui-pre --locked
cargo test -p gpui-mcp --no-default-features --features gpui-pre,test-support --locked
cargo test --manifest-path examples/gpui-kit/Cargo.toml --locked
```

The Kit demo is an independent workspace so Cargo cannot unify its
`gpui-pre` bridge feature with the Zed examples' `zed` feature. CI checks both
backends separately; `--all-features` on the main workspace would select both
and is intentionally rejected.

Remove the patch in the workspace `Cargo.toml` and this directory once an
upstream GPUI release includes these behaviors. Each change can be dropped
independently after its upstream equivalent ships.
