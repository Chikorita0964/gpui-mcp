# Batch B Rewire Coverage

What the gpui-kit rewire covered per fork patch, and what it could not. Pairs with
`docs/rewire-survey.md` (Batch A).

## Gate state

| Command | Result |
|---|---|
| `cargo check -p gpui-mcp --tests` | exit 0, no diagnostics |
| `cargo check --workspace` | exit 0, no diagnostics |
| `cargo check -p gpui-mcp-html --tests` | exit 0, no diagnostics |
| `cargo check -p gpui-mcp-html --features visual-parity` | exit 0, no diagnostics |

## Coverage

| Patch | Covered | Could not | Citation (gpui-pre 0.3.6) |
|---|---|---|---|
| **P-A** Observe each completed AccessKit tree | `observer.rs` pulls `Window::debug_a11y_tree_json()` and publishes role, label, value, state, parentage, and per-node actions; identities are frame-unique (own element id, else the shortest unique trailing ancestor path, else `path#accesskit_id`); the window host node is skipped so application roots are the tree roots | The tree exists only after a platform adapter activates it; per-node bounds are absent from the dump; `hidden` and `disabled` state is absent; application metadata and redaction are absent; the overlay paint pass has no stock hook; only explicitly-roled elements become nodes, so id-bearing containers without a role, plain text without an id, and the patch's inferred Hover/Drag actions are gone; readable element ids exist only in `debug_assertions` builds | `window.rs:1607` (activation flag starts `false`), `element.rs:370` (a node needs `is_active()` and `a11y_role()`), `window/a11y.rs:150` (`node_bounds` is `pub(crate)`), `window/a11y/debug.rs:174` (the dump's field list), `window.rs:2610` (`on_next_frame`, the only post-frame hook) |
| **P-B** Focus and text replacement through the active input handler | Focus goes through `Window::focus(&FocusHandle)` (`input.rs::dispatch_focus`); synthetic keyboard input runs `Window::dispatch_keystroke` | Text replacement is unreachable; a semantic node id cannot be mapped to a `FocusHandle` from outside the crate | `window.rs:989` (`input_handlers` is `pub(crate)`), `window/a11y.rs:149` (`focus_ids` is `pub(crate)`), `window.rs:558` (`FocusHandle::for_id` is `pub(crate)`), `window.rs:6805` (`handle_a11y_action` is `pub(crate)`) |
| **P-C** Pointer ownership | Synthetic pointer events run GPUI's native pipeline (`Window::dispatch_event`), and `DispatchEventResult` is public | `bounds_changed` still overwrites the synthetic pointer position with the platform's, so a resize or DPI change mid-hover reverts to the physical pointer | `window.rs:2695` |
| **P-D** Font-family fallback preserving requested weight, style, and OpenType features | Nothing in the bridge; the fallback stack is GPUI-internal | The stack is built from bare `font(family)` calls, so a fallback face loses the requested weight and style | `text_system.rs:255` (stack construction), `text_system.rs:370` (`resolve_font`) |

This table is the Batch B snapshot, and its citations name stock 0.3.6 behavior.
Batch C closed the activation (C02), bounds (C03), focus and text replacement
(C04), pointer (C05), fallback (C06), and hidden/disabled state (C08) gaps
through the patches in `vendor/PATCHES.md`; redaction is read from
`Role::PasswordInput` bridge-side.

## Carried bridge-side edits

| Item | Where | Reason |
|---|---|---|
| Provenance metadata and action hints (`frame_metadata`, `frame_action`) | `crates/gpui-mcp-html/src/render.rs` | Dropped: the JSON carries neither, and actions now come from `on_action` (P-B row) |
| Redaction (`frame_redacted`) | `crates/gpui-mcp/src/observer.rs` | Replaced: `Role::PasswordInput` publishes redacted, empty text with no value or text-entry action |
| Hidden and disabled semantics (`aria_hidden`, `aria_disabled`) | `crates/gpui-mcp-html/src/render.rs`, `examples/demo/src/main.rs` | Restored through C08 |
| HTML runtime test target | `crates/gpui-mcp-html/tests/runtime.rs` | Compiles; Batch C opened focus through `Window::a11y_focus_handle` and text replacement through `Window::replace_input_text`, so its hooks and assertions rewire onto those. Its remaining runtime assertions (advertised Hover, bounds-based dispatch, tree contents) depend on the Batch C activation and bounds wiring |
| Visual parity bin focus path | `crates/gpui-mcp-html/src/bin/visual.rs` | The `visual-parity` required-features gate is unchanged; Batch C opened focus through `Window::a11y_focus_handle`, so the bin's focus path rewires onto it |
| `dispatch_focus` unused outside tests | `crates/gpui-mcp/src/input.rs:184` | Resolved: `service.rs` resolves the node's handle through `Window::a11y_focus_handle` and calls `dispatch_focus`, so the warning is gone |
