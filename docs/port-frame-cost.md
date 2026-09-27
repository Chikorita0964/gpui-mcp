# Pending: port upstream frame-cost measurement (a95e4a9)

`origin/main` carries `a95e4a9` ("Measure what an interaction costs instead of
redrawing the whole window"), written on the old `vendor/gpui` fork (protocol
v13). `gpui-pre` is the rewrite on the crates.io `gpui-pre` crate (protocol
v15), 85 commits past their merge base `7b834c1`. A plain `git merge a95e4a9`
conflicts in 11 files (23 hunks in `registry.rs` alone) and deletes-vs-modifies
`vendor/gpui/`, so the plan is to port the features, then record the merge.

## Plan

1. Port each feature below onto the `gpui-pre` architecture as normal commits.
2. `git merge -s ours a95e4a9 -m "Merge a95e4a9: ported onto gpui-pre in <revs>"`
   so `origin/main` history is joined without taking its tree.
3. Full CI gate (see the end), push `gpui-pre`.

Read the upstream diffs with `git show a95e4a9 -- <path>`.

## Features to port

### Vendor patch C18 (`vendor/gpui-pre`, record with `./vendor/patch.ps1 record gpui-pre "C18 ..."`)

- `elements/div.rs`: `redraw_painting_view` notifies the view that painted a
  clicked/active element instead of `window.refresh()`; tooltip show notifies
  `current_view`; tooltip hide calls `request_frame`. A three-way
  `git merge-file` of upstream's `div.rs` (base `7b834c1`) onto the vendored
  file applies with no conflicts.
- `window.rs`: `Window::request_frame` (mark dirty without discarding view
  caches) and `Window::frame_pending`; time the whole `Window::draw` and hand
  observers the draw duration plus the bridge's own observation time.
  `gpui-pre` already has `is_redraw_pending` (C14); reuse or align with it.
- `view.rs`: record per-view outcome (`Reused` or `Rendered(cause)`) with
  `ViewRenderCause` = Uncached, CachingDisabled, Refresh, FirstDraw, Notified,
  AncestorRendered, LayoutChanged; `view_cache_miss` decides the cause.
  Upstream stores this on `next_frame.observed` (its `FrameBuilder`), which
  `gpui-pre` does not have; put it on the C14 `A11yFrame` path instead
  (`window/a11y.rs`, `A11yFrameObserver`), e.g. a new `frame_drawn` callback.

### Protocol (bump to v16)

- `FrameStats` gains `draw_*`, `bridge_*`, `mark_frame_count`.
- New types: `ViewRenderCause`, `ViewOutcome`, `ViewDraw`, `FrameSample`,
  `Distribution`, `FrameSummary`, `ViewActivity`, `FrameReport`, `PendingFrame`;
  constants `MAX_FRAME_SAMPLES` (512), `MAX_FRAME_VIEWS` (256).
- Operations: `RequestFrame`, `GetPendingFrame`, `MarkFrames`,
  `GetFrameReport { after_frame_count, frame_limit }`; results `PendingFrame`,
  `FrameReport`.

### Bridge (`crates/gpui-mcp`)

- `registry.rs`: bounded frame history (`FrameRecord`), `mark_frames`,
  `frame_report` with nearest-rank p50/p95, per-view activity; `finish_draw`
  completes a frame. Upstream also moved tree conversion off the UI thread;
  `gpui-pre` already converts in the observer, so only port it if profiling
  shows the conversion inside the frame.
- `service.rs`: drop `window.refresh()` after `PointerInput`, `Input`, and
  `PerformAction`; `ClearHighlights` uses `request_frame`; keep refresh for
  live-document previews and app commands; handle the new operations.
- `lib.rs`: `Automation::frame_stats`, `mark_frames`, `frame_report`, and the
  re-exports.
- Tests from upstream `observer.rs`: hover, click, and tooltip redraw only the
  cached view involved; a requested frame replays every cached view.

### Server (`crates/gpui-mcp-server`)

- Replace `settle_after_refresh` with `settle_pending` (wait only for frames
  already pending, at most 4) and `settle_requested_frames` (two
  `RequestFrame`s, used before screenshots).
- Tools `mark_frames` and `get_frame_report`; `record_performance` includes the
  report; `average_render_work_ms` uses draw minus bridge time. Add both tool
  names to the tool-list test.
- `tests/frame_cost.rs` (607 lines upstream): one hover over MCP stdio is one
  frame in which only the hovered cached region renders.

### Demo and docs

- Demo: two cached `ProbeRegion` views (`probe-left`, `probe-right`); keep the
  existing `--animate` ticker.
- `README.md`: "Measuring frame cost" section.
- `vendor/PATCHES.md`: C18 section and patch table row.

## Gate

`cargo fmt --all -- --check`; `cargo check --workspace --all-targets --all-features --locked`;
`cargo check -p gpui-mcp --no-default-features --locked`; `cargo build -p gpui-mcp-demo --locked`;
`cargo test --workspace --all-features --locked`;
`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
`./vendor/patch.ps1 verify gpui-pre` and `./vendor/patch.ps1 verify gpui-base`.
