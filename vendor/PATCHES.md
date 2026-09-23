# GPUI-pre patch inventory

`vendor/gpui-pre` is `gpui-pre` 0.3.6 from crates.io: the crate GPUI Kit
re-exports as `gpui`, which `gpui-mcp` aliases in the workspace manifest. Six
focused patches open APIs the bridge needs but cannot reach through the stock
surface. Every patch is visibility or gating only: it opens an existing
implementation, adds no behavior beyond the gate it opens, and is marked at its
site with a `gpui-mcp patch (C0x):` comment.

## Re-applying the set

1. Copy the `gpui-pre` source at the version `gpui-kit` pins into
   `vendor/gpui-pre` (extract the `.crate` archive, or run `cargo vendor`).
2. Apply the hunks below. Each names its anchor: the stock item it modifies or
   the item it sits next to.
3. Add the workspace patch so the graph resolves to the vendored tree:

   ```toml
   [patch.crates-io]
   gpui-pre = { path = "vendor/gpui-pre" }
   ```

4. Confirm `cargo tree -p gpui-pre` resolves to `vendor/gpui-pre` and that
   `cargo check --workspace` exits 0.
5. On a newer `gpui-pre`, re-verify that each stock item still exists and the
   capability is still unreachable, and drop any patch whose capability the
   newer version exposes natively.

## C01 - Vendored crate and workspace patch

| | |
|---|---|
| File | `Cargo.toml` (workspace), `vendor/gpui-pre/` |
| Item | `[patch.crates-io] gpui-pre = { path = "vendor/gpui-pre" }` |
| Opened | The workspace resolves `gpui-pre` to the vendored tree, so C02-C06 apply to the exact crate `gpui-kit` aliases as `gpui`; every other crate still resolves from crates.io. |
| Why | `gpui-kit` depends on the crates.io `gpui-pre`; without a path patch, the source edits would require forking `gpui-kit` itself. |

`vendor/gpui-pre` is the 0.3.6 archive with only the files below modified.

## C02 - Accessibility activation

| | |
|---|---|
| File | `vendor/gpui-pre/src/window.rs` |
| Item | `Window::new`, the `a11y_active_flag` initializer (stock `window.rs:1607`) |
| Opened | `Window::debug_a11y_tree_json()` returns `Some` on a freshly opened window with no assistive technology attached, so `observer.rs` can observe every frame; a platform adapter can still deactivate the tree. |
| Why | Stock GPUI builds the accessibility tree only while `A11y::is_active()` reads this flag. The flag starts `false` and only the platform adapter's activation callback flips it, so a bridge that is not a screen reader never sees a tree. |

```rust
// was: AtomicBool::new(false)
let a11y_active_flag = Arc::new(AtomicBool::new(!accessibility_force_disabled));
```

## C03 - Per-node bounds

| | |
|---|---|
| File | `vendor/gpui-pre/src/window.rs` |
| Item | `Window::a11y_node_bounds`, next to `debug_a11y_tree_json` (stock `window.rs:6782`) |
| Opened | Per-node logical bounds in the same pixels the layout engine used, which `observer.rs` publishes as protocol `Rect`. |
| Why | `A11y::node_bounds` is `pub(crate)` (`window/a11y.rs:150`) and the debug JSON does not carry bounds, so no stock call returns them. |

```rust
/// gpui-mcp patch (C03): logical bounds of an accessibility node.
///
/// These are the same logical pixels the layout engine used, stored on the
/// node during prepaint.
pub fn a11y_node_bounds(&self, node_id: accesskit::NodeId) -> Option<Bounds<Pixels>> {
    self.a11y.node_bounds.get(&node_id).copied()
}
```

## C04 - Active input handler and node focus

| | |
|---|---|
| File | `vendor/gpui-pre/src/window.rs` |
| Item | `Window::insert_input_text` and `Window::replace_input_text` (after `Window::focus`), plus `Window::a11y_focus_handle` (next to `debug_a11y_tree_json`) |
| Opened | Text insertion and replacement through the focused element's live `PlatformInputHandler` (`dispatch_input` and `replace_text_in_range`), used by `input.rs`; and resolution of an accessibility `NodeId` to its `FocusHandle`, used by the `Focus` operation in `service.rs`. |
| Why | The platform window and its handler slot are crate-private, so the live handler is unreachable from the bridge; `A11y::focus_ids` (`window/a11y.rs:149`) and `FocusHandle::for_id` (`window.rs:558`) are `pub(crate)`, and the only stock `NodeId`-to-focus path is `Window::handle_a11y_action` (`window.rs:6805`), also `pub(crate)`. |

```rust
/// gpui-mcp patch (C04): insert text through the focused element's active
/// input handler.
///
/// Returns `false` when the window has no active input handler.
pub fn insert_input_text(&mut self, text: &str, cx: &mut App) -> bool {
    let Some(mut input_handler) = self.platform_window.take_input_handler() else {
        return false;
    };
    input_handler.dispatch_input(text, self, cx);
    self.platform_window.set_input_handler(input_handler);
    true
}

/// gpui-mcp patch (C04): replace the complete document owned by the focused
/// input handler through `PlatformInputHandler::replace_text_in_range`.
///
/// Returns `false` when the handler cannot provide its document range.
pub fn replace_input_text(&mut self, text: &str) -> bool {
    let Some(mut input_handler) = self.platform_window.take_input_handler() else {
        return false;
    };
    let mut document_range = None;
    let available = input_handler
        .text_for_range(0..usize::MAX, &mut document_range)
        .is_some()
        && document_range.is_some();
    if let Some(document_range) = document_range {
        input_handler.replace_text_in_range(Some(document_range), text);
    }
    self.platform_window.set_input_handler(input_handler);
    available
}

/// gpui-mcp patch (C04): resolve an accessibility node to its focus handle.
///
/// Returns `None` when the node does not track focus in the current frame.
pub fn a11y_focus_handle(&self, node_id: accesskit::NodeId, cx: &App) -> Option<FocusHandle> {
    let focus_id = self.a11y.focus_ids.get(&node_id).copied()?;
    FocusHandle::for_id(focus_id, &cx.focus_handles)
}
```

## C05 - Synthetic pointer preservation

| | |
|---|---|
| File | `vendor/gpui-pre/src/window.rs` |
| Item | New field `platform_mouse_position` (next to `mouse_position`), its initializer in `Window::new` and its value in the struct literal, and the adoption guard in `bounds_changed` (stock `window.rs:2695`) |
| Opened | A synthetic hover survives a resize or a DPI change until the physical mouse actually moves; `input.rs` tests assert this for `bounds_changed`, the platform resize callback, and a scale-factor change. |
| Why | Stock `bounds_changed` unconditionally overwrites `self.mouse_position` with `platform_window.mouse_position()`, so a stale platform reading cancels a synthetic hover before the physical mouse moves. |

```rust
// field, next to `mouse_position`
platform_mouse_position: Point<Pixels>,

// Window::new, next to `let mouse_position = platform_window.mouse_position();`
let platform_mouse_position = mouse_position;

// struct literal, next to `mouse_position,`
platform_mouse_position,

// bounds_changed, replacing `self.mouse_position = self.platform_window.mouse_position();`
let platform_mouse_position = self.platform_window.mouse_position();
if platform_mouse_position != self.platform_mouse_position {
    self.mouse_position = platform_mouse_position;
    self.platform_mouse_position = platform_mouse_position;
}
```

## C06 - Fallback font traits

| | |
|---|---|
| File | `vendor/gpui-pre/src/text_system.rs` |
| Item | `fallback_with_requested_traits` (new private helper) called from the fallback loop in `resolve_font` (stock `text_system.rs:255` builds the stack; `text_system.rs:370` resolves it) |
| Opened | Fallback faces keep the requested weight, style, and OpenType features, so a bold Han string renders bold through the fallback stack. |
| Why | Stock `resolve_font` passes each fallback font to `font_id` unchanged, dropping the traits of the font the caller requested. |

```rust
// in resolve_font, inside `for fallback in &self.fallback_font_stack {`
let fallback = fallback_with_requested_traits(fallback, font);
if let Ok(font_id) = self.font_id(&fallback) {
    return font_id;
}

fn fallback_with_requested_traits(fallback: &Font, requested: &Font) -> Font {
    Font {
        family: fallback.family.clone(),
        features: requested.features.clone(),
        fallbacks: fallback.fallbacks.clone(),
        weight: requested.weight,
        style: requested.style,
    }
}
```

`vendor/gpui-pre/src/text_system.rs` also carries the helper's unit test
(`fallback_replaces_only_the_unavailable_family`).

## Files with no patch

`vendor/gpui-pre/src/window/a11y.rs` is unchanged: C03 and C04 read its
`pub(crate)` fields from `window.rs`, which is in the same crate.

## Verification in this fork

| Command | Result |
|---|---|
| `cargo check --workspace` | exits 0 |
| `cargo tree -p gpui-pre` | resolves to `vendor/gpui-pre` at v0.3.6; no other crate resolves from `vendor/` |
| `cargo test -p gpui-mcp --lib` | 21 tests pass; covers C02-C05 behavior through the bridge (`observer.rs` and `input.rs` tests) |
| `cargo check -p gpui-pre --tests` | fails on pristine `src/svg_renderer.rs` `include_bytes!` paths to Zed workspace assets (`assets/fonts/...`) that this fork does not carry; C06's unit test type-checks but cannot run in this tree |
