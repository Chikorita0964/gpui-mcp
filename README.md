# gpui-mcp

Give MCP agents eyes and hands inside a
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) app.

Agents read the live UI as a semantic tree, then click, type, hover, drag,
scroll and wait on real state. They can also take screenshots and diffs, record
video, measure frame cost, and edit HTML-authored interfaces while the app runs.
It works on Windows 11, macOS, and Linux.

## Setup

Install the MCP server and add it to your MCP client:

```console
cargo install --git https://github.com/themixednuts/gpui-mcp --locked gpui-mcp-server
```

```json
{ "mcpServers": { "gpui": { "command": "gpui-mcp" } } }
```

Add the bridge and its GPUI build to your app:

```toml
[dependencies]
gpui = { package = "gpui-pre", version = "=0.3.7" }
gpui_platform = { package = "gpui-pre-platform", version = "=0.3.7", features = ["font-kit", "wayland", "x11"] }
gpui-mcp = { git = "https://github.com/themixednuts/gpui-mcp", branch = "main" }

[patch.crates-io]
gpui-pre = { git = "https://github.com/themixednuts/gpui-mcp", branch = "main" }
```

`gpui-pre` is Zed's GPUI published under another name — the library is still
`gpui` — and the single `[patch.crates-io]` line points it at this repository's
vendored, patched copy, which keeps your app, `gpui_platform` and the bridge on
one GPUI build. The patch can go away once the additions inventoried in
[vendor/README.md](vendor/README.md) land in an upstream `gpui-pre` release;
each can be dropped independently.

Install the bridge when you create a window, and keep the returned handle in
your root view:

```rust,ignore
use gpui_mcp::{AppId, BridgeConfig, BridgeHandle};

let app_id = AppId::new("my-app")?;
let bridge = BridgeHandle::install(
    window,
    cx,
    BridgeConfig::new(app_id, "My App"),
)?;
```

That is the whole integration. The MCP client discovers running apps on its
own. See the [demo](examples/demo/src/main.rs) for a complete window.

## Making your UI agent-ready

The tree is built from your rendered elements and their accessibility data.
Write ordinary GPUI elements with stable IDs and normal event handlers;
`gpui-mcp` discovers the rendered hierarchy, text, bounds, state, and available
interactions automatically. Most problems come from the items below.

1. **Keep the `BridgeHandle` alive.** Store it in your root view. Dropping it
   stops the bridge.
2. **Give elements an `.id(...)`.** Only elements with an id become nodes.
   Text inside an element without one becomes part of its nearest ancestor's
   label, and that text cannot be clicked, waited on or asserted on separately.
   Clickable elements already need an id in GPUI.
3. **Keep ids unique among siblings.** When ids collide, the nodes get longer
   identities qualified by their path, and exact duplicates are dropped with a
   `DuplicateId` diagnostic. Find children through `parent`; don't rely on the
   shape of an id.
4. **Name controls that have no text.** An icon button's label is otherwise its
   glyph, such as `⚙`. Use `.aria_label("Settings")`, and use `.role(...)` when
   the role can't be inferred, as with tabs.
5. **State disabled and read-only explicitly.** `enabled` comes only from
   `.aria_disabled(true)`. A grey control with no click handler still reports
   `enabled: true`, so a consumer cannot tell a disabled control from one that
   is wrongly unreachable. Mark read-only inputs with `.aria_read_only(true)`:
   the tree and `get_element_state` expose `read_only`, and `wait_for_state`
   accepts an optional `read_only` predicate. A missing accessibility node
   leaves the state unknown, and a present node reports AccessKit's flag, which
   cannot recover a read-only state that the control omits.
6. **Redact secrets.** `.frame_redacted(true)` on an element with an id
   withholds its text and value from the bridge, and from any labels derived
   from them.
7. **Check the diagnostics.** `get_ui_tree` returns a `diagnostics` list that
   reports omitted, duplicate, orphaned and over-capacity nodes.

```rust,ignore
div()
    .id("settings")
    .aria_label("Settings")
    .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))
    .child(svg().path("icons/settings.svg").size_4())
```

For live HTML/CSS interfaces that agents can also edit, see the
[visual builder guide](docs/visual-builder.md) and the
[showcase](examples/runtime-showcase).

## What agents can do

| Area | Tools |
| --- | --- |
| Discover | `list_apps`, `select_app`, `ping`, `check_connection`, `get_ui_tree`, `find_elements`, `get_element`, `get_element_bounds` |
| Act | `click_element`, `double_click_element`, `click_coordinates`, `hover_element`, `drag_element`, `drag_coordinates`, `scroll`, `keyboard`, `type_text`, `set_text`, `set_value`, `focus_element`, `perform_action`, `pointer_click`, `pointer_down`, `pointer_up`, `pointer_move`, `pointer_drag`, `pointer_scroll`, `pointer_location` |
| Verify | `wait_for_element`, `wait_for_state`, `get_element_state`, `get_selection_count`, `get_text_info`, `get_value`, `save_ui_snapshot`, `load_ui_snapshot`, `diff_ui_snapshots`, `diff_current_ui` |
| Pixels | `screenshot`, `screenshot_region`, `screenshot_element`, `capture_screenshot_snapshot`, `compare_screenshots`, `diff_screenshots`, `highlight_elements`, `clear_highlights`, `start_video_recording`, `stop_video_recording` |
| Performance | `mark_frames`, `get_frame_stats`, `get_frame_report`, `record_performance`, `get_performance_report` |
| Live edit | `get_live_document`, `preview_live_document` |
| App | `list_app_commands`, `execute_app_command`, `get_logs`, `clear_logs` |

`get_ui_tree` returns the latest rendered tree. It can return only one node's
subtree (`root`), limit how many levels below the starting nodes are included
(`max_depth`; `0` returns the starting nodes only), or omit nodes whose state
is not visible (`visible_only`). The cut applies to `nodes` only: a returned
node still lists every child id, and a node's `parent` and the reply's `roots`
are returned as they are, so the reply can name an id that `nodes` does not
contain.

Several of the larger tools take an opt-in argument for a smaller reply.
`find_elements`, `get_ui_tree`, and `load_ui_snapshot` accept `ids_only`, which
drops the full nodes: `find_elements` returns the count and the ids, while
`get_ui_tree` and `load_ui_snapshot` return the generation, the node count, the
roots, and the ids. `get_frame_report`, `record_performance`,
`get_live_document`, and `preview_live_document` accept `summary_only`, which
returns the summary instead of the full detail. Defaults are unchanged; the
compact form appears only when the caller asks for it. A client that runs code
(OMP's eval cell, any MCP host that executes code) can call several tools in
one turn and should prefer the compact form for the big replies, so the
combined result stays under the client's display cap. Each tool's description
gives the exact shape of its compact reply.

`perform_action` runs an accessibility action on an element the way assistive
technology does, without coordinates or keystrokes: `click`, `increment`,
`decrement`, `expand`, `collapse`, and `set_value`. Use increment/decrement for
sliders and spinners, expand/collapse for disclosure widgets, and set_value to
replace a value; an element that does not register the action is reported
rather than silently ignored.

Prefer the element tools over coordinates. All coordinates are logical pixels
relative to the window.

## GPUI Kit and `gpui-pre`

The bridge builds against one GPUI: the patched `gpui-pre` 0.3.7 that this
repository vendors. There are no backends to select and no backend features; an
application and the bridge always share one GPUI type universe.

### GPUI Kit 0.7.0

[GPUI Kit](https://github.com/longbridge/gpui-kit) 0.7.0 pins the same
`gpui-pre` 0.3.7, so its recipe is the one above with `gpui-kit` added:

```toml
[dependencies]
gpui-kit = "=0.7.0"
gpui-mcp = { git = "https://github.com/themixednuts/gpui-mcp", branch = "main" }

[patch.crates-io]
gpui-pre = { git = "https://github.com/themixednuts/gpui-mcp", branch = "main" }
```

Call `gpui_kit::init(cx)`, open the window with `gpui_kit::open_window`, and
install and retain `BridgeHandle` as above. The bridge accepts GPUI Kit's
re-exported `Window` and `App` directly. Kit's components and platform backend
resolve to the same patched `gpui-pre`, which is the GPUI the bridge builds
against, so the MCP server and client configuration need no per-framework
selection.

Run the [Kit demo](examples/gpui-kit) with:

```console
cargo run --manifest-path examples/gpui-kit/Cargo.toml
```

Kit supplies accessibility roles, labels, control states and standard input
handlers, so the ordinary MCP tools can inspect and drive its annotated
components. Kit's disabled controls currently omit the click handler without
setting AccessKit's disabled state, so their `enabled` field still reports
`true`; the bridge does not infer disabled state from a missing handler. Kit
0.7.0 also does not publish `read_only` for its read-only inputs. Custom-drawn
components expose only the semantics they annotate; pointer tools and capture
remain available.

GPUI Kit 0.7.0 pins `gpui-pre = 0.3.7`; a newer snapshot pin requires a
matching vendor update, described in [vendor/README.md](vendor/README.md).

### The vendored `gpui-pre` copy

`vendor/gpui-pre/` is generated from the crates.io release plus the patch
series in [`vendor/patches/gpui-pre/0.3.7/`](vendor/patches/gpui-pre/0.3.7),
never edited by hand:

```console
cargo xtask vendor --crate gpui-pre --check   # verify the committed tree
cargo xtask vendor --crate gpui-pre           # rebuild it from crates.io
```

`cargo xtask vendor` replaces the old Python scripts, verifies the download's
checksum, and changes nothing if a patch fails to apply. This repository
vendors, builds and tests `gpui-pre` 0.3.7 only: the 0.3.5 and 0.3.6 patch
series stay in place, unbuilt, as upstream ships them, so the next upstream
sync stays small.

## Measuring frame cost

Injected input costs what the same input from the operating system costs.
Pointer and keyboard events invalidate only what their handlers notify, and the
server waits for the frames that input caused without adding frames of its own.
Screenshots request fresh frames, but those frames replay every cached view that
was not notified, so they do not render the whole window.

Call `mark_frames`, perform the interaction with `hover_element`,
`pointer_move`, or a real mouse, then call `get_frame_report`. The report
covers every frame completed after the mark. For each frame it gives:

- GPUI's whole `Window::draw` time (`draw_ms`, the interval the profiler
  records as `FrameTiming::draw_duration`), split into the application's share
  (`app_draw_ms`) and the bridge's (`bridge_ms`);
- p50, p95 and max for each;
- every view that rendered and why, plus the cached views that replayed
  instead.

The render causes are:

- `notified`: the view, or a view inside it, called `cx.notify()`
- `ancestor_rendered`: a cached view around it rendered
- `refresh`: the window was refreshed
- `first_draw`: the view had nothing cached yet
- `layout_changed`: its bounds, content mask, or text style changed
- `uncached`: it is not embedded with `.cached(...)`

A hover inside a region drawn with `Entity::cached` should show that region
rendering because it was `notified` and its siblings replaying. Anything else
shows where a caching boundary leaks. `get_frame_stats` averages over the same
window, `record_performance` reports the frames drawn during a fixed interval,
and an app can read the same numbers in process with `Automation::mark_frames`
and `Automation::frame_report`.

`bridge_ms` covers the work the bridge adds to a draw: finishing the
accessibility tree when no screen reader wants it, building the observed frame,
and painting highlights. Recording each element's accessibility node during
prepaint happens inside the application's own work and stays in `app_draw_ms`.
The semantic tree is converted when a client reads it, off the UI thread, so
that cost is in neither.

## Platform notes

- **Linux:** exact-window capture currently requires X11.
- **Windows:** screenshots take a short burst of compositor samples and return
  the newest, because Windows Graphics Capture can return a stale frame first.
  The one-second deadline bounds waiting on the compositor, not readback: the
  readback cost scales with the window's pixel count and with whether the
  server was built optimized, so it is measured on the first sample and
  credited back to the budget, up to a bound. A 5120x1440 window captures from
  a debug build, and an ordinary window keeps the one-second wait.

## Security

Only enable automation in development, testing, or another trusted
environment. See [SECURITY.md](SECURITY.md).

## License

Apache-2.0.
