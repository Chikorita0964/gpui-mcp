# Vendored dependencies

This directory holds the patched copy of GPUI the workspace builds on. The
patches add what the bridge needs to observe and drive a running app. Each can
be dropped once upstream ships an equivalent.

| Directory                      | What it is                                                                                                                    | Used by                                     |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------- |
| `gpui-fast/`                   | The crates.io `gpui-fast` 0.1.6 release, Longbridge's GPUI with retained rendering, as GPUI Kit's `gpui-fast` feature uses it | The whole workspace (as `gpui`)             |
| `patches/gpui-fast/<version>/` | The patches for each supported `gpui-fast` version                                                                            | `gpui-fast/` and downstream apps            |

GPUI Fast is Zed's GPUI with retained rendering and scroll layers. It builds on
the `gpui-pre` crates for its smaller parts and keeps `gpui-pre`'s API, so GPUI
Kit switches to it with one feature. The copy keeps its Apache-2.0 license file
(`LICENSE`).

## `gpui-fast/`

It includes the parts GPUI Kit depends on:

- Accessibility elements from other crates are observed.
- Clicks and other interactions are still detected through wrapper elements.
- Focusing a Kit input focuses the editor inside it.

The Kit demo's tests cover control states, focus, Unicode text replacement and
redaction.

The copy is the published crate with three changes: the patches are applied,
source files use LF line endings, and the published `Cargo.lock` is removed.

### Supported versions

| Crate       | Versions | Vendored here |
| ----------- | -------- | ------------- |
| `gpui-fast` | 0.1.6    | 0.1.6         |

Each version has its own folder in `patches/gpui-fast/<version>/` with these
patches, applied in this order:

1. `automation.patch`: everything the bridge needs.
2. `accessibility-actions.patch`: `Window::perform_a11y_action`, which runs an
   accessibility action through the window's own AccessKit handling,
   `Window::a11y_action_is_handled` and `Window::a11y_node_id`.
   `perform_action`'s semantic actions need it.
3. `grid.patch`: CSS grid track lists. Upstream GPUI only offers
   `grid_cols(n)` / `grid_rows(n)`, which always mean
   `repeat(n, minmax(0, 1fr))`, although taffy implements full CSS grid. This
   adds `GridTrack`, `GridTrackSize`, `GridTrackBreadth`, `GridRepetition` and
   `GridAutoFlow`, and the `Styled` builders `grid_template_columns`,
   `grid_template_rows`, `grid_auto_columns`, `grid_auto_rows`,
   `grid_auto_flow`, `grid_column` and `grid_row`. `gpui-mcp-html` needs it to
   render `grid-template-*`; the bridge doesn't, so it can be skipped.

GPUI Fast already keeps a fallback font's requested weight, style and features,
so it needs no font-fallback patch.

The automation patch follows GPUI Fast where it moved upstream code: views draw
through its retained rendering, which draws nothing again from last frame while
the accessibility tree is built, so a frame that builds it renders every view
and reports a cached view's cause as `CachingDisabled`. A frame observer says
whether a frame needs the tree (`FrameObserver::wants_accessibility`, `true` by
default), and the bridge asks for it only on demand; and its input-change
snapshot is taken before the pointer moves, so a view that read the pointer is
rendered again when it moves.

`gpui-fast.json` lists each version with its download checksum, and says which
version is vendored here.

### Commands

Run these from a checkout of this repository. They only need Rust: `xtask` is
a small tool in this workspace that downloads the crate and applies the
patches itself. `cargo xtask --help` lists every option.

```console
# Check that the vendored copy matches the published crate plus the patches
cargo xtask vendor --check

# Rebuild the vendored copy from the published crate
cargo xtask vendor

# Write a patched copy somewhere else
cargo xtask vendor --output <dir>
cargo xtask vendor --output <dir> --without grid
```

The tool verifies the download's checksum and changes nothing if a patch fails
to apply. It refuses to run if the workspace `Cargo.toml`'s `gpui` requirement
doesn't match `gpui-fast.json`.

### What CI checks

- The vendored copy matches the published crate plus its patches.
- The workspace builds, passes its tests and passes Clippy on every OS against
  the vendored copy, the GPUI Kit demo included.
- On Linux the bridge does the same against `gpui-fast` 0.1.6 without the grid
  patch.
- A fresh GPUI Kit app that installs the bridge from the pushed Git commit
  builds, and its GPUI crates and the bridge share the single patched GPUI
  Fast.

To run that last check yourself against a pushed commit:

```console
cargo xtask check-consumer --repository https://github.com/Chikorita0964/gpui-mcp --rev <full-commit-sha> --target-dir target
```

### Adding a new version

When GPUI Kit moves to a new GPUI Fast version:

1. Add the version to `gpui-fast.json` and make it the `vendored` one.
2. Copy the newest folder in `patches/gpui-fast/` to the new version and fix
   any patches that no longer apply.
3. Update the `gpui` and `gpui_platform` requirements in the workspace
   `Cargo.toml`, and GPUI Kit's revision if it moved.
4. Point the `patch-series` job in `.github/workflows/ci.yml` at the new
   version.
5. Run `cargo xtask vendor` and update the lockfile.
6. Run the workspace's checks:

   ```console
   cargo test --workspace --features gpui-mcp/test-support --locked
   cargo clippy --workspace --all-targets --features gpui-mcp/test-support --locked -- -D warnings
   ```

To drop an old version, delete its folder, remove it from `gpui-fast.json` and
the CI job, and update the version requirement in `Cargo.toml`.
