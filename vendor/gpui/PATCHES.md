# GPUI patch inventory

This directory tracks Zed GPUI at commit
`16c9aa7ea6d897a8044d9501cde1b295256722f2` (`gpui` 0.2.2).

gpui-mcp carries six focused additions that are not available from upstream
GPUI yet. Each was last checked against `zed-industries/zed` `main` on
2026-09-22, at the commit named above:

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
  the bridge, including labels derived from content;
- `aria_disabled`, `aria_hidden`, and `aria_read_only` builders that forward to
  the corresponding AccessKit node states. An ID-bearing, hidden `Div` with no
  other role reports `Role::Group` so the hidden state covers its descendants;
- programmatic focus and text replacement through GPUI's active input handler
  (`Window::insert_input_text`, `Window::replace_input_text`);
- pointer ownership that prevents a stale native mouse position from cancelling
  a synthetic hover before the physical mouse actually moves; and
- font-family fallback that preserves the requested weight, style, and OpenType
  features.

Two earlier additions are no longer needed and are not carried:

- `DispatchEventResult` was made public so callers could use `Window::
  dispatch_event`; upstream now exposes both publicly.
- `Window::native_window_id`; `gpui-mcp` obtains native window identity through
  `raw-window-handle` instead.

The bridge uses GPUI's standard `Role` and `aria_*` APIs, extended only by the
builders above. It does not maintain a second semantic tree.

Every item should be removed here as soon as an equivalent upstream API is
available. The rest of this directory is an unmodified snapshot of that Zed
commit, except that `Cargo.toml` has Zed workspace inheritance unwound (and
`[dev-dependencies]` dropped) so the crate can build outside the Zed workspace,
and two blank doc comments in `_accessibility.rs` have trailing spaces removed.
The manifest rewrite carries no behavior change.
