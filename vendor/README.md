# Vendored dependencies

Each vendored crate is its crates.io release plus a patch series. The workspace
`[patch.crates-io]` table points the crate at `vendor/<crate>`. Two crates are
vendored: `gpui-pre` (GPUI itself) and `gpui-base` (the foundation of
gpui-component).

| Path | Contents |
|---|---|
| `vendor/<crate>/` | The release with every patch applied. Never edit it without recording a patch. |
| `vendor/patches/<crate>/VERSION` | The crates.io version the series applies to. |
| `vendor/patches/<crate>/NNNN-*.patch` | The series, in order. `PATCHES.md` explains each one. |
| `vendor/patch.ps1` | Rebuild, verify, upgrade, or extend a vendored crate. |

| Command | Effect |
|---|---|
| `./vendor/patch.ps1 verify <crate>` | Fails unless `vendor/<crate>` equals the release plus the series. CI runs it. |
| `./vendor/patch.ps1 apply <crate>` | Rebuilds `vendor/<crate>` from the release and the series. |
| `./vendor/patch.ps1 record <crate> "<title>"` | Saves the edits made in `vendor/<crate>` as the next patch. |
| `./vendor/patch.ps1 update <crate> <version>` | Applies the series to a new release with a 3-way merge and moves `VERSION`. A conflict stops with the scratch repository to resolve it in. |

`.gitattributes` marks `vendor/**` as `-text`, so each file keeps the release's
exact bytes.

Each crate keeps its own license file (`gpui-pre/LICENSE-APACHE`,
`gpui-base/LICENSE-APACHE`).
