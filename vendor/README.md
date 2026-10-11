# Vendored dependencies

This directory holds patched copies of GPUI. The patches add what the bridge
needs to observe and drive a running app. Each can be dropped once upstream
GPUI ships an equivalent.

| Directory                    | What it is                                                                                                                    | Used by                                                   |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| `gpui/`                      | GPUI 0.2.2 from Zed commit `16c9aa7ea6d897a8044d9501cde1b295256722f2`                                                         | The `zed` backend                                         |
| `gpui-pre/`                  | The crates.io `gpui-pre` 0.3.8 release (Zed commit `279fe070bb389b79652e52065b2f001edcc0b11b`), as used by GPUI Kit           | The `gpui-pre` backend                                    |
| `gpui-fast/`                 | The crates.io `gpui-fast` 0.1.6 release, Longbridge's GPUI with retained rendering, as GPUI Kit's `gpui-fast` feature uses it | The `gpui-fast` backend                                   |
| `gpui-ce/`                   | The crates.io `gpui-ce` 0.2.2 release, the community fork of GPUI                                                             | The `gpui-ce` backend                                     |
| `patches/<crate>/<version>/` | The patches for each supported `gpui-pre`, `gpui-fast` and `gpui-ce` version                                                  | `gpui-pre/`, `gpui-fast/`, `gpui-ce/` and downstream apps |

Each copy keeps its Apache-2.0 license file (`LICENSE-APACHE`, or `LICENSE` in
`gpui-fast/`).

## `gpui/`

The changes are listed in [`gpui/PATCHES.md`](gpui/PATCHES.md). The ones
present on 2026-09-22 were still unmerged upstream that day. The frame-cost
changes added on 2026-09-23 were written against the same commit.

## `gpui-pre/`, `gpui-fast/` and `gpui-ce/`

All three carry the same changes as `gpui/`, so every backend behaves the same
way. This includes the parts GPUI Kit depends on:

- Accessibility elements from other crates are observed.
- Clicks and other interactions are still detected through wrapper elements.
- Focusing a Kit input focuses the editor inside it.

The Kit demo's tests cover control states, focus, Unicode text replacement and
redaction.

Each copy is the published crate with three changes: the patches are applied,
source files use LF line endings, and the published `Cargo.lock` is removed.

### Supported versions

| Crate       | Versions | Vendored here |
| ----------- | -------- | ------------- |
| `gpui-pre`  | 0.3.8    | 0.3.8         |
| `gpui-fast` | 0.1.6    | 0.1.6         |
| `gpui-ce`   | 0.2.2    | 0.2.2         |

Each version has its own folder in `patches/<crate>/<version>/` with these
patches, applied in this order (`gpui-fast` has no `font-fallback.patch`: it
already keeps a fallback font's requested weight, style and features; only
`gpui-fast` has `accessibility-actions.patch`):

1. `automation.patch`: everything the bridge needs.
2. `accessibility-actions.patch`: `Window::perform_a11y_action`, which runs an
   accessibility action through the window's own AccessKit handling,
   `Window::a11y_action_is_handled` and `Window::a11y_node_id`.
   `perform_action`'s semantic actions need it; every other backend refuses
   them.
3. `font-fallback.patch`: keeps the requested weight and style when a font
   falls back to another family. The bridge doesn't need it, so it can be
   skipped.
4. `grid.patch`: CSS grid track lists. Upstream GPUI only offers
   `grid_cols(n)` / `grid_rows(n)`, which always mean
   `repeat(n, minmax(0, 1fr))`, although taffy implements full CSS grid. This
   adds `GridTrack`, `GridTrackSize`, `GridTrackBreadth`, `GridRepetition` and
   `GridAutoFlow`, and the `Styled` builders `grid_template_columns`,
   `grid_template_rows`, `grid_auto_columns`, `grid_auto_rows`,
   `grid_auto_flow`, `grid_column` and `grid_row`. `gpui-mcp-html` needs it to
   render `grid-template-*`; the bridge doesn't, so it can be skipped. The Zed
   tree in `gpui/` carries the same change (see
   [`gpui/PATCHES.md`](gpui/PATCHES.md)).

The `gpui-ce` patches differ slightly because the GPUI code they change
differs: `gpui-ce` has no `is_enabled()` helper, its view-caching code hasn't
been split into helper functions, and it has no touch gestures, so it has no
touch changes and shows tooltips from a different spot in `div.rs`.

The `gpui-fast` automation patch follows GPUI Fast where it moved upstream
code: views draw through its retained rendering, which draws nothing again from
last frame while the accessibility tree that a frame observer turns on is built,
so an observed frame renders every view and reports a cached view's cause as
`CachingDisabled`; and its input-change snapshot is taken before the pointer
moves, so a view that read the pointer is rendered again when it moves.

`gpui-pre.json`, `gpui-fast.json` and `gpui-ce.json` list each version with its
download checksum, and say which version is vendored here. For `gpui-pre`, they also
record the Zed commit the release was cut from.

### Commands

Run these from a checkout of this repository. They only need Rust: `xtask` is
a small tool in this workspace that downloads the crate and applies the
patches itself. `--crate` defaults to `gpui-pre`, and `cargo xtask --help`
lists every option.

```console
# Check that a vendored copy matches the published crate plus the patches
cargo xtask vendor --crate gpui-pre --check
cargo xtask vendor --crate gpui-fast --check
cargo xtask vendor --crate gpui-ce --check

# Rebuild a vendored copy from the published crate
cargo xtask vendor --crate gpui-ce

# Write a patched copy somewhere else
cargo xtask vendor --crate gpui-pre --output <dir>
cargo xtask vendor --crate gpui-pre --output <dir> --without font-fallback
cargo xtask vendor --crate gpui-pre --output <dir> --without font-fallback --without grid
```

The script verifies the download's checksum and changes nothing if a patch
fails to apply. It refuses to run if a crate's version requirement in the
workspace `Cargo.toml` doesn't match its `.json` file.

### What CI checks

- The three vendored copies match the published crate plus their patches.
- The bridge builds, passes its tests and passes Clippy on every OS against
  each vendored copy.
- On Linux it does the same against `gpui-pre` 0.3.8 and `gpui-ce` 0.2.2
  without the font patch, and against `gpui-pre` 0.3.8 and `gpui-fast` 0.1.6
  without the grid patch.
- `gpui-mcp-html` builds, passes its tests and passes Clippy against the
  vendored `gpui-pre` as well as the Zed tree.
- The Kit demo builds and passes its tests on `gpui-pre` and on GPUI Fast.
- Fresh apps that install the bridge from the pushed Git commit build: GPUI Kit,
  GPUI Kit on GPUI Fast, and `gpui-ce`. In each, the app's GPUI crates and the bridge
  must share a single patched GPUI.

To run that last check yourself against a pushed commit:

```console
cargo xtask check-consumer --backend gpui-kit --repository https://github.com/themixednuts/gpui-mcp --rev <full-commit-sha> --target-dir target
cargo xtask check-consumer --backend gpui-kit-fast --repository https://github.com/themixednuts/gpui-mcp --rev <full-commit-sha> --target-dir target
cargo xtask check-consumer --backend gpui-ce --repository https://github.com/themixednuts/gpui-mcp --rev <full-commit-sha> --target-dir target
```

The Kit demo is a separate workspace. Cargo would otherwise turn on both the
`zed` and `gpui-pre` features of the bridge at once, which is not allowed. For
the same reason, `--all-features` on the main workspace fails on purpose.

### Adding a new version

When GPUI Kit or `gpui-ce` moves to a new version:

1. Add the version to `<crate>.json` and make it the `vendored` one.
2. Copy the newest folder in `patches/<crate>/` to the new version and fix
   any patches that no longer apply.
3. Update the crate's version requirement in the workspace `Cargo.toml`. For
   `gpui-pre`, also update the Kit demo's pin.
4. Add the previous version to the `patch-series` job in
   `.github/workflows/ci.yml`.
5. Run `cargo xtask vendor --crate <crate>` and update the
   lockfiles.
6. Run the bridge's checks for that backend, for example:

   ```console
   cargo check -p gpui-mcp --no-default-features --features gpui-ce --locked
   cargo test -p gpui-mcp --no-default-features --features gpui-ce,test-support --locked
   ```

   For `gpui-pre`, also run
   `cargo test --manifest-path examples/gpui-kit/Cargo.toml --locked`.

To drop an old version, delete its folder, remove it from `<crate>.json` and
the CI job, and update the version requirement in `Cargo.toml`.
