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
gpui = "=0.2.2"
gpui_platform = { git = "https://github.com/zed-industries/zed", rev = "16c9aa7ea6d897a8044d9501cde1b295256722f2", features = ["font-kit", "wayland", "x11"] }
gpui-mcp = { git = "https://github.com/themixednuts/gpui-mcp", branch = "main" }

[patch.crates-io]
gpui = { git = "https://github.com/themixednuts/gpui-mcp", branch = "main" }

[patch."https://github.com/zed-industries/zed"]
gpui = { git = "https://github.com/themixednuts/gpui-mcp", branch = "main" }
```

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

The two Cargo patches keep your app, `gpui_platform`, and the bridge on one GPUI
type universe. They can go away once the small additions in the
[vendor patch inventory](vendor/gpui/PATCHES.md) land upstream.

See the [demo](examples/demo/src/main.rs) for a complete window. For live
HTML/CSS interfaces, see the [visual builder guide](docs/visual-builder.md).

Only enable automation in development, testing, or another explicitly trusted
environment. See [SECURITY.md](SECURITY.md).

Apache-2.0.
