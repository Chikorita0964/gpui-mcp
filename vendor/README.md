# Vendored dependencies

`gpui/` is GPUI 0.2.2 from Zed commit
`16c9aa7ea6d897a8044d9501cde1b295256722f2`. It carries the downstream source
changes inventoried in `gpui/PATCHES.md`, each checked against upstream as of
2026-09-22 and still unmerged there.

The crate's Apache-2.0 license is retained in `gpui/LICENSE-APACHE`.

Remove the patch in the workspace `Cargo.toml` and this directory once an
upstream GPUI release includes these behaviors. Each change can be dropped
independently after its upstream equivalent ships.
