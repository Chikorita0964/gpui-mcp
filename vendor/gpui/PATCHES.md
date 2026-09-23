# GPUI patch inventory

This directory tracks Zed GPUI at commit
`82878540b5410b288a2c92cb9ee5675533e4d807` (`gpui` 0.2.2).

gpui-mcp carries four focused additions that are not available from upstream
GPUI yet:

- read-only observation of each completed rendered AccessKit tree, with stable
  GPUI element paths, frame-unique node identities, bounds, text provenance,
  and an overlay paint pass. A node keeps its own element id while that id
  names it alone, and otherwise takes the shortest trailing run of its element
  path that separates it, so identities are deliberately **not uniform in
  shape**: two siblings can look nothing alike because one collided and the
  other did not, and adding a widget to one container can change the shape of
  identities in a container nobody touched. Consumers must group nodes by
  `parent`, which is exact and resolved from paths, and never by the form of
  the identity itself — a matcher keyed on an identity pattern selects exactly
  the nodes that collided and silently skips the ones that did not;
- programmatic focus and text replacement through GPUI's active input handler;
- pointer ownership that prevents a stale native mouse position from cancelling
  a synthetic hover before the physical mouse actually moves; and
- font-family fallback that preserves the requested weight, style, and OpenType
  features.

The bridge uses GPUI's standard `Role` and `aria_*` APIs. It does not maintain a
second semantic tree. Native window identity is obtained in `gpui-mcp` through
`raw-window-handle`, so it does not require a GPUI patch.

Each item should be removed here as soon as an equivalent upstream API is
available. The rest of this directory is an unmodified snapshot of that Zed
commit, adjusted only so the crate can build outside the Zed workspace.
