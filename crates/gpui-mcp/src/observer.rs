//! Read-only observation of GPUI's accessibility tree.
//!
//! GPUI hands every drawn frame to [`BridgeObserver`] through the frame
//! observer hook (patch C14), after the frame's accessibility tree is
//! finalized. GPUI owns the semantics; this module only names nodes, restores
//! parentage, and translates AccessKit nodes into the protocol's [`UiNode`].
//! A frame whose tree, focus, and pointer interactions match the previous one
//! is counted but not re-published.
//!
//! Node identity is frame-unique. A node keeps its own element id while that id
//! names it alone and otherwise takes the shortest trailing run of its
//! reconstructed ancestor path that separates it, falling back to its AccessKit
//! node id when even the full path collides. Consumers must group nodes by
//! `parent`, never by the shape of the identity itself.
//!
//! Element ids come from [`Window::a11y_element_id`] (patch C13), so release
//! builds name nodes the same way debug builds do.
//!
//! The native tree does not carry everything the patched GPUI observed, and
//! this module publishes what it carries:
//!
//! - Elements without an explicit role are not in GPUI's accessibility tree at
//!   all, so id-bearing containers without a role and plain text without an id
//!   never appear as nodes.
//! - Per-node bounds are published when the node's accessibility id resolves
//!   through [`Window::a11y_node_bounds`]; a node whose id does not resolve
//!   keeps [`UiNode::bounds`] empty.
//! - `hidden` and `disabled` come from the `aria_hidden` / `aria_disabled`
//!   builders (patch C08). A node is visible only when neither it nor an
//!   ancestor is hidden and its bounds are not empty.
//! - Redaction is AccessKit's own: a `PasswordInput` publishes redacted, empty
//!   text, no value, and no text-entry actions, and its text never reaches an
//!   ancestor's content-derived label.
//! - Application metadata is not emitted; [`UiNode::metadata`] carries only the
//!   node's `accesskit_id` for focus resolution.
//! - Hover, Drag, and Scroll come from the pointer interactions GPUI records
//!   per node (patch C12); the rest map from advertised AccessKit actions.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use gpui::accesskit::{self, Action, NodeId, Toggled, TreeUpdate};
use gpui::{
    A11yFrame, A11yFrameObserver, A11yPointerInteractions, App, BorderStyle, Window, outline,
    point, px, rgba, size,
};
use gpui_mcp_protocol::{NodeAction, NodeState, Rect, Role, TextInfo, UiNode, ValueInfo};

use crate::registry::{SharedState, rect_from_gpui};

/// Publishes drawn frames to the bridge's shared state and paints highlights.
pub(crate) struct BridgeObserver {
    state: Weak<SharedState>,
    /// The next frame that carries a tree is published even if GPUI reports
    /// it unchanged, because the published tree may predate it.
    stale: AtomicBool,
}

impl BridgeObserver {
    /// Create an observer that publishes into `state`.
    pub(crate) fn new(state: &Arc<SharedState>) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::downgrade(state),
            stale: AtomicBool::new(true),
        })
    }

    /// Build the window's accessibility tree every frame, or stop building it.
    pub(crate) fn set_observed(&self, window: &mut Window, observed: bool) {
        if observed && !window.is_a11y_observed() {
            self.stale.store(true, Ordering::Release);
        }
        window.set_a11y_observed(observed);
        if let Some(state) = self.state.upgrade() {
            state.set_observing(observed);
        }
    }
}

impl A11yFrameObserver for BridgeObserver {
    fn paint_overlay(&self, window: &mut Window, _cx: &mut App) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        state.with_highlights(|highlights| {
            for highlight in highlights {
                let Some(color) = parse_color(&highlight.color) else {
                    continue;
                };
                let rect = highlight.rect;
                window.paint_quad(outline(
                    gpui::Bounds::new(
                        point(px(rect.x), px(rect.y)),
                        size(px(rect.width), px(rect.height)),
                    ),
                    rgba(color),
                    BorderStyle::Solid,
                ));
            }
        });
    }

    fn frame_finished(&self, window: &Window, frame: &A11yFrame<'_>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut content_bounds = window.bounds();
        content_bounds.size = window.viewport_size();
        state.set_window_geometry(rect_from_gpui(content_bounds), window.scale_factor());
        if let Some(tree) = frame.tree {
            let republish = self.stale.swap(false, Ordering::AcqRel);
            if frame.changed || republish {
                state.publish_frame(frame_nodes(tree, frame.gpui_focus, |id| NodeLookup {
                    bounds: window.a11y_node_bounds(id).map(rect_from_gpui),
                    pointer: window.a11y_pointer_interactions(id).unwrap_or_default(),
                    element_id: window
                        .a11y_element_id(id)
                        .and_then(|element| element_id_name(&element)),
                }));
            }
        }
        state.complete_frame(frame.prepaint, frame.paint);
    }
}

/// Translate one finalized AccessKit tree into protocol nodes.
///
/// The tree's root is not published when it carries no element identity: it
/// is GPUI's window node, and its children are the application's roots.
/// `lookup` resolves what the window holds beside the tree: logical bounds,
/// pointer interactions, and the element id. Each published node also carries
/// its AccessKit id as `metadata["accesskit_id"]` so focus requests can
/// resolve a handle.
fn frame_nodes(
    update: &TreeUpdate,
    gpui_focus: Option<NodeId>,
    lookup: impl Fn(NodeId) -> NodeLookup,
) -> Vec<UiNode> {
    let tree = FrameTree::new(update, &lookup);
    let focus = gpui_focus.and_then(|id| tree.index.get(&id).copied());

    let order = tree.depth_first_order();
    let identities = tree.assign_identities(&order);
    let content = tree.content_texts(&order);

    // `order` is depth-first, so a parent's hidden state is known before its children.
    let mut hidden = vec![false; tree.nodes.len()];
    let mut nodes = Vec::with_capacity(order.len());
    for &index in &order {
        let node = &tree.nodes[index];
        let parent = tree.parent[index];
        hidden[index] = node.node.is_hidden() || parent.is_some_and(|parent| hidden[parent]);
        nodes.push(to_ui_node(
            node,
            PublishedNode {
                identity: identities[index].clone(),
                // The host is never published, so a child of it is a root.
                parent: parent
                    .filter(|&parent| tree.host != Some(parent))
                    .map(|parent| identities[parent].clone()),
                content: content[index].as_deref().unwrap_or_default(),
                hidden: hidden[index],
                focused: focus == Some(index),
            },
        ));
    }
    nodes
}

/// One AccessKit node with what the window holds about it.
struct RawNode<'a> {
    id: NodeId,
    node: &'a accesskit::Node,
    /// The node's element id, or its AccessKit id when it has none.
    own_id: String,
    has_element_id: bool,
    bounds: Option<Rect>,
    pointer: A11yPointerInteractions,
}

/// The tree's nodes, indexed so every relationship is a `usize`, not a lookup.
struct FrameTree<'a> {
    nodes: Vec<RawNode<'a>>,
    index: HashMap<NodeId, usize>,
    children: Vec<Vec<usize>>,
    parent: Vec<Option<usize>>,
    root: Option<usize>,
    host: Option<usize>,
}

impl<'a> FrameTree<'a> {
    fn new(update: &'a TreeUpdate, lookup: &impl Fn(NodeId) -> NodeLookup) -> Self {
        let nodes: Vec<RawNode<'a>> = update
            .nodes
            .iter()
            .map(|(id, node)| {
                let facts = lookup(*id);
                RawNode {
                    id: *id,
                    node,
                    has_element_id: facts.element_id.is_some(),
                    own_id: facts.element_id.unwrap_or_else(|| id.0.to_string()),
                    bounds: facts.bounds,
                    pointer: facts.pointer,
                }
            })
            .collect();
        let index: HashMap<NodeId, usize> = nodes
            .iter()
            .enumerate()
            .map(|(position, node)| (node.id, position))
            .collect();
        let children: Vec<Vec<usize>> = nodes
            .iter()
            .map(|node| {
                node.node
                    .children()
                    .iter()
                    .filter_map(|child| index.get(child).copied())
                    .collect()
            })
            .collect();
        let mut parent = vec![None; nodes.len()];
        for (position, node_children) in children.iter().enumerate() {
            for &child in node_children {
                parent[child] = Some(position);
            }
        }
        let root = update
            .tree
            .as_ref()
            .and_then(|tree| index.get(&tree.root).copied());
        let host = root.filter(|&root| !nodes[root].has_element_id);
        Self {
            nodes,
            index,
            children,
            parent,
            root,
            host,
        }
    }

    /// Every node reachable from the root's children, depth first in listed
    /// order, then any unreachable node in AccessKit id order. The host is skipped.
    fn depth_first_order(&self) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut visited = vec![false; self.nodes.len()];
        if let Some(root) = self.root {
            self.collect_order(&self.children[root], &mut visited, &mut order);
        }
        let mut leftovers: Vec<usize> = (0..self.nodes.len())
            .filter(|&index| !visited[index])
            .collect();
        leftovers.sort_by_key(|&index| self.nodes[index].id.0);
        self.collect_order(&leftovers, &mut visited, &mut order);
        order
    }

    fn collect_order(&self, start: &[usize], visited: &mut [bool], order: &mut Vec<usize>) {
        let mut stack: Vec<usize> = start.iter().rev().copied().collect();
        while let Some(index) = stack.pop() {
            if self.host == Some(index) || visited[index] {
                continue;
            }
            visited[index] = true;
            order.push(index);
            stack.extend(self.children[index].iter().rev());
        }
    }

    /// The own ids of `index` and its ancestors, root first, stopping below the host.
    fn ancestor_path(&self, index: usize) -> Vec<&str> {
        let mut path = Vec::new();
        let mut current = Some(index);
        while let Some(node) = current {
            if self.host == Some(node) {
                break;
            }
            path.push(self.nodes[node].own_id.as_str());
            current = self.parent[node];
        }
        path.reverse();
        path
    }

    /// Name every published node so that no two nodes share an identity.
    ///
    /// This is the same rule the patched GPUI applied to element paths, applied
    /// to the ancestor path this module can reconstruct from the tree. Candidate
    /// suffixes are compared as borrowed segment slices; a string is built once
    /// per node, for the identity it settles on.
    fn assign_identities(&self, order: &[usize]) -> Vec<String> {
        let segments: Vec<Vec<&str>> = (0..self.nodes.len())
            .map(|index| self.ancestor_path(index))
            .collect();
        let mut identities = vec![String::new(); self.nodes.len()];
        let mut taken: HashSet<String> = HashSet::with_capacity(order.len());
        let mut unresolved: Vec<usize> = order.to_vec();
        let mut depth = 1usize;
        while !unresolved.is_empty() {
            let mut counts: HashMap<&[&str], usize> = HashMap::with_capacity(unresolved.len());
            for &index in &unresolved {
                *counts.entry(suffix(&segments[index], depth)).or_default() += 1;
            }
            let exhausted = unresolved
                .iter()
                .all(|&index| segments[index].len() <= depth);
            let mut still = Vec::new();
            let mut settled = Vec::new();
            for index in unresolved {
                let candidate = suffix(&segments[index], depth);
                if counts.get(candidate).copied() == Some(1) {
                    let joined = candidate.join("/");
                    if !taken.contains(&joined) {
                        identities[index] = joined;
                        settled.push(index);
                        continue;
                    }
                }
                if exhausted {
                    identities[index] =
                        format!("{}#{}", segments[index].join("/"), self.nodes[index].id.0);
                    settled.push(index);
                    continue;
                }
                still.push(index);
            }
            for index in settled {
                taken.insert(identities[index].clone());
            }
            unresolved = still;
            depth += 1;
        }
        identities
    }

    /// Normalized descendant text of every node in `order`: each text-role
    /// child's value and each descendant's own collected text, in tree order.
    fn content_texts(&self, order: &[usize]) -> Vec<Option<String>> {
        let mut memo: Vec<Option<String>> = vec![None; self.nodes.len()];
        // Children come after their parent in depth-first order, so fold back to front.
        for &index in order.iter().rev() {
            let mut joined = String::new();
            for &child in &self.children[index] {
                let child_node = self.nodes[child].node;
                if is_redacted(child_node.role()) {
                    continue;
                }
                if is_text_role(child_node.role())
                    && let Some(value) = child_node.value()
                {
                    push_words(&mut joined, value);
                }
                if let Some(child_text) = memo[child].as_deref() {
                    push_words(&mut joined, child_text);
                }
            }
            memo[index] = (!joined.is_empty()).then_some(joined);
        }
        memo
    }
}

/// The trailing `depth` segments of a path.
fn suffix<'a, 'b>(segments: &'a [&'b str], depth: usize) -> &'a [&'b str] {
    &segments[segments.len().saturating_sub(depth)..]
}

/// Append `text`'s words to `joined`, single-space separated.
fn push_words(joined: &mut String, text: &str) {
    for word in text.split_whitespace() {
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(word);
    }
}

/// What the window holds about one node beside the tree.
#[derive(Default)]
struct NodeLookup {
    bounds: Option<Rect>,
    pointer: A11yPointerInteractions,
    element_id: Option<String>,
}

/// What `frame_nodes` resolves about a node from the whole tree.
struct PublishedNode<'a> {
    identity: String,
    parent: Option<String>,
    content: &'a str,
    hidden: bool,
    focused: bool,
}

/// Build one protocol node from its own semantics and its tree context.
fn to_ui_node(raw: &RawNode<'_>, published: PublishedNode<'_>) -> UiNode {
    let node = raw.node;
    let role = node.role();
    let redacted = is_redacted(role);
    let label = node
        .label()
        .map(str::to_owned)
        .or_else(|| label_from_content(role, published.content));
    let text_value = if redacted {
        None
    } else {
        node.value()
            .map(str::to_owned)
            .or_else(|| (!published.content.is_empty()).then(|| published.content.to_owned()))
    };
    let text = is_text_role(role).then(|| TextInfo {
        text: text_value.clone().unwrap_or_default(),
        caret: None,
        selection: None,
        redacted,
    });
    let value =
        (!redacted && (node.numeric_value().is_some() || node.value().is_some())).then(|| {
            ValueInfo {
                value: node
                    .numeric_value()
                    .map(|number| number.to_string())
                    .or(text_value)
                    .unwrap_or_default(),
                min: node.min_numeric_value(),
                max: node.max_numeric_value(),
                step: node.numeric_value_step(),
            }
        });
    let mut actions = node_actions(node);
    push_action(&mut actions, raw.pointer.hover, NodeAction::Hover);
    push_action(&mut actions, raw.pointer.drag, NodeAction::Drag);
    push_action(&mut actions, raw.pointer.scroll, NodeAction::Scroll);
    if redacted {
        actions.retain(|action| !matches!(action, NodeAction::SetText | NodeAction::SetValue));
    }
    let has_area = raw
        .bounds
        .is_none_or(|bounds| bounds.width > 0.0 && bounds.height > 0.0);
    let mut metadata = BTreeMap::new();
    metadata.insert("accesskit_id".to_owned(), raw.id.0.to_string());
    UiNode {
        id: published.identity,
        parent: published.parent,
        children: Vec::new(),
        role: protocol_role(role),
        label,
        description: node.description().map(str::to_owned),
        bounds: raw.bounds,
        state: NodeState {
            visible: !published.hidden && has_area,
            enabled: !node.is_disabled(),
            focused: published.focused,
            checked: match node.toggled() {
                Some(Toggled::True) => Some(true),
                Some(Toggled::False) => Some(false),
                _ => None,
            },
            selected: node.is_selected(),
            expanded: node.is_expanded(),
        },
        actions,
        text,
        value,
        metadata,
    }
}

/// The identity a named element contributes, from the element id GPUI recorded.
fn element_id_name(id: &gpui::ElementId) -> Option<String> {
    match id {
        gpui::ElementId::Name(name) => Some(name.to_string()),
        gpui::ElementId::NamedInteger(name, index) => Some(format!("{name}-{index}")),
        gpui::ElementId::Integer(index) => Some(index.to_string()),
        _ => None,
    }
}

/// Map an AccessKit role onto the protocol's role vocabulary.
fn protocol_role(role: accesskit::Role) -> Role {
    use accesskit::Role as A;
    match role {
        A::Application => Role::Application,
        A::Window | A::RootWebArea => Role::Window,
        A::Button | A::DefaultButton | A::DisclosureTriangle => Role::Button,
        A::CheckBox => Role::Checkbox,
        A::RadioButton => Role::Radio,
        A::Switch => Role::Switch,
        A::Link => Role::Link,
        A::Label
        | A::TextRun
        | A::Paragraph
        | A::Heading
        | A::Legend
        | A::Caption
        | A::FigureCaption
        | A::Term
        | A::Code
        | A::Emphasis
        | A::Strong => Role::Text,
        A::TextInput
        | A::MultilineTextInput
        | A::EmailInput
        | A::PasswordInput
        | A::PhoneNumberInput
        | A::UrlInput => Role::TextInput,
        A::SearchInput | A::Search => Role::SearchInput,
        A::Slider | A::SpinButton => Role::Slider,
        A::ProgressIndicator | A::Meter => Role::Progress,
        A::Image | A::GraphicsSymbol => Role::Image,
        A::List | A::ListBox => Role::List,
        A::ListItem => Role::ListItem,
        A::Tree => Role::Tree,
        A::TreeItem => Role::TreeItem,
        A::Table | A::Grid | A::TreeGrid | A::ListGrid => Role::Table,
        A::Row | A::LayoutTableRow => Role::Row,
        A::Cell | A::GridCell | A::LayoutTableCell | A::RowHeader | A::ColumnHeader => Role::Cell,
        A::Menu | A::MenuBar | A::MenuListPopup => Role::Menu,
        A::MenuItem | A::MenuItemCheckBox | A::MenuItemRadio => Role::MenuItem,
        A::ComboBox | A::EditableComboBox => Role::Combobox,
        A::ListBoxOption | A::MenuListOption => Role::Option,
        A::Splitter => Role::Separator,
        A::Tooltip => Role::Tooltip,
        A::TabList => Role::TabList,
        A::Tab => Role::Tab,
        A::Toolbar => Role::Toolbar,
        A::Dialog | A::AlertDialog => Role::Dialog,
        A::Alert => Role::Alert,
        A::ScrollBar | A::ScrollView => Role::ScrollArea,
        A::Group | A::Pane | A::RadioGroup | A::TabPanel => Role::Group,
        _ => Role::Generic,
    }
}

/// Whether AccessKit marks this role's value as secret.
fn is_redacted(role: accesskit::Role) -> bool {
    role == accesskit::Role::PasswordInput
}

/// Whether the role's value is its text content.
fn is_text_role(role: accesskit::Role) -> bool {
    use accesskit::Role as A;
    is_editable_text_role(role)
        || matches!(
            role,
            A::Label
                | A::TextRun
                | A::Paragraph
                | A::Heading
                | A::Legend
                | A::Caption
                | A::FigureCaption
                | A::Term
                | A::Code
                | A::Emphasis
                | A::Strong
        )
}

/// Whether the role accepts text replacement through its value.
fn is_editable_text_role(role: accesskit::Role) -> bool {
    use accesskit::Role as A;
    matches!(
        role,
        A::TextInput
            | A::MultilineTextInput
            | A::SearchInput
            | A::EmailInput
            | A::PasswordInput
            | A::PhoneNumberInput
            | A::UrlInput
    )
}

/// Map a node's advertised AccessKit actions onto the bridge's vocabulary.
///
/// Dispatch is native: the platform adapter delivers action requests to the
/// app's [`Window::on_a11y_action`] listeners and otherwise falls back to
/// GPUI's built-in handling (Click clicks the node's bounds, Focus focuses it).
/// The bridge registers no listeners of its own because a matching listener
/// suppresses that fallback.
fn node_actions(node: &accesskit::Node) -> Vec<NodeAction> {
    let supports = |action| node.supports_action(action);
    let mut actions = Vec::new();
    push_action(&mut actions, supports(Action::Click), NodeAction::Click);
    push_action(&mut actions, supports(Action::Focus), NodeAction::Focus);
    push_action(
        &mut actions,
        supports(Action::ReplaceSelectedText)
            || (is_editable_text_role(node.role()) && supports(Action::SetValue)),
        NodeAction::SetText,
    );
    push_action(
        &mut actions,
        supports(Action::SetValue),
        NodeAction::SetValue,
    );
    push_action(
        &mut actions,
        SCROLL_ACTIONS.into_iter().any(supports),
        NodeAction::Scroll,
    );
    actions
}

fn push_action(actions: &mut Vec<NodeAction>, condition: bool, action: NodeAction) {
    if condition && !actions.contains(&action) {
        actions.push(action);
    }
}

const SCROLL_ACTIONS: [Action; 7] = [
    Action::ScrollDown,
    Action::ScrollLeft,
    Action::ScrollRight,
    Action::ScrollUp,
    Action::ScrollIntoView,
    Action::ScrollToPoint,
    Action::SetScrollOffset,
];

/// Derive a label from descendant text for roles that have no label of their own.
fn label_from_content(role: accesskit::Role, content: &str) -> Option<String> {
    use accesskit::Role as A;
    let labelled = matches!(
        role,
        A::Button
            | A::DefaultButton
            | A::CheckBox
            | A::RadioButton
            | A::Switch
            | A::Link
            | A::MenuItem
            | A::MenuItemCheckBox
            | A::MenuItemRadio
            | A::ListBoxOption
            | A::MenuListOption
            | A::Tab
    );
    (labelled && !content.is_empty()).then(|| content.to_owned())
}

/// Parse an eight-digit `#RRGGBBAA` highlight color.
fn parse_color(color: &str) -> Option<u32> {
    let value = color.strip_prefix('#')?;
    (value.len() == 8)
        .then(|| u32::from_str_radix(value, 16).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use gpui::accesskit::{self, Action, NodeId, Toggled, TreeUpdate};
    use gpui::{
        A11yFrame, A11yFrameObserver, AppContext as _, Context, Entity, FocusHandle,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, Role, SharedString,
        StatefulInteractiveElement as _, Styled as _, TestAppContext, Text, Window, div, px,
    };
    use gpui_mcp_protocol::{
        Highlight, NodeAction, Rect, Role as McpRole, TextInfo, UiNode, ValueInfo,
    };

    use super::{NodeLookup, frame_nodes};
    use crate::Automation;
    use crate::registry::{SharedState, TreeUpdate as Published};

    fn ak(role: accesskit::Role, children: &[u64]) -> accesskit::Node {
        let mut node = accesskit::Node::new(role);
        if !children.is_empty() {
            node.set_children(
                children
                    .iter()
                    .map(|&child| NodeId(child))
                    .collect::<Vec<_>>(),
            );
        }
        node
    }

    /// A finalized tree rooted at node 0, with element ids by AccessKit id.
    struct Fixture {
        update: TreeUpdate,
        names: HashMap<NodeId, &'static str>,
    }

    impl Fixture {
        fn new(nodes: Vec<(u64, Option<&'static str>, accesskit::Node)>) -> Self {
            let names = nodes
                .iter()
                .filter_map(|(id, name, _)| name.map(|name| (NodeId(*id), name)))
                .collect();
            let update = TreeUpdate {
                nodes: nodes
                    .into_iter()
                    .map(|(id, _, node)| (NodeId(id), node))
                    .collect(),
                tree: Some(accesskit::Tree::new(NodeId(0))),
                tree_id: accesskit::TreeId::ROOT,
                focus: NodeId(0),
            };
            Self { update, names }
        }

        fn translate(
            &self,
            focus: Option<u64>,
            bounds: impl Fn(u64) -> Option<Rect>,
        ) -> HashMap<String, UiNode> {
            frame_nodes(&self.update, focus.map(NodeId), |id| NodeLookup {
                bounds: bounds(id.0),
                element_id: self.names.get(&id).map(|name| (*name).to_owned()),
                ..NodeLookup::default()
            })
            .into_iter()
            .map(|node| (node.id.clone(), node))
            .collect()
        }
    }

    fn semantics_fixture() -> Fixture {
        let mut window = ak(accesskit::Role::Window, &[1]);
        window.set_label("Demo");
        let mut save = ak(accesskit::Role::Button, &[4]);
        for action in [
            Action::Click,
            Action::Focus,
            Action::ReplaceSelectedText,
            Action::SetValue,
            Action::ScrollDown,
        ] {
            save.add_action(action);
        }
        let mut volume = ak(accesskit::Role::Slider, &[]);
        volume.set_label("Volume");
        volume.set_numeric_value(3.0);
        volume.set_min_numeric_value(0.0);
        volume.set_max_numeric_value(11.0);
        volume.set_numeric_value_step(1.0);
        volume.add_action(Action::SetValue);
        let mut label = ak(accesskit::Role::Label, &[]);
        label.set_value("Save");
        let mut toggled = ak(accesskit::Role::Group, &[]);
        toggled.set_selected(true);
        toggled.set_expanded(false);
        toggled.set_toggled(Toggled::True);
        Fixture::new(vec![
            (0, None, window),
            (
                1,
                Some("root"),
                ak(accesskit::Role::Application, &[2, 3, 5, 6, 7, 8, 10]),
            ),
            (2, Some("save"), save),
            (3, Some("volume"), volume),
            (4, Some("save-label"), label),
            (5, Some("panel"), ak(accesskit::Role::Group, &[])),
            (6, Some("panel"), toggled),
            (7, Some("q/y"), ak(accesskit::Role::Group, &[])),
            (8, Some("q"), ak(accesskit::Role::Group, &[9])),
            (9, Some("y"), ak(accesskit::Role::Group, &[])),
            (10, Some("y"), ak(accesskit::Role::Group, &[])),
        ])
    }

    #[test]
    fn translates_roles_labels_values_states_and_parentage() {
        let save_bounds = Rect {
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 40.0,
        };
        let tree = semantics_fixture().translate(Some(3), |id| (id == 2).then_some(save_bounds));

        assert_eq!(tree.len(), 10, "the window host node is not published");
        assert_eq!(tree["root"].role, McpRole::Application);
        assert_eq!(tree["root"].parent, None);
        assert!(
            tree["root"].bounds.is_none(),
            "a node without a bounds lookup stays empty"
        );
        assert_eq!(tree["save"].bounds, Some(save_bounds));
        assert_eq!(
            tree["save"]
                .metadata
                .get("accesskit_id")
                .map(String::as_str),
            Some("2"),
            "the published node carries the id focus resolution needs"
        );

        assert_eq!(tree["save"].parent.as_deref(), Some("root"));
        assert_eq!(tree["save"].role, McpRole::Button);
        assert_eq!(
            tree["save"].label.as_deref(),
            Some("Save"),
            "a button without its own label takes descendant text"
        );
        assert!(!tree["save"].state.focused);
        assert!(tree["save"].state.visible && tree["save"].state.enabled);
        assert_eq!(
            tree["save"].actions,
            [
                NodeAction::Click,
                NodeAction::Focus,
                NodeAction::SetText,
                NodeAction::SetValue,
                NodeAction::Scroll,
            ],
            "advertised AccessKit actions map onto the bridge vocabulary"
        );

        assert_eq!(tree["save-label"].parent.as_deref(), Some("save"));
        assert_eq!(tree["save-label"].role, McpRole::Text);
        assert_eq!(
            tree["save-label"].text,
            Some(TextInfo {
                text: "Save".to_owned(),
                caret: None,
                selection: None,
                redacted: false,
            })
        );

        assert_eq!(tree["volume"].role, McpRole::Slider);
        assert_eq!(tree["volume"].label.as_deref(), Some("Volume"));
        assert_eq!(
            tree["volume"].value,
            Some(ValueInfo {
                value: "3".to_owned(),
                min: Some(0.0),
                max: Some(11.0),
                step: Some(1.0),
            })
        );
        assert!(tree["volume"].state.focused, "GPUI's focus names the node");
        assert_eq!(tree["volume"].actions, [NodeAction::SetValue]);
    }

    #[test]
    fn colliding_element_ids_get_frame_unique_identities() {
        let tree = semantics_fixture().translate(None, |_| None);

        assert_eq!(tree["root/panel#5"].role, McpRole::Group);
        assert_eq!(tree["root/panel#5"].state.selected, None);
        assert_eq!(tree["root/panel#5"].state.checked, None);
        assert_eq!(tree["root/panel#6"].parent.as_deref(), Some("root"));
        assert_eq!(tree["root/panel#6"].state.selected, Some(true));
        assert_eq!(tree["root/panel#6"].state.checked, Some(true));
        assert_eq!(tree["root/panel#6"].state.expanded, Some(false));

        assert_eq!(
            tree["q/y"].role,
            McpRole::Group,
            "an unambiguous id containing the separator keeps its own id"
        );
        assert_eq!(
            tree["root/q/y"].role,
            McpRole::Group,
            "a qualified identity never spells one already given out"
        );
        assert_eq!(tree["root/y"].role, McpRole::Group);
    }

    #[test]
    fn hidden_inherits_disabled_reads_and_password_input_redacts() {
        let mut drawer = ak(accesskit::Role::Group, &[3]);
        drawer.set_hidden();
        let mut close = ak(accesskit::Role::Button, &[]);
        close.add_action(Action::Click);
        let mut locked = ak(accesskit::Role::Button, &[]);
        locked.set_disabled();
        let mut pin = ak(accesskit::Role::PasswordInput, &[]);
        pin.set_value("1234");
        for action in [Action::Focus, Action::SetValue, Action::ReplaceSelectedText] {
            pin.add_action(action);
        }
        let mut reveal_pin = ak(accesskit::Role::PasswordInput, &[]);
        reveal_pin.set_value("1234");
        let tree = Fixture::new(vec![
            (0, None, ak(accesskit::Role::Window, &[1])),
            (
                1,
                Some("root"),
                ak(accesskit::Role::Application, &[2, 4, 5, 6]),
            ),
            (2, Some("drawer"), drawer),
            (3, Some("drawer-close"), close),
            (4, Some("locked"), locked),
            (5, Some("pin"), pin),
            (6, Some("reveal"), ak(accesskit::Role::Button, &[7])),
            (7, Some("reveal-pin"), reveal_pin),
        ])
        .translate(None, |_| None);

        assert!(!tree["drawer"].state.visible);
        assert!(
            !tree["drawer-close"].state.visible,
            "a hidden container hides its descendants"
        );
        assert!(tree["locked"].state.visible);
        assert!(!tree["locked"].state.enabled);
        assert!(tree["root"].state.enabled);

        let pin = &tree["pin"];
        assert_eq!(pin.role, McpRole::TextInput);
        assert_eq!(
            pin.text,
            Some(TextInfo {
                text: String::new(),
                caret: None,
                selection: None,
                redacted: true,
            })
        );
        assert_eq!(pin.value, None);
        assert_eq!(pin.actions, [NodeAction::Focus]);
        assert_eq!(
            tree["reveal"].label, None,
            "secret text never becomes an ancestor's label"
        );
    }

    struct StateFixture;

    impl Render for StateFixture {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("root")
                .role(Role::Application)
                .size_full()
                .child(
                    div()
                        .id("drawer")
                        .role(Role::Group)
                        .aria_hidden(true)
                        .w(px(120.0))
                        .h(px(40.0))
                        .child(
                            div()
                                .id("drawer-close")
                                .role(Role::Button)
                                .w(px(100.0))
                                .h(px(30.0)),
                        ),
                )
                .child(
                    div()
                        .id("locked")
                        .role(Role::Button)
                        .aria_disabled(true)
                        .w(px(100.0))
                        .h(px(30.0)),
                )
                .child(
                    div()
                        .id("pin")
                        .role(Role::PasswordInput)
                        .aria_value("1234")
                        .w(px(100.0))
                        .h(px(24.0)),
                )
                .child(
                    div()
                        .id("roleless-click")
                        .on_click(|_, _, _| {})
                        .w(px(80.0))
                        .h(px(24.0)),
                )
                .child(
                    div()
                        .id("hover-card")
                        .role(Role::Group)
                        .hover(|style| style.opacity(0.5))
                        .w(px(80.0))
                        .h(px(24.0)),
                )
                .child(
                    div()
                        .id("drag-handle")
                        .role(Role::Button)
                        .on_drag(DragPayload, |_, _, _, cx| cx.new(|_| DragPreview))
                        .w(px(80.0))
                        .h(px(24.0)),
                )
                .child(
                    div()
                        .id("scroller")
                        .role(Role::ScrollView)
                        .overflow_y_scroll()
                        .w(px(80.0))
                        .h(px(24.0)),
                )
        }
    }

    struct DragPayload;
    struct DragPreview;

    impl Render for DragPreview {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn observes_hidden_disabled_and_redacted_state(cx: &mut TestAppContext) {
        let automation = Automation::isolated();
        let automation_for_window = automation.clone();
        let (_view, visual) = cx.add_window_view(move |window, _| {
            automation_for_window.attach(window);
            StateFixture
        });
        visual.run_until_parked();

        let tree = automation.snapshot();
        assert!(tree.diagnostics.is_empty());
        assert!(!tree.nodes["drawer"].state.visible, "aria_hidden (C08)");
        assert!(!tree.nodes["drawer-close"].state.visible);
        assert!(
            tree.nodes["drawer-close"]
                .bounds
                .is_some_and(|bounds| bounds.width > 0.0),
            "a hidden node keeps its layout bounds"
        );
        assert!(!tree.nodes["locked"].state.enabled, "aria_disabled (C08)");
        assert!(tree.nodes["locked"].state.visible);
        let pin_text = tree.nodes["pin"].text.clone().unwrap_or_default();
        assert!(pin_text.redacted && pin_text.text.is_empty());
        assert!(tree.nodes["pin"].value.is_none());
        let roleless = tree.nodes.get("roleless-click");
        assert_eq!(
            roleless.map(|node| node.role),
            Some(McpRole::Button),
            "a clickable div without a role is a button (C11)"
        );
        assert!(roleless.is_some_and(|node| node.actions.contains(&NodeAction::Click)));

        let actions = |id: &str| tree.nodes.get(id).map(|node| node.actions.clone());
        assert_eq!(actions("hover-card"), Some(vec![NodeAction::Hover]), "C12");
        assert_eq!(actions("drag-handle"), Some(vec![NodeAction::Drag]), "C12");
        assert_eq!(actions("scroller"), Some(vec![NodeAction::Scroll]), "C12");
        assert_eq!(
            actions("locked"),
            Some(Vec::new()),
            "no listener, no inferred action"
        );
    }

    struct Heavy {
        rows: usize,
    }

    impl Render for Heavy {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("root")
                .role(Role::Application)
                .size_full()
                .children((0..self.rows).map(|row| {
                    div()
                        .id(("row", row))
                        .role(Role::ListItem)
                        .hover(|style| style.opacity(0.9))
                        .on_click(|_, _, _| {})
                        .h(px(4.0))
                        .child(Text::new(
                            ("label", row).into(),
                            SharedString::from(format!("Row {row}")),
                        ))
                }))
        }
    }

    /// Keeps a copy of the last tree a frame carried.
    #[derive(Default)]
    struct Capture(Mutex<Option<TreeUpdate>>);

    impl A11yFrameObserver for Capture {
        fn frame_finished(&self, _window: &Window, frame: &A11yFrame<'_>) {
            if let Some(tree) = frame.tree {
                *self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tree.clone());
            }
        }
    }

    /// Per-stage cost of observing a ~5,000-node frame. Run with
    /// `cargo test --release -p gpui-mcp --lib observation_stage_costs -- --ignored --nocapture`.
    #[gpui::test]
    #[ignore = "benchmark; prints timings rather than asserting"]
    fn observation_stage_costs(cx: &mut TestAppContext) {
        use std::time::Instant;
        type Stage<'a> = (&'static str, Box<dyn FnMut() + 'a>);
        let automation = Automation::isolated();
        let for_window = automation.clone();
        let capture = Arc::new(Capture::default());
        let capture_for_window = capture.clone();
        let (_view, visual) = cx.add_window_view(move |window, _| {
            for_window.attach(window);
            window.add_a11y_frame_observer(capture_for_window);
            Heavy { rows: 2_500 }
        });
        visual.run_until_parked();
        let Some(update) = capture
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            unreachable!("the frame carried a tree");
        };
        let translated = frame_nodes(&update, None, |_| NodeLookup::default());
        let mut changed = translated.clone();
        if let Some(node) = changed.first_mut() {
            node.label = Some("changed".to_owned());
        }
        let state = SharedState::new();
        state.publish_frame(translated.clone());
        let tree = automation.snapshot();
        let bytes = serde_json::to_vec(&tree).unwrap_or_default();
        let mut flip = false;
        let mut stages: Vec<Stage<'_>> = vec![
            (
                "unchanged redraw",
                Box::new(|| visual.update(|window, _| window.refresh())),
            ),
            (
                "frame_nodes",
                Box::new(|| {
                    let _ = frame_nodes(&update, None, |_| NodeLookup::default());
                }),
            ),
            (
                "publish one change",
                Box::new(|| {
                    flip = !flip;
                    state.publish_frame(if flip {
                        changed.clone()
                    } else {
                        translated.clone()
                    });
                }),
            ),
            (
                "one-step delta",
                Box::new(|| {
                    let _ = state.tree_since(state.tree_generation() - 1);
                }),
            ),
            (
                "tree to_vec",
                Box::new(|| {
                    let _ = serde_json::to_vec(&tree);
                }),
            ),
            (
                "tree from_slice",
                Box::new(|| {
                    let _ = serde_json::from_slice::<gpui_mcp_protocol::UiTree>(&bytes);
                }),
            ),
        ];
        eprintln!("nodes={} tree_bytes={}", tree.nodes.len(), bytes.len());
        for (label, stage) in &mut stages {
            let start = Instant::now();
            for _ in 0..10 {
                stage();
            }
            eprintln!(
                "{label:<22} {:>8.2} ms",
                start.elapsed().as_secs_f64() * 100.0
            );
        }
    }

    struct Toggle {
        on: bool,
    }

    impl Render for Toggle {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().id("root").role(Role::Application).size_full().child(
                div()
                    .id("switch")
                    .role(Role::Button)
                    .aria_disabled(!self.on)
                    .w(px(80.0))
                    .h(px(24.0)),
            )
        }
    }

    fn toggle(view: &Entity<Toggle>, visual: &mut gpui::VisualTestContext) {
        visual.update(|_, cx| {
            view.update(cx, |toggle, cx| {
                toggle.on = !toggle.on;
                cx.notify();
            });
        });
        visual.run_until_parked();
    }

    #[gpui::test]
    fn app_driven_changes_reach_the_tree_as_a_delta(cx: &mut TestAppContext) {
        let automation = Automation::isolated();
        let automation_for_window = automation.clone();
        let (view, visual) = cx.add_window_view(move |window, _| {
            automation_for_window.attach(window);
            Toggle { on: false }
        });
        visual.run_until_parked();
        let before = automation.snapshot();
        assert!(!before.nodes["switch"].state.enabled);

        // The app changes its own state; no bridge operation asks for a frame.
        toggle(&view, visual);
        let after = automation.snapshot();
        assert!(
            after.nodes["switch"].state.enabled,
            "a frame the app drew on its own is observed"
        );

        let update = automation.state.tree_since(before.generation);
        let Published::Delta(delta) = update else {
            unreachable!("a retained generation answers with a delta, got {update:?}");
        };
        assert_eq!(delta.upserted.len(), 1, "only the switch changed");
        assert!(delta.removed.is_empty() && delta.roots.is_none());
        let mut patched = before;
        assert!(delta.apply(&mut patched));
        assert_eq!(patched, after, "the delta rebuilds the full tree");

        let generation = after.generation;
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        assert_eq!(
            automation.semantic_generation(),
            generation,
            "an unchanged frame does not publish"
        );
    }

    #[gpui::test]
    fn on_demand_observation_builds_the_tree_only_while_requested(cx: &mut TestAppContext) {
        let automation = Automation::new(SharedState::new(), true);
        let automation_for_window = automation.clone();
        let (view, visual) = cx.add_window_view(move |window, _| {
            automation_for_window.attach(window);
            Toggle { on: false }
        });
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        assert!(
            automation.snapshot().nodes.is_empty(),
            "no tree is built before a client asks"
        );
        let frames = automation.state.frame_stats().frame_count;
        assert!(frames > 0, "frames are counted while unobserved");

        visual.update(|window, _| automation.set_observed(window, true));
        visual.run_until_parked();
        assert!(!automation.snapshot().nodes["switch"].state.enabled);

        visual.update(|window, _| automation.set_observed(window, false));
        toggle(&view, visual);
        assert!(
            !automation.snapshot().nodes["switch"].state.enabled,
            "an unobserved frame does not publish"
        );
        assert!(automation.state.frame_stats().frame_count > frames);

        visual.update(|window, _| automation.set_observed(window, true));
        visual.run_until_parked();
        assert!(
            automation.snapshot().nodes["switch"].state.enabled,
            "resuming republishes the current frame"
        );
    }

    #[gpui::test]
    fn frames_report_measured_phases_and_paint_highlights(cx: &mut TestAppContext) {
        let automation = Automation::isolated();
        let automation_for_window = automation.clone();
        let (_view, visual) = cx.add_window_view(move |window, _| {
            automation_for_window.attach(window);
            Toggle { on: true }
        });
        visual.run_until_parked();
        automation.state.set_highlights(vec![Highlight {
            rect: Rect {
                x: 1.0,
                y: 2.0,
                width: 30.0,
                height: 40.0,
            },
            color: "#ff0000ff".to_owned(),
            label: None,
        }]);
        for _ in 0..3 {
            visual.update(|window, _| window.refresh());
            visual.run_until_parked();
        }
        let stats = automation.state.frame_stats();
        assert!(stats.frame_count >= 3);
        assert!(stats.sample_count >= 2);
        assert!(
            stats.prepaint_max_ms > 0.0,
            "prepaint is measured: {stats:?}"
        );
        assert!(
            stats.root_paint_max_ms > 0.0,
            "paint is measured: {stats:?}"
        );
    }

    struct SemanticFixture {
        focus: FocusHandle,
    }

    impl Render for SemanticFixture {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("root")
                .role(Role::Application)
                .size_full()
                .child(
                    div()
                        .id("save")
                        .role(Role::Button)
                        .w(px(120.0))
                        .h(px(40.0))
                        // gpui-pre only publishes a Label node for `Text` with
                        // an id, so the button's content-derived label needs one.
                        .child(Text::new("save-text".into(), "Save".into())),
                )
                .child(
                    div()
                        .id("field")
                        .role(Role::TextInput)
                        .track_focus(&self.focus)
                        .w(px(120.0))
                        .h(px(24.0)),
                )
                .child(div().id("status").child("Ready"))
        }
    }

    #[gpui::test]
    fn observes_roles_labels_and_parentage(cx: &mut TestAppContext) {
        let automation = Automation::isolated();
        let automation_for_window = automation.clone();
        let focus = cx.update(|cx| cx.focus_handle());
        let focus_for_window = focus.clone();
        let (_view, visual) = cx.add_window_view(move |window, _| {
            automation_for_window.attach(window);
            SemanticFixture {
                focus: focus_for_window,
            }
        });
        visual.run_until_parked();

        let tree = automation.snapshot();
        assert!(tree.diagnostics.is_empty());
        assert_eq!(tree.roots, ["root"]);
        assert_eq!(tree.nodes["root"].role, McpRole::Application);
        assert_eq!(tree.nodes["save"].parent.as_deref(), Some("root"));
        assert_eq!(tree.nodes["save"].role, McpRole::Button);
        assert_eq!(tree.nodes["save"].label.as_deref(), Some("Save"));
        assert!(
            tree.nodes["save"].bounds.is_some(),
            "bounds arrive from the window's accessibility map (C03)"
        );
        assert!(!tree.nodes["save"].state.focused);
        assert!(
            !tree.nodes.contains_key("status"),
            "elements without an explicit role are not in GPUI's accessibility tree"
        );

        // C04: the published accesskit id resolves the node's focus handle, and
        // focusing it moves focus without a synthetic key event.
        let accesskit_id = tree.nodes["field"]
            .metadata
            .get("accesskit_id")
            .and_then(|id| id.parse::<u64>().ok());
        assert!(
            accesskit_id.is_some(),
            "the field publishes its accesskit id"
        );
        let accesskit_id = accesskit_id.unwrap_or_default();
        let resolved =
            visual.update(|window, cx| window.a11y_focus_handle(NodeId(accesskit_id), cx));
        assert_eq!(resolved.as_ref(), Some(&focus));
        visual.update(|window, cx| {
            let Some(handle) = window.a11y_focus_handle(NodeId(accesskit_id), cx) else {
                return;
            };
            window.focus(&handle, cx);
        });
        visual.run_until_parked();
        assert_eq!(
            visual.update(|window, cx| window.focused(cx)).as_ref(),
            Some(&focus)
        );
        assert!(
            automation.snapshot().nodes["field"].state.focused,
            "a focus change alone publishes a new frame"
        );
    }

    struct DockPanel {
        title: SharedString,
    }

    impl Render for DockPanel {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("tab-panel")
                .role(Role::Group)
                .size_full()
                .child(Text::new("panel-title".into(), self.title.clone()))
        }
    }

    struct DockFixture {
        panels: Vec<Entity<DockPanel>>,
    }

    impl Render for DockFixture {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("dock-area")
                .role(Role::Group)
                .size_full()
                .children(self.panels.iter().cloned())
        }
    }

    #[gpui::test]
    fn repeated_element_ids_stay_frame_unique_and_grouped_by_parent(cx: &mut TestAppContext) {
        let automation = Automation::isolated();
        let automation_for_window = automation.clone();
        let (_view, visual) = cx.add_window_view(move |window, cx| {
            automation_for_window.attach(window);
            DockFixture {
                panels: vec![
                    cx.new(|_| DockPanel {
                        title: "Hierarchy".into(),
                    }),
                    cx.new(|_| DockPanel {
                        title: "Console".into(),
                    }),
                ],
            }
        });
        visual.run_until_parked();

        let tree = automation.snapshot();
        assert!(tree.diagnostics.is_empty());
        let dock = &tree.nodes["dock-area"];
        assert_eq!(dock.children.len(), 2);
        let mut panel_ids = dock.children.clone();
        panel_ids.sort();
        assert_ne!(panel_ids[0], panel_ids[1]);

        let mut titles = Vec::new();
        for id in &panel_ids {
            let panel = &tree.nodes[id];
            assert_eq!(panel.parent.as_deref(), Some("dock-area"));
            assert!(
                id.contains("tab-panel"),
                "an ambiguous element id keeps its id and gains a frame-unique suffix: {id}"
            );
            assert_eq!(panel.children.len(), 1);
            let text = tree.nodes[&panel.children[0]]
                .text
                .clone()
                .unwrap_or_default();
            titles.push(text.text);
        }
        titles.sort();
        assert_eq!(titles, ["Console", "Hierarchy"]);
    }
}
