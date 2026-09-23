# gpui-mcp Rewire — Batch A Compilation Survey

Binding state after A01 and the error classification that is the work plan for
Batches B and C (A02). Evidence: `cargo check --workspace` in the fork, Rust
1.96.0.

## Environment

| Item | Value |
|---|---|
| Command | `cargo check --workspace` |
| Result | exit 101 — one compiler error in `gpui-mcp`; every other member checks |
| Binding | `gpui = { package = "gpui-kit", version = "0.6" }` |
| Lock | `gpui-kit 0.6.6`, `gpui-pre 0.3.6`, `gpui-pre-platform 0.3.6`, `gpui-component 0.6.6`, `gpui-base 0.6.6` |
| Resolution constraint | `gpui-kit 0.6.1` is unusable: `gpui-component 0.6.1` with `gpui-component-macros 0.6.6` fails to compile (14 errors in chart `IntoPlot` derives). Keep the lock on the 0.6.6 family. |

Checked clean: `gpui-mcp-protocol`, `gpui-mcp-capture`, `gpui-mcp-server`, and
the whole `gpui-pre` / `gpui-kit` graph. Not type-checked because they depend
on `gpui-mcp`: `crates/gpui-mcp-html`, `examples/demo`, `examples/runtime-showcase`.

## 1. Compiler-reported errors (resolution phase)

```
error[E0432]: unresolved imports `gpui::AccessibilityFrame`, `gpui::FrameAction`,
`gpui::FrameNode`, `gpui::FrameObserver`
 --> crates\gpui-mcp\src\observer.rs:4:5
```

| file:line | Missing item | Patch | Candidate native API (gpui-pre 0.3.6) |
|---|---|---|---|
| `crates/gpui-mcp/src/observer.rs:4` | `gpui::AccessibilityFrame` | P-A | `Window::debug_a11y_tree_json()` (`window.rs:6782`) for the tree; per-node bounds need `A11y::node_bounds` (`window/a11y.rs:150`, `pub(crate)` → C03) |
| `crates/gpui-mcp/src/observer.rs:4` | `gpui::FrameNode` | P-A | JSON node fields: `element_id`, `view`, `source_location`, `children`, `root`, `focus`, role/label/value/state, `on_action` |
| `crates/gpui-mcp/src/observer.rs:4` | `gpui::FrameAction` | P-A (B02) | JSON `on_action` per node + `Window::on_a11y_action` (`window.rs:6791`) |
| `crates/gpui-mcp/src/observer.rs:4` | `gpui::FrameObserver` | P-A | No native trait. Poll `debug_a11y_tree_json()`; overlay pass candidate `Window::on_next_frame` (`window.rs:2610`) |

This error aborts `gpui-mcp` before type-checking runs, so §2 is a source
audit against the gpui-pre 0.3.6 registry source, not compiler output.

## 2. Missing APIs masked by the resolution abort (source audit)

| file:line | Missing item | Patch | Candidate native API | Batch |
|---|---|---|---|---|
| `crates/gpui-mcp/src/lib.rs:71` | `Window::observe_frames` | P-A | drop the registration; observation reads `debug_a11y_tree_json()` (`window.rs:6782`) | B01 |
| `crates/gpui-mcp/src/observer.rs:24–75` | `impl FrameObserver` (`frame_started`, `accessibility_updated`, `paint_started`, `paint_overlay`, `frame_finished`) | P-A | frame work rebuilt from the JSON snapshot; overlay via `on_next_frame` (`window.rs:2610`) | B01 |
| `crates/gpui-mcp/src/observer.rs:35–41, 77–143` | `AccessibilityFrame::nodes/accessibility_node/tree`; `FrameNode::path/id/parent/metadata/bounds/content_text/is_redacted/fallback_role/accessibility_id` | P-A | JSON `element_id` / `view` / `source_location` / `children` / `root` + node fields; parentage rebuilt from `children` — group by `parent`, never by identity shape | B01 |
| `crates/gpui-mcp/src/observer.rs:145–189` | `FrameNode::actions`; `FrameAction::{Hover,Drag,Scroll,SetText,SetValue}` | P-A (B02) | JSON `on_action` list; action requests via `on_a11y_action` (`window.rs:6791`) | B02 |
| `crates/gpui-mcp/src/input.rs:143` | `Window::insert_input_text` | P-B | `Window::focus` (`window.rs:2302`) + `PlatformInputHandler::replace_text_in_range` (`platform.rs:1720`, `pub`) reached through `Window::input_handlers` (`window.rs:989`, `pub(crate)` → C04) | B04/C04 |
| `crates/gpui-mcp/src/input.rs:147` | `Window::replace_input_text` | P-B | same as above | B04/C04 |
| `crates/gpui-mcp/src/service.rs:601` | `Window::focus_observed_element` | P-B | `Window::focus(&FocusHandle, cx)` (`window.rs:2302`); node id → `FocusHandle` mapping is the rewire work | B04/B05 |

## 3. Native APIs present — no patch, no rewire beyond naming

| Bridge use | gpui-pre 0.3.6 |
|---|---|
| `Window::dispatch_event` returning `DispatchEventResult` | `window.rs:5415`; `pub struct DispatchEventResult` at `window.rs:2117` — P-C's visibility patch is native |
| `Window::dispatch_keystroke` | `window.rs:5373` |
| `Window::focus` | `window.rs:2302` |
| `Window::mouse_position` | `window.rs:3204` |
| `Window::paint_quad`, `refresh`, `bounds`, `viewport_size`, `scale_factor` | `window.rs:4510`, `2272`, `2705`, `2770`, `2933` |
| `Window::debug_a11y_tree_json`, `on_a11y_action` | `window.rs:6782`, `6791` |
| OS window id | bridge `native_window.rs:5` uses `raw_window_handle`; no `Window::native_window_id` needed |
| `gpui_platform::application()` | gpui-kit re-export of `gpui-pre-platform`; the alias entry satisfies it |

## 4. Runtime/visibility gaps — no compile error, Batch C referrals

| Gap | gpui-pre 0.3.6 | Effect | Item |
|---|---|---|---|
| A11y tree inactive until assistive technology activates it | `window.rs:1607` (`a11y_active_flag = false`), activation callback `1627` | `debug_a11y_tree_json()` returns `None` on a freshly opened window | C02 |
| Per-node bounds not exposed | `window/a11y.rs:150` `pub(crate) node_bounds` | bounds absent from the JSON; `geometry.csv` comparison needs them | C03 |
| Live input handler unreachable | `window.rs:989` `pub(crate) input_handlers` | text replacement cannot reach `replace_text_in_range` | C04 |
| Synthetic pointer position overwritten | `window.rs:2691–2693` `bounds_changed` → `self.mouse_position = self.platform_window.mouse_position()` | resize or DPI change mid-hover reverts to the physical pointer | C05 |
| Fallback stack drops requested weight/style/features | `text_system.rs:255` bare `font(family)` entries; `resolve_font` at `370` | bold Han fallback renders regular weight (`09-language-zh.png`) | C06 |

## 5. Referrals and notes

- `gpui_platform` entry retained as `{ package = "gpui-pre-platform", version = "0.3" }`; three members still name it (`examples/demo/Cargo.toml:11`, `examples/runtime-showcase/Cargo.toml:11`, `crates/gpui-mcp-html/Cargo.toml:31`). It is the same crate gpui-kit re-exports as `gpui::platform`; deleting the entry without rewiring those members aborts resolution. Batch B may drop it once those call sites use `gpui::platform`.
- `crates/gpui-mcp-html/src/scaffold.rs` (lines 266, 279, 392, 664) still emits `ZED_REPOSITORY` / `ZED_REVISION` into generated projects — outside A01's acceptance (workspace manifest), B05 scope.
- `vendor/README.md` still describes the deleted `vendor/gpui` tree; not in A01 ownership.
