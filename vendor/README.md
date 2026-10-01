# Vendored dependencies

`gpui-pre/` is the crates.io `gpui-pre` 0.3.7 snapshot of Zed commit
`1a28cff4b409169bac058bca40dfbfeb7621d19b`, matching GPUI Kit 0.7.0. It is the
one GPUI this repository builds on: the crate's library name is still `gpui`,
and the workspace, the demos and the bridge all depend on it through
`gpui = { package = "gpui-pre", ... }` with the single
`[patch.crates-io] gpui-pre` line described below. It carries the ten
downstream changes inventoried in this file, applied with
`patches/gpui-pre.patch`. The patch also drops the published manifest's
`repository` URL, which named the Zed repository, so the vendored crate no
longer points at it.

The crate's Apache-2.0 license is retained in `gpui-pre/LICENSE-APACHE`.
The published manifest, including its renamed snapshot dependencies and `gpui`
library name, is retained - except for the `repository` URL the patch removes
and the empty `[workspace]` table it adds (the published manifest has none), so
Cargo treats the vendored crate as its own workspace root and its test target
can be checked from any checkout. Source line endings are normalized, two blank
doc comments in `_accessibility.rs` have trailing spaces removed (the patch's
first hunk), and the published Cargo.lock is omitted. Those normalizations
carry no behavior change.

## The ten changes

The first six were last checked against `zed-industries/zed` `main` on
2026-09-22, when they were written against Zed commit `16c9aa7` (`gpui` 0.2.2);
the last three were written against the same commit on 2026-09-23, and the
accessibility actions on 2026-09-27. All ten now live in this snapshot (Zed
`1a28cff`), which is newer than that commit.

- read-only observation of each completed rendered AccessKit tree, with stable
  GPUI element paths, frame-unique node identities, bounds, text provenance,
  and an overlay paint pass (`FrameObserver`, `AccessibilityFrame`, `FrameNode`,
  `Window::observe_frames`, `Window::focus_observed_element`). A node keeps its
  own element id while that id names it alone, and otherwise takes the shortest
  trailing run of its element path that separates it, so identities are
  deliberately **not uniform in shape**: two siblings can look nothing alike
  because one collided and the other did not, and adding a widget to one
  container can change the shape of identities in a container nobody touched.
  Consumers must group nodes by `parent`, which is exact and resolved from
  paths, and never by the form of the identity itself — a matcher keyed on an
  identity pattern selects exactly the nodes that collided and silently skips
  the ones that did not;
- rendered-frame provenance that AccessKit does not model: the
  `Element::frame_node` and `Element::frame_text` hooks, the `frame_metadata`,
  `frame_action`, and `frame_redacted` builders, hover/drag/scroll interaction
  inference from an element's listeners, and a `Div` with click listeners
  reporting `Role::Button`. Redacted frame text and values are withheld from
  the bridge, including labels derived from content. Text inside a redacted
  element cannot contribute to ancestor labels, including during cached replay.
  Elements that implement
  the standard accessibility hooks are observed even without overriding
  `frame_node`. A wrapped Div contributes its interaction provenance during
  prepaint, preserving actions, metadata and redaction through external
  wrappers such as GPUI Kit's test-support elements;
- `aria_disabled`, `aria_hidden`, and `aria_read_only` builders that forward to
  the corresponding AccessKit node states. An ID-bearing, hidden `Div` with no
  other role reports `Role::Group` so the hidden state covers its descendants;
- programmatic focus and text replacement through GPUI's active input handler
  (`Window::insert_input_text`, `Window::replace_input_text`). Observed focus
  honors a control's AccessKit Focus handler, which lets composite controls
  redirect focus to their actual editor before text is dispatched;
- pointer ownership that prevents a stale native mouse position from cancelling
  a synthetic hover before the physical mouse actually moves;
- font-family fallback that preserves the requested weight, style, and OpenType
  features;
- frame cost and view-cache observation. `FrameObserver::frame_drawn` receives
  a `DrawnFrame` when `Window::draw` returns. It carries the draw's wall time,
  the same interval the profiler records as `FrameTiming::draw_duration`, but
  without the `profiler` feature. It also carries the part of that time spent
  only because the frame was observed, and a `ViewDraw` for every view: whether
  it rendered or replayed its previous output, and, for a render, the
  `ViewRenderCause`. `FrameObserver::accessibility_frame` hands observers a
  shared `Arc<AccessibilityFrame>`, so they can keep it and convert it later,
  off the UI thread. `AccessibilityFrame::accessibility_node` indexes the tree
  on first use instead of scanning it for every node. An observed frame's
  prepaint checkpoints record only the nodes that are open. Those are the only
  existing nodes a later range can add text to, so a cached view's two
  checkpoints no longer copy every node drawn so far. Text is added to open
  nodes by position rather than by searching for their path;
- `Window::request_frame`, which schedules a frame without the view-cache
  invalidation of `Window::refresh`, and `Window::frame_pending`, which reports
  whether the window is invalidated; and
- view-scoped redraws for an element's own interaction state. Pressing,
  releasing, and the active state of an element with click or drag handlers,
  and showing its tooltip, notify the view that painted the element. Upstream
  refreshes the whole window, which renders every cached view again. That state
  is read only while its view prepaints and paints the element, so the view is
  enough. An element painted outside any view still refreshes. Hiding a tooltip
  requests a frame without notifying anything: a replayed element re-registers
  its old tooltip request, and that request's visibility check reports the
  tooltip hidden. This is the one change here that alters GPUI's behavior
  rather than adding to it. Starting a drag, dropping, changing the window's
  hover status, and switching between keyboard and pointer input still refresh
  the window: a drag is drawn at window level, any view may read
  `Window::is_window_hovered`, and input modality changes hover and
  focus-visible styling everywhere;
- semantic accessibility actions through the window's own AccessKit
  handling. Upstream registers elements' `on_a11y_action` listeners and
  handles click, focus, and blur internally, but `handle_a11y_action` is
  private, so only the platform adapter can reach it.
  `Window::perform_a11y_action` runs one requested action through that same
  path, and `Window::a11y_action_is_handled` reports whether the node
  registered a listener for it, so a caller can tell "the element handles
  this" from "GPUI's built-in fallback will run". `Window::a11y_node_id`
  resolves an element's stable ID to the AccessKit node of the last observed
  frame, the lookup `focus_observed_element` already needed; both keep its
  rule that a duplicated ID resolves to nothing.

Two earlier additions are no longer needed and are not carried:

- `DispatchEventResult` was made public so callers could use `Window::
  dispatch_event`; upstream now exposes both publicly.
- `Window::native_window_id`; `gpui-mcp` obtains native window identity through
  `raw-window-handle` instead.

The bridge uses GPUI's standard `Role` and `aria_*` APIs, extended only by the
builders above. It does not maintain a second semantic tree.

Every item should be removed here as soon as an equivalent upstream API is
available.

## Patching GPUI

The workspace and every application that links the bridge add one line:

```toml
[patch.crates-io]
gpui-pre = { path = "vendor/gpui-pre" }
```

(A published application points the same key at this repository's Git source.)
That is the only patch. It keeps the application, `gpui_platform`, GPUI Kit and
the bridge on one GPUI type universe.

## Updating the GPUI Kit snapshot

The version, archive SHA-256 and original Zed revision are recorded in
`gpui-pre.json`. Python 3.11+ and GNU patch reproduce the vendored copy:

```console
python3 script/vendor-gpui-pre.py --check
python3 script/vendor-gpui-pre.py
```

`--check` downloads the checksum-verified archive, applies patches in a
temporary directory, and compares every file with the vendor tree. It also
verifies that the workspace's `gpui = { package = "gpui-pre", version = ... }`
pin matches `gpui-pre.json`. The update command replaces the vendor tree only
after the patches apply successfully.

When Kit changes its exact GPUI pin, update `gpui-pre.json` from that crates.io
release, the workspace's `gpui` dependency and the Kit demo's dependency pin
together. Rebase `patches/gpui-pre.patch` if needed, regenerate the vendor tree
and both lockfiles, then run:

```console
cargo check -p gpui-mcp --locked
cargo test -p gpui-mcp --features test-support --locked
cargo test --manifest-path examples/gpui-kit/Cargo.toml --locked
cargo check --manifest-path vendor/gpui-pre/Cargo.toml --tests --features test-support
```

The last command compiles the patch's own test modules - `fallback_tests` in
`text_system.rs` and `pointer_state_tests` in `window.rs` - which no workspace
target builds, because `vendor/gpui-pre` is excluded from the workspace. The
vendored manifest carries an empty `[workspace]` table (a deliberate deviation:
the published manifest has none) so Cargo treats it as its own workspace root
and the command runs from any checkout. `--features test-support` supplies the
dev-dependencies the published manifest drops, and the crate's `svg_renderer`
test module includes two font files by relative path, so the two files must
exist at `<checkout>/assets/fonts/{ibm-plex-sans/IBMPlexSans-Regular.ttf,
lilex/Lilex-Regular.ttf}` (empty files suffice for the check; they are Zed
assets this repository does not carry) - without them the command fails in
`include_bytes!` before compiling anything else. As its own workspace root the
crate writes `vendor/gpui-pre/Cargo.lock` and `vendor/gpui-pre/target/`; point
`CARGO_TARGET_DIR` outside the tree to keep `target/` away. Then, as a definite
step after the check, delete `vendor/gpui-pre/Cargo.lock` and the two font
stubs you created: `script/vendor-gpui-pre.py --check` compares every file and
drops the published Cargo.lock by design, so a leftover lock makes it exit 1
blaming the patch. The three paths are named in the root `.gitignore`, so an
accidental `git add -A` cannot commit them - if this repository ever carries
real Zed font assets at those paths, drop those two rules first.

The Kit demo is an independent workspace so its lockfile resolves separately
from the demos' workspace; there are no GPUI backends to select, so the bridge
needs no feature to build against Kit.

CI also checks a standalone consumer against the public Git commit, with no
local path dependencies. To check an already pushed revision manually:

```console
python3 script/check-gpui-kit-consumer.py --repository https://github.com/themixednuts/gpui-mcp --rev <full-commit-sha> --target-dir target
```

The fixture type-checks `BridgeHandle::install` using Kit's `Window` and `App`
and verifies that Kit, Base, Component, Platform and the bridge all resolve to
one patched `gpui-pre` package. This check resolves a fresh application lockfile
to exercise the documented installation recipe, while the repository's own
builds use their committed lockfiles.

Remove the patch in the workspace `Cargo.toml` and this directory once an
upstream `gpui-pre` release includes these behaviors. Each change can be dropped
independently after its upstream equivalent ships.
