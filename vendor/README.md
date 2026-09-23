# Vendored dependencies

`gpui-pre/` is `gpui-pre` 0.3.6 from crates.io, the crate GPUI Kit re-exports
as `gpui`. Its downstream changes are inventoried in `PATCHES.md`, with enough
detail to re-apply them to a newer release.

The crate's Apache-2.0 license is retained in `gpui-pre/LICENSE-APACHE`.

Remove the patch in the workspace `Cargo.toml` and this directory once an
upstream `gpui-pre` release includes these behaviors. Each change can be dropped
independently after its upstream equivalent ships.
