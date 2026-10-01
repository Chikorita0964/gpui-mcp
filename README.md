# gpui-mcp

Give MCP agents eyes and hands inside a
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) app.

Agents can inspect the live UI, click, type, focus, hover, drag, scroll, take
screenshots, and record video. The bridge works on Windows 11, macOS, and Linux;
exact-window capture on Linux currently requires X11.

Windows screenshots take a short ordered burst of compositor samples and return
the newest, because Windows Graphics Capture can hand back an older composited
frame first. The one-second deadline on that burst bounds how long the capture
waits for the compositor, not how long the readbacks themselves take: the cost
of a readback scales with the window's pixel count and with whether the server
was built optimized, so it is measured on the first sample and credited back to
the budget, up to a bound. A 5120x1440 window captures from a debug build, and
an ordinary window keeps the one-second wait.

## Setup

Install the MCP server:

```console
cargo install --git https://github.com/themixednuts/gpui-mcp --locked gpui-mcp-server
```

Add it to your MCP client:

```json
{
  "mcpServers": {
    "gpui": {
      "command": "gpui-mcp"
    }
  }
}
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
vendored, patched copy. Everything in this repository builds on that one GPUI.

Install the bridge when you create a window and keep the returned handle in
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

That is the whole integration. The MCP client discovers running apps
automatically.

### GPUI Kit

[GPUI Kit](https://github.com/longbridge/gpui-kit) 0.7.0 pins the same
`gpui-pre` family, so its recipe is the one above with `gpui-kit` added:

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
against. The MCP server and client configuration need no per-framework
selection.

Run the [Kit demo](examples/gpui-kit) with:

```console
cargo run --manifest-path examples/gpui-kit/Cargo.toml
```

The bridge has no backends to select: it builds against the patched `gpui-pre`
everywhere, so an application and the bridge always share one GPUI type
universe without feature flags. GPUI Kit 0.7.0 pins `gpui-pre = 0.3.7`; a newer
snapshot pin requires a matching vendor update, described in
[vendor/README.md](vendor/README.md).

Kit supplies accessibility roles, labels, control states and standard input
handlers, so the ordinary MCP tools can inspect and drive its annotated
components. Kit's disabled controls currently omit the click handler without
setting AccessKit's disabled state, so their `enabled` field still reports
`true`. The bridge does not infer disabled state from a missing handler. Custom
drawn components expose only the semantics they annotate; pointer tools and
capture remain available.

## Building your UI

Write ordinary GPUI elements with stable IDs and normal event handlers:

```rust,ignore
div()
    .id("save")
    .on_click(cx.listener(|this, _, _, cx| this.save(cx)))
    .child("Save")
```

`gpui-mcp` discovers the rendered hierarchy, text, bounds, state, and available
interactions automatically. Use GPUI's standard accessibility methods when a
control's meaning cannot be inferred, such as `.role(Role::Tab)` or
`.aria_label("Settings")` on an icon button.

A control that refuses input must say so with `.aria_disabled(true)`. The tree's
`enabled` is read from AccessKit's disabled flag and from nothing else, so a
widget that merely withholds its click handler and paints itself grey still
reports `enabled: true` — the field then asserts a falsehood rather than
admitting it does not know, and a consumer cannot tell a disabled control from
one that is wrongly unreachable.

For read-only inputs, use `.aria_read_only(true)` separately from disabled
state. The tree and `get_element_state` expose `read_only`, and
`wait_for_state` accepts an optional `read_only` predicate. A missing
accessibility node leaves the state unknown; a present node reports AccessKit's
flag, which cannot recover a read-only state that the control omits. Kit 0.7.0
does not currently publish that flag for its read-only inputs.

The single `[patch.crates-io] gpui-pre` line keeps your app, `gpui_platform`,
and the bridge on one GPUI type universe. It can go away once the ten small
additions inventoried in [vendor/README.md](vendor/README.md) land in an
upstream `gpui-pre` release; each can be dropped independently.

See the [demo](examples/demo/src/main.rs) for a complete window. For live
HTML/CSS interfaces, see the [visual builder guide](docs/visual-builder.md).

## Measuring frame cost

Injected input costs what the same input from the operating system costs.
Pointer and keyboard events invalidate only what their handlers notify, and the
server waits for the frames that input caused without adding frames of its own.
Screenshots request fresh frames, but those frames replay every cached view that
was not notified, so they do not render the whole window.

To measure an interaction, call `mark_frames`, perform it with `hover_element`,
`pointer_move`, or a real mouse, then call `get_frame_report`. The report covers
every frame completed after the mark. For each frame it gives GPUI's whole
`Window::draw` time (`draw_ms`, the interval GPUI's profiler records as
`FrameTiming::draw_duration`), split into the application's share
(`app_draw_ms`) and the bridge's (`bridge_ms`). It also gives p50, p95, and
maximum for each, and every view that rendered, with the reason:

- `notified`: the view, or a view inside it, called `cx.notify()`
- `ancestor_rendered`: a cached view around it rendered
- `refresh`: the window was refreshed
- `first_draw`: the view had nothing cached yet
- `layout_changed`: its bounds, content mask, or text style changed
- `uncached`: it is not embedded with `.cached(...)`

It also lists the cached views that replayed instead. A hover inside a region
drawn with `Entity::cached` should show that region rendering because it was
`notified` and its siblings replaying. Anything else shows where a caching
boundary leaks. `get_frame_stats` averages over the same window, and
`record_performance` reports the frames drawn during a fixed interval. An app
can read the same numbers in process with `Automation::mark_frames` and
`Automation::frame_report`.

`bridge_ms` covers the work the bridge adds to a draw: finishing the
accessibility tree when no screen reader wants it, building the observed frame,
and painting highlights. Recording each element's accessibility node during
prepaint happens inside the application's own work and stays in `app_draw_ms`.
The semantic tree is converted when a client reads it, off the UI thread, so
that cost is in neither.

Only enable automation in development, testing, or another explicitly trusted
environment. See [SECURITY.md](SECURITY.md).

Apache-2.0.
