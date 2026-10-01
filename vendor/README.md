# Vendored dependencies

This directory holds patched copies of GPUI. The patches add what the bridge
needs to observe and drive a running app. Each can be dropped once upstream
GPUI ships an equivalent.

| Directory | What it is | Used by |
| --- | --- | --- |
| `gpui/` | GPUI 0.2.2 from Zed commit `16c9aa7ea6d897a8044d9501cde1b295256722f2` | The `zed` backend |
| `gpui-pre/` | The crates.io `gpui-pre` 0.3.7 release (Zed commit `1a28cff4b409169bac058bca40dfbfeb7621d19b`), as used by GPUI Kit 0.7.0 | The `gpui-pre` backend |
| `patches/gpui-pre/` | The patches for each supported `gpui-pre` version | `gpui-pre/` and downstream apps |

Each copy keeps its Apache-2.0 license in `LICENSE-APACHE`.

## `gpui/`

The changes are listed in [`gpui/PATCHES.md`](gpui/PATCHES.md). The ones
present on 2026-09-22 were still unmerged upstream that day. The frame-cost
changes added on 2026-09-23 were written against the same commit.

## `gpui-pre/`

`gpui-pre/` carries the same changes as `gpui/`, so both backends behave the
same way. This includes the parts GPUI Kit depends on:

- Accessibility elements from other crates are observed.
- Clicks and other interactions are still detected through wrapper elements.
- Focusing a Kit input focuses the editor inside it.

The Kit demo's tests cover control states, focus, Unicode text replacement and
redaction.

The copy is the published crate with three changes: the patches are applied,
source files use LF line endings, and the published `Cargo.lock` is removed.

### Supported versions

The bridge accepts `gpui-pre` 0.3.5 to 0.3.7. Each version has its own folder
in `patches/gpui-pre/<version>/` with two patches, applied in this order:

1. `automation.patch`: everything the bridge needs.
2. `font-fallback.patch`: keeps the requested weight and style when a font
   falls back to another family. The bridge doesn't need it, so it can be
   skipped.

The patches differ slightly between versions because the GPUI code they
change differs. 0.3.5 and 0.3.6 don't have the `is_enabled()` helper. In
0.3.5, the view-caching code hasn't been split into helper functions yet.

`gpui-pre.json` lists each version with its download checksum and Zed commit,
and says which version is in `gpui-pre/`.

### Commands

These need Python 3.11+ and GNU patch (`gpatch` on macOS).

```console
# Check that gpui-pre/ matches the published crate plus the patches
python3 script/vendor-gpui-pre.py --check

# Rebuild gpui-pre/ from the published crate
python3 script/vendor-gpui-pre.py

# Write a patched copy of any supported version somewhere else
python3 script/vendor-gpui-pre.py --version 0.3.5 --output <dir>
python3 script/vendor-gpui-pre.py --version 0.3.5 --output <dir> --without font-fallback
```

The script verifies the download's checksum and changes nothing if a patch
fails to apply. It refuses to run if the version range in the workspace
`Cargo.toml` doesn't match `gpui-pre.json`.

### What CI checks

- `gpui-pre/` matches the published crate plus its patches.
- The bridge builds, passes its tests and passes Clippy against `gpui-pre/`,
  against patched 0.3.5 and 0.3.6, and against 0.3.7 without the font patch.
- The Kit demo builds and passes its tests.
- The bridge builds and passes its tests on Rust 1.95, the oldest version it
  supports.
- A fresh app that installs the bridge from the pushed Git commit builds. Kit
  and the bridge must share a single patched `gpui-pre`.

To run that last check yourself against a pushed commit:

```console
python3 script/check-gpui-kit-consumer.py --repository https://github.com/themixednuts/gpui-mcp --rev <full-commit-sha> --target-dir target
```

The Kit demo is a separate workspace. Cargo would otherwise turn on both the
`zed` and `gpui-pre` features of the bridge at once, which is not allowed. For
the same reason, `--all-features` on the main workspace fails on purpose.

### Adding a new version

When GPUI Kit moves to a new `gpui-pre` version:

1. Add the version to `gpui-pre.json` and make it the `vendored` one.
2. Copy the newest folder in `patches/gpui-pre/` to the new version and fix
   any patches that no longer apply.
3. Raise the upper bound of `gpui_pre` in the workspace `Cargo.toml`, and
   update the Kit demo's pin.
4. Add the previous version to the `gpui-pre-range` job in
   `.github/workflows/ci.yml`.
5. Run `python3 script/vendor-gpui-pre.py` and update both lockfiles.
6. Run:

   ```console
   cargo check -p gpui-mcp --no-default-features --features gpui-pre --locked
   cargo test -p gpui-mcp --no-default-features --features gpui-pre,test-support --locked
   cargo test --manifest-path examples/gpui-kit/Cargo.toml --locked
   ```

To drop an old version, delete its folder, remove it from `gpui-pre.json` and
the CI job, and raise the lower bound in `Cargo.toml`.
