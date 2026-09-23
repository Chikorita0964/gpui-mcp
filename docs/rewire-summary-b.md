# Batch B Rewire Coverage

What the gpui-kit rewire covered per fork patch, and what it could not. Pairs with
`docs/rewire-survey.md` (Batch A).

## Gate state

| Command | Result |
|---|---|
| `cargo check -p gpui-mcp --tests` | exit 0, no diagnostics |
| `cargo check --workspace` | exit 0; one warning: `crates/gpui-mcp/src/input.rs:184` (`dispatch_focus` unused outside tests) |
| `cargo check -p gpui-mcp-html --tests` | exit 0; the same single warning |
| `cargo check -p gpui-mcp-html --features visual-parity` | exit 0; the same single warning |

## Coverage

| Patch | Covered | Could not | Citation (gpui-pre 0.3.6) |
|---|---|---|---|
| **P-A** Observe each completed AccessKit tree | `observer.rs` pulls `Window::debug_a11y_tree_json()` and publishes role, label, value, state, parentage, and per-node actions; identities are frame-unique (own element id, else the shortest unique trailing ancestor path, else `path#accesskit_id`); the window host node is skipped so application roots are the tree roots | The tree exists only after a platform adapter activates it; per-node bounds are absent from the dump; `hidden` and `disabled` state is absent; application metadata and redaction are absent; the overlay paint pass has no stock hook; only explicitly-roled elements become nodes, so id-bearing containers without a role, plain text without an id, and the patch's inferred Hover/Drag actions are gone; readable element ids exist only in `debug_assertions` builds | `window.rs:1607` (activation flag starts `false`), `element.rs:370` (a node needs `is_active()` and `a11y_role()`), `window/a11y.rs:150` (`node_bounds` is `pub(crate)`), `window/a11y/debug.rs:174` (the dump's field list), `window.rs:2610` (`on_next_frame`, the only post-frame hook) |
| **P-B** Focus and text replacement through the active input handler | Focus goes through `Window::focus(&FocusHandle)` (`input.rs::dispatch_focus`); synthetic keyboard input runs `Window::dispatch_keystroke` | Text replacement is unreachable; a semantic node id cannot be mapped to a `FocusHandle` from outside the crate | `window.rs:989` (`input_handlers` is `pub(crate)`), `window/a11y.rs:149` (`focus_ids` is `pub(crate)`), `window.rs:558` (`FocusHandle::for_id` is `pub(crate)`), `window.rs:6805` (`handle_a11y_action` is `pub(crate)`) |
| **P-C** Pointer ownership | Synthetic pointer events run GPUI's native pipeline (`Window::dispatch_event`), and `DispatchEventResult` is public | `bounds_changed` still overwrites the synthetic pointer position with the platform's, so a resize or DPI change mid-hover reverts to the physical pointer | `window.rs:2695` |
| **P-D** Font-family fallback preserving requested weight, style, and OpenType features | Nothing in the bridge; the fallback stack is GPUI-internal | The stack is built from bare `font(family)` calls, so a fallback face loses the requested weight and style | `text_system.rs:255` (stack construction), `text_system.rs:370` (`resolve_font`) |

## Carried bridge-side edits

| Item | Where | Reason |
|---|---|---|
| Provenance metadata, redaction, and action hints (`frame_metadata`, `frame_redacted`, `frame_action`) | `crates/gpui-mcp-html/src/render.rs` | Dropped: the JSON carries none of them (P-A row), and actions now come from `on_action` (P-B row) |
| Hidden and disabled semantics (`aria_hidden`, `aria_disabled`) | `crates/gpui-mcp-html/src/render.rs`, `examples/demo/src/main.rs:133` | Dropped: stock GPUI has no such setters (the `aria_*` list in `elements/div.rs`) |
| HTML runtime test target | `crates/gpui-mcp-html/tests/runtime.rs` | Compiles again: the focus hook and the replaced-text assertion now mirror the bridge's `Unsupported` referrals. Its remaining runtime assertions (advertised Hover, bounds-based dispatch, tree contents) await C02 and C03 |
| Visual parity bin focus path | `crates/gpui-mcp-html/src/bin/visual.rs` | Returns the same `Unsupported` referral; the `visual-parity` required-features gate is unchanged |
| `dispatch_focus` unused outside tests | `crates/gpui-mcp/src/input.rs:184` | Warning only; the call site returns the P-B focus error until Batch C exposes the mapping |
