# GPUI-pre patch inventory

`vendor/gpui-pre` is `gpui-pre` 0.3.6 from crates.io: the crate GPUI Kit
re-exports as `gpui`, which `gpui-mcp` aliases in the workspace manifest.
Focused patches open APIs the bridge needs but cannot reach through the stock
surface. Each patch site is marked with a `gpui-mcp patch (Cxx):` comment.

The set falls into two classes:

- **Gating and visibility**: C01 (vendored crate and workspace patch entry),
  C02 (accessibility activation), C03 (`Window::a11y_node_bounds`), C04's
  `Window::a11y_focus_handle`, and C08 (`aria_hidden`/`aria_disabled` forwarded
  to AccessKit's existing flags). These open an existing implementation and add
  no behavior beyond the gate they open.
- **Ported fork-patch behavior**: C04's document-range probe in
  `PlatformInputHandler::replace_all_text` (called from
  `Window::replace_input_text`), C05's pointer-tracking field with move detection
  in `bounds_changed`, and C06's trait-fill in `resolve_font`. These carry
  behavior the fork's own patch applied before the rewire — pointer ownership
  and synthetic-input preservation from P-C, fallback traits from P-D, and
  C11's clickable-div role and C12's pointer interactions from P-A — as
  recorded in `GPUI_MCP_REWIRE.md` and the fork's original
  `vendor/gpui/PATCHES.md`.

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

## C02 - Accessibility activation (superseded by C15)

C02 started `a11y_active_flag` as `true`, so every window built its tree every
frame and sent it to the platform adapter. C15 replaces it: the flag is stock
again and the bridge turns the tree on only while a client uses it.

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
| File | `vendor/gpui-pre/src/platform.rs` and `vendor/gpui-pre/src/window.rs` |
| Item | `PlatformInputHandler::replace_all_text` (platform.rs, after `dispatch_input`); `Window::insert_input_text` and `Window::replace_input_text` (window.rs, after `Window::focus`); plus `Window::a11y_focus_handle` (window.rs, next to `debug_a11y_tree_json`) |
| Opened | Text insertion and replacement through the focused element's live `PlatformInputHandler` (`dispatch_input` and `replace_all_text`), used by `input.rs`; and resolution of an accessibility `NodeId` to its `FocusHandle`, used by the `Focus` operation in `service.rs`. |
| Why | The platform window and its handler slot are crate-private, so the live handler is unreachable from the bridge; `A11y::focus_ids` (`window/a11y.rs:149`) and `FocusHandle::for_id` (`window.rs:558`) are `pub(crate)`, and the only stock `NodeId`-to-focus path is `Window::handle_a11y_action` (`window.rs:6805`), also `pub(crate)`. The stock handler wrappers (`text_for_range`, `replace_text_in_range`) re-enter the window through `AsyncWindowContext::update`, so `replace_all_text` reaches the handler with the caller's window and context instead. |

```rust
// vendor/gpui-pre/src/platform.rs, after `dispatch_input`
/// gpui-mcp patch (C04): replace the complete document owned by this
/// handler.
///
/// Reaches the handler with the caller's window and context, so
/// `Window::replace_input_text` does not re-enter the window through
/// `AsyncWindowContext::update`.
pub fn replace_all_text(&mut self, text: &str, window: &mut Window, cx: &mut App) -> bool {
    let mut document_range = None;
    if self
        .handler
        .text_for_range(0..usize::MAX, &mut document_range, window, cx)
        .is_none()
    {
        return false;
    }
    let Some(document_range) = document_range else {
        return false;
    };
    self.handler
        .replace_text_in_range(Some(document_range), text, window, cx);
    true
}

// vendor/gpui-pre/src/window.rs, after `Window::focus`
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
/// input handler through `PlatformInputHandler::replace_all_text`.
///
/// Returns `false` when the handler cannot provide its document range.
pub fn replace_input_text(&mut self, text: &str, cx: &mut App) -> bool {
    let Some(mut input_handler) = self.platform_window.take_input_handler() else {
        return false;
    };
    let replaced = input_handler.replace_all_text(text, self, cx);
    self.platform_window.set_input_handler(input_handler);
    replaced
}

// vendor/gpui-pre/src/window.rs, next to `debug_a11y_tree_json`
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

The same rule covers platform input. `on_input` routes through a new
`Window::dispatch_platform_input`. It drops a platform `MouseMove` whose position
equals `platform_mouse_position` while a synthetic position is active, and it
records the platform position on every other move, down, and up. Windows sends
such a move when a Graphics Capture session starts on the window. Before this,
every screenshot put the pointer back at the physical cursor and cancelled a
synthetic hover. `region_capture` failed whenever the physical cursor rested
over the window. `synthetic_hover_survives_a_platform_move_that_did_not_move`
covers it.

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

## C08 - Hidden and disabled state

| | |
|---|---|
| File | `vendor/gpui-pre/src/elements/div.rs` and `vendor/gpui-pre/src/window/a11y/debug.rs` |
| Item | Two fields at the end of `AriaProperties`; `StatefulInteractiveElement::aria_hidden` and `aria_disabled` after `aria_toggled`; two `set_*` calls in `Interactivity::write_a11y_info` after `column_count`; two keys in `node_to_json` after `orientation` |
| Opened | `observer.rs` publishes `NodeState::visible` and `enabled` from the tree, and inherits hidden into descendants. The HTML runtime and the demo's locked control set them. |
| Why | AccessKit's `Node::set_hidden` / `set_disabled` exist, but the stock `aria_*` builders stop at `aria_column_count`, so no element can set either flag, and the dump emits neither. Every node therefore read `visible: true, enabled: true`; `gpui-mcp-server/tests/disabled_state.rs` proves the gap. |

```rust
// AriaProperties, after `column_count`
pub(crate) hidden: bool,
pub(crate) disabled: bool,

// StatefulInteractiveElement, after `aria_toggled`
fn aria_hidden(mut self, hidden: bool) -> Self {
    self.interactivity().aria.hidden = hidden;
    self
}
fn aria_disabled(mut self, disabled: bool) -> Self {
    self.interactivity().aria.disabled = disabled;
    self
}

// Interactivity::write_a11y_info, after `column_count`
if self.aria.hidden {
    node.set_hidden();
}
if self.aria.disabled {
    node.set_disabled();
}

// window/a11y/debug.rs node_to_json, after `orientation`
if node.is_hidden() {
    aria.insert("hidden".into(), json!(true));
}
if node.is_disabled() {
    aria.insert("disabled".into(), json!(true));
}
```

Not carried from the pre-rewire fork: `aria_read_only` (no protocol field reads
it), and `frame_redacted` / `frame_metadata` / `frame_action`. Redaction needs
no patch: `observer.rs` treats AccessKit's `Role::PasswordInput` as redacted.
`frame_action(SetText)` needs no patch either: the HTML runtime registers an
`on_a11y_action(SetValue)` listener on each editable text input, as
`gpui-component`'s `Input` does.

## C11 - Clickable divs report a role

| | |
|---|---|
| File | `vendor/gpui-pre/src/elements/div.rs` |
| Item | `Div::a11y_role`, one `.or_else` after the `GenericContainer` filter |
| Opened | A `div` with `on_click` and no `.role(...)` reports `Role::Button`, so it enters the tree and its existing `Click` action is reachable. An explicit `.role(...)` still wins. |
| Why | Stock `write_a11y_info` already adds `Action::Click` for any click listener, but `element.rs` only emits a node when `a11y_role()` is `Some`, and stock `a11y_role` returns only `override_role`. Every role-less clickable div was invisible, as the demo's `increment`/`reset` were. The pre-rewire fork patch carried this same rule. |

```rust
// Div::a11y_role, after `.filter(|role| *role != accesskit::Role::GenericContainer)`
.or_else(|| {
    (!self.interactivity.click_listeners.is_empty()).then_some(accesskit::Role::Button)
})
```

## C12 - Pointer interactions per node

| | |
|---|---|
| File | `vendor/gpui-pre/src/window/a11y.rs`, `element.rs`, `elements/div.rs`, `window.rs` |
| Item | `A11yPointerInteractions` (new public struct) and an `A11y::pointer_interactions` map cleared in `begin_frame`; `Element::a11y_pointer_interactions` (default: none), recorded beside `node_bounds` where the node is pushed; `Interactivity::a11y_pointer_interactions` after `write_a11y_info`, forwarded by `Div` and `Stateful`; `Window::a11y_pointer_interactions` beside `a11y_node_bounds`; re-exported next to `A11ySubtreeBuilder` |
| Opened | `observer.rs` reports `Hover`, `Drag`, and `Scroll`, so `hover_element`, `drag_element`, and `scroll` by id work again. The flags are read from listeners and styles the element already holds; nothing is dispatched differently. |
| Why | AccessKit has no hover or drag action, and stock `write_a11y_info` adds no scroll action, so the tree cannot say which nodes accept them and the server's action gates rejected every node. The pre-rewire fork patch inferred the same three from the same fields (`FrameNode::actions`). Unlike C03 this is new surface, not a visibility change, because stock GPUI keeps no such record. |

```rust
// Interactivity, after write_a11y_info
pub(crate) fn a11y_pointer_interactions(&self) -> crate::A11yPointerInteractions {
    crate::A11yPointerInteractions {
        hover: self.hover_style.is_some()
            || self.group_hover_style.is_some()
            || self.hover_listener.is_some()
            || !self.mouse_move_listeners.is_empty()
            || self.tooltip_builder.is_some(),
        drag: self.drag_listener.is_some(),
        scroll: self.scroll_offset.is_some()
            || self.tracked_scroll_handle.is_some()
            || !self.scroll_wheel_listeners.is_empty(),
    }
}

// element.rs, after `window.a11y.node_bounds.insert(node_id, bounds);`
window
    .a11y
    .pointer_interactions
    .insert(node_id, self.element.a11y_pointer_interactions());
```

## C13 - Element ids in every build

| | |
|---|---|
| File | `vendor/gpui-pre/src/window/a11y.rs`, `element.rs`, `window.rs` |
| Item | An `A11y::element_ids` map cleared in `begin_frame`; the node's leaf `ElementId` inserted beside `pointer_interactions` where the node is pushed; `Window::a11y_element_id` beside `a11y_pointer_interactions` |
| Opened | `observer.rs` names nodes by their element id in release builds too (`increment`, `lock-toggle`), instead of an AccessKit number (`12369605594182645555`). |
| Why | Stock GPUI records the element id only as `cfg(debug_assertions)` provenance for the debug dump (`NodeDebugInfo::element_id`), so a release app had no stable names to find elements by. The id is already in `GlobalElementId`; this keeps the leaf the node was built from. |

```rust
// element.rs, after the C12 insert
if let Some(leaf) = global_id.0.last() {
    window.a11y.element_ids.insert(node_id, leaf.clone());
}
```

## C14 - Frame observer

| | |
|---|---|
| File | `vendor/gpui-pre/src/window/a11y.rs`, `window/a11y/debug.rs`, `window.rs` |
| Item | `A11yFrameObserver` and `A11yFrame`; `Window::add_a11y_frame_observer` and `Window::is_redraw_pending`; `draw_roots` times prepaint and paint, calls each observer's `paint_overlay` after the inspector hitbox, and `frame_finished` after `A11y::end_frame`; `end_frame` also returns whether the tree, GPUI focus, active descendant, or pointer interactions differ from the previous tree frame; `A11yDebug::capture` clones the tree only when it changed |
| Opened | `observer.rs` sees every drawn frame as it completes, including frames the app draws on its own, reads AccessKit nodes directly instead of the JSON dump, skips unchanged frames, reports measured prepaint and paint times, and paints highlight overlays. |
| Why | Stock GPUI exposes the tree only through `debug_a11y_tree_json`, which serializes every node, and only `on_next_frame`, which runs before the next draw. A pulled tree therefore lagged one frame, missed app-driven frames, cost a full serialize and parse per observation, and nothing could paint over the frame. |

## C15 - Observation on request

| | |
|---|---|
| File | `vendor/gpui-pre/src/window/a11y.rs`, `window.rs` |
| Item | `A11y::observed`, read by `sync_active_flag`; `A11y::platform_active`; `Window::set_a11y_observed` and `Window::is_a11y_observed`; `draw_roots` sends tree updates to the platform only when the platform flag is set |
| Opened | The bridge builds the tree only while a client uses it and stops after 30 seconds of silence; an app without a client pays nothing. Isolated automation (tests, previews) observes from attach. |
| Why | Stock activation comes only from the platform adapter. C02 forced it on for every window forever, which also pushed every frame's tree to the OS adapter. |

## Fields read without a patch

C03 and C04 read `A11y::node_bounds` and `A11y::focus_ids` from `window.rs`,
which is in the same crate, so those `pub(crate)` fields stay as they are.

## Verification in this fork

| Command | Result |
|---|---|
| `cargo check --workspace` | exits 0 |
| `cargo tree -p gpui-pre` | resolves to `vendor/gpui-pre` at v0.3.6; no other crate resolves from `vendor/` |
| `cargo test -p gpui-mcp --lib` | 31 tests pass; covers C03-C05, C08, and C11-C15 behavior through the bridge (`observer.rs`, `registry.rs`, `service.rs`, and `input.rs` tests) |
| `cargo test -p gpui-mcp-server --test disabled_state` | passes; C08 over the real MCP stdio surface against the demo |
| `cargo check -p gpui-pre --tests` | fails on pristine `src/svg_renderer.rs` `include_bytes!` paths to Zed workspace assets (`assets/fonts/...`) that this fork does not carry; C06's unit test type-checks but cannot run in this tree |
