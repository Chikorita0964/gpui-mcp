//! Read-only observation of GPUI's accessibility tree.
//!
//! The bridge publishes the semantic tree of the window's last completed frame
//! from [`Window::debug_a11y_tree_json`]. GPUI owns the semantics; this module
//! only names nodes, restores parentage, and translates the JSON shape into the
//! protocol's [`UiNode`].
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
//! - Hover and Drag have no AccessKit action, so only `Click`, `Focus`,
//!   `SetText`, `SetValue`, and `Scroll` can be reported.
//! - The overlay paint pass has no stock equivalent, so highlight overlays are
//!   not drawn.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Weak};

use gpui::accesskit::NodeId;
use gpui::{A11yPointerInteractions, Window};
use gpui_mcp_protocol::{NodeAction, NodeState, Rect, Role, TextInfo, UiNode, ValueInfo};
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};

use crate::registry::{SharedState, rect_from_gpui};

/// Publishes completed accessibility frames to the bridge's shared state.
///
/// The window retains nothing; the bridge owns the observer and decides when to
/// read the tree, because GPUI exposes it only on demand through
/// [`Window::debug_a11y_tree_json`].
pub(crate) struct BridgeObserver {
    state: Weak<SharedState>,
}

impl BridgeObserver {
    /// Create an observer that publishes into `state`.
    pub(crate) fn new(state: &Arc<SharedState>) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::downgrade(state),
        })
    }

    /// Publish the accessibility tree of the window's last completed frame.
    ///
    /// Returns `false` while no tree is available, which happens until a frame
    /// completes with accessibility active.
    fn observe_once(&self, window: &Window) -> bool {
        let Some(state) = self.state.upgrade() else {
            return false;
        };
        let Some(json) = window.debug_a11y_tree_json() else {
            return false;
        };
        let Some(nodes) = parse_frame(&json, |accesskit_id| NodeLookup {
            bounds: window
                .a11y_node_bounds(NodeId(accesskit_id))
                .map(rect_from_gpui),
            pointer: window
                .a11y_pointer_interactions(NodeId(accesskit_id))
                .unwrap_or_default(),
            element_id: window
                .a11y_element_id(NodeId(accesskit_id))
                .and_then(|id| element_id_name(&id)),
        }) else {
            state.add_log("warn", "the accessibility tree could not be parsed");
            return false;
        };
        let mut content_bounds = window.bounds();
        content_bounds.size = window.viewport_size();
        state.begin_frame();
        state.set_window_geometry(rect_from_gpui(content_bounds), window.scale_factor());
        state.publish_frame(nodes);
        // Frame timings are not measurable from a pulled tree; advance the
        // completed-frame watch so `WaitForFrame` keeps answering.
        state.begin_root_paint();
        state.finish_root_paint();
        true
    }

    /// Publish the accessibility tree of the next completed frame.
    ///
    /// Arm this after requesting a refresh so the published tree reflects it.
    /// The tree only exists once a frame has completed with accessibility
    /// active, so the callback re-arms for a few frames before giving up; a
    /// force-disabled application therefore cannot spin.
    pub(crate) fn observe_on_next_frame(self: &Arc<Self>, window: &mut Window) {
        self.observe_within_frames(window, OBSERVE_FRAME_ATTEMPTS);
    }

    fn observe_within_frames(self: &Arc<Self>, window: &mut Window, attempts: u8) {
        let observer = Arc::clone(self);
        window.on_next_frame(move |window, _cx| {
            if observer.observe_once(window) || attempts == 0 {
                return;
            }
            observer.observe_within_frames(window, attempts - 1);
        });
    }
}

/// Frames to wait for the first accessibility tree after attaching or
/// refreshing, so observation does not give up before a frame completes.
const OBSERVE_FRAME_ATTEMPTS: u8 = 4;

/// The fields of one `debug_a11y_tree_json()` document the observer reads.
#[derive(Deserialize)]
struct Frame {
    root: Option<String>,
    gpui_focus: Option<String>,
    #[serde(deserialize_with = "node_entries")]
    nodes: Vec<(String, RawNode)>,
}

/// One node of `debug_a11y_tree_json()` before it is named.
#[derive(Deserialize)]
struct RawNode {
    #[serde(default)]
    accesskit_id: String,
    #[serde(default)]
    children: Vec<String>,
    #[serde(default, deserialize_with = "element_id")]
    element_id: Option<String>,
    #[serde(default)]
    aria: Aria,
}

impl RawNode {
    /// The node's own identifier: its element id, or its AccessKit node id.
    fn own_id(&self) -> &str {
        self.element_id.as_deref().unwrap_or(&self.accesskit_id)
    }
}

/// Accessibility semantics carried by one JSON node.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Aria {
    role: String,
    label: Option<String>,
    description: Option<String>,
    value: Option<String>,
    numeric_value: Option<f64>,
    #[serde(rename = "min_numeric_value")]
    min: Option<f64>,
    #[serde(rename = "max_numeric_value")]
    max: Option<f64>,
    #[serde(rename = "numeric_value_step")]
    step: Option<f64>,
    selected: Option<bool>,
    expanded: Option<bool>,
    toggled: Option<String>,
    hidden: bool,
    disabled: bool,
    on_action: Vec<String>,
}

impl Aria {
    /// Whether AccessKit marks this node's value as secret.
    fn is_redacted(&self) -> bool {
        self.role == "PasswordInput"
    }
}

/// Translate one `debug_a11y_tree_json()` document into protocol nodes.
///
/// The document's host node is not published: it is GPUI's window node, it
/// carries no element identity, and its children are the application's roots.
/// `lookup` resolves each node's AccessKit id to what the window holds beside
/// the dump: logical bounds and pointer interactions. Each published node also
/// carries that id as `metadata["accesskit_id"]` so focus requests can resolve
/// a handle. Returns `None` when the document does not carry a node map.
fn parse_frame(json: &str, lookup: impl Fn(u64) -> NodeLookup) -> Option<Vec<UiNode>> {
    let frame: Frame = serde_json::from_str(json).ok()?;
    let mut entries = frame.nodes;
    let mut window_facts: Vec<NodeLookup> = Vec::with_capacity(entries.len());
    for (_, node) in &mut entries {
        let mut facts = node
            .accesskit_id
            .parse::<u64>()
            .ok()
            .map(&lookup)
            .unwrap_or_default();
        // The window's record works in every build; the dump's field only in debug.
        if let Some(element_id) = facts.element_id.take() {
            node.element_id = Some(element_id);
        }
        window_facts.push(facts);
    }
    let tree = FrameTree::new(entries, frame.root.as_deref());
    let focus = frame
        .gpui_focus
        .as_deref()
        .and_then(|key| tree.index.get(key).copied());

    let order = tree.depth_first_order();
    let identities = tree.assign_identities(&order);
    let content = tree.content_texts(&order);

    // `order` is depth-first, so a parent's hidden state is known before its children.
    let mut hidden = vec![false; tree.nodes.len()];
    let mut nodes = Vec::with_capacity(order.len());
    for &index in &order {
        let node = &tree.nodes[index];
        let parent = tree.parent[index];
        hidden[index] = node.aria.hidden || parent.is_some_and(|parent| hidden[parent]);
        let facts = &window_facts[index];
        nodes.push(to_ui_node(
            node,
            PublishedNode {
                identity: identities[index].clone(),
                // The host is never published, so a child of it is a root.
                parent: parent
                    .filter(|&parent| tree.host != Some(parent))
                    .map(|parent| identities[parent].clone()),
                content: content[index].as_deref().unwrap_or_default(),
                bounds: facts.bounds,
                pointer: facts.pointer,
                hidden: hidden[index],
                focused: focus == Some(index),
            },
        ));
    }
    Some(nodes)
}

/// The dump's nodes, indexed so every relationship is a `usize`, not a key lookup.
struct FrameTree {
    nodes: Vec<RawNode>,
    keys: Vec<String>,
    index: HashMap<String, usize>,
    children: Vec<Vec<usize>>,
    parent: Vec<Option<usize>>,
    root: Option<usize>,
    host: Option<usize>,
}

impl FrameTree {
    fn new(entries: Vec<(String, RawNode)>, root_key: Option<&str>) -> Self {
        let mut nodes = Vec::with_capacity(entries.len());
        let mut keys = Vec::with_capacity(entries.len());
        for (key, mut node) in entries {
            if node.accesskit_id.is_empty() {
                node.accesskit_id.clone_from(&key);
            }
            nodes.push(node);
            keys.push(key);
        }
        let index: HashMap<String, usize> = keys
            .iter()
            .enumerate()
            .map(|(position, key)| (key.clone(), position))
            .collect();
        let children: Vec<Vec<usize>> = nodes
            .iter()
            .map(|node| {
                node.children
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
        let root = root_key.and_then(|key| index.get(key).copied());
        let host = root.filter(|&root| nodes[root].element_id.is_none());
        Self {
            nodes,
            keys,
            index,
            children,
            parent,
            root,
            host,
        }
    }

    /// Every node reachable from the root's children, depth first in listed
    /// order, then any unreachable node in key order. The host is skipped.
    fn depth_first_order(&self) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut visited = vec![false; self.nodes.len()];
        if let Some(root) = self.root {
            self.collect_order(&self.children[root], &mut visited, &mut order);
        }
        let mut leftovers: Vec<usize> = (0..self.nodes.len())
            .filter(|&index| !visited[index])
            .collect();
        leftovers.sort_by(|left, right| self.keys[*left].cmp(&self.keys[*right]));
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
            path.push(self.nodes[node].own_id());
            current = self.parent[node];
        }
        path.reverse();
        path
    }
}

/// Read the dump's `nodes` object as its entries, in document order.
fn node_entries<'de, D>(deserializer: D) -> Result<Vec<(String, RawNode)>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Entries;

    impl<'de> Visitor<'de> for Entries {
        type Value = Vec<(String, RawNode)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of accessibility nodes")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some(entry) = map.next_entry()? {
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Entries)
}

/// Decode GPUI's `Debug` rendering of the node's leaf element id.
fn element_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
    Ok(raw.as_deref().and_then(decode_element_id))
}

/// What the window holds about one node beside the tree dump.
#[derive(Default)]
struct NodeLookup {
    bounds: Option<Rect>,
    pointer: A11yPointerInteractions,
    element_id: Option<String>,
}

/// What `parse_frame` resolves about a node from the whole tree.
struct PublishedNode<'a> {
    identity: String,
    parent: Option<String>,
    content: &'a str,
    bounds: Option<Rect>,
    pointer: A11yPointerInteractions,
    hidden: bool,
    focused: bool,
}

/// Build one protocol node from its own semantics and its tree context.
fn to_ui_node(node: &RawNode, published: PublishedNode<'_>) -> UiNode {
    let role_name = node.aria.role.as_str();
    let redacted = node.aria.is_redacted();
    let label = node
        .aria
        .label
        .clone()
        .or_else(|| label_from_content(role_name, published.content));
    let text_value = if redacted {
        None
    } else {
        node.aria
            .value
            .clone()
            .or_else(|| (!published.content.is_empty()).then(|| published.content.to_owned()))
    };
    let text = is_text_role_name(role_name).then(|| TextInfo {
        text: text_value.clone().unwrap_or_default(),
        caret: None,
        selection: None,
        redacted,
    });
    let value = (!redacted && (node.aria.numeric_value.is_some() || node.aria.value.is_some()))
        .then(|| ValueInfo {
            value: node
                .aria
                .numeric_value
                .map(|number| number.to_string())
                .or(text_value)
                .unwrap_or_default(),
            min: node.aria.min,
            max: node.aria.max,
            step: node.aria.step,
        });
    let mut actions = actions_from_names(role_name, &node.aria.on_action);
    push_action(&mut actions, published.pointer.hover, NodeAction::Hover);
    push_action(&mut actions, published.pointer.drag, NodeAction::Drag);
    push_action(&mut actions, published.pointer.scroll, NodeAction::Scroll);
    if redacted {
        actions.retain(|action| !matches!(action, NodeAction::SetText | NodeAction::SetValue));
    }
    let has_area = published
        .bounds
        .is_none_or(|bounds| bounds.width > 0.0 && bounds.height > 0.0);
    let mut metadata = BTreeMap::new();
    if let Ok(accesskit_id) = node.accesskit_id.parse::<u64>() {
        metadata.insert("accesskit_id".to_owned(), accesskit_id.to_string());
    }
    UiNode {
        id: published.identity,
        parent: published.parent,
        children: Vec::new(),
        role: role_from_name(role_name),
        label,
        description: node.aria.description.clone(),
        bounds: published.bounds,
        state: NodeState {
            visible: !published.hidden && has_area,
            enabled: !node.aria.disabled,
            focused: published.focused,
            checked: match node.aria.toggled.as_deref() {
                Some("True") => Some(true),
                Some("False") => Some(false),
                _ => None,
            },
            selected: node.aria.selected,
            expanded: node.aria.expanded,
        },
        actions,
        text,
        value,
        metadata,
    }
}

/// Append every node reachable from `start` to `order`, depth first, in the
/// order the tree lists children.
impl FrameTree {
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
                    identities[index] = format!(
                        "{}#{}",
                        segments[index].join("/"),
                        self.nodes[index].accesskit_id
                    );
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
                let child_node = &self.nodes[child];
                if child_node.aria.is_redacted() {
                    continue;
                }
                if is_text_role_name(&child_node.aria.role)
                    && let Some(value) = &child_node.aria.value
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

/// Decode GPUI's `Debug` rendering of an [`ElementId`] into a usable identity.
///
/// GPUI records the leaf element id as `format!("{id:?}")`, so a named element
/// arrives as `Name("save")` and a named-and-indexed one as
/// `NamedInteger("row", 3)`. Anything else keeps no element identity and falls
/// back to the AccessKit node id.
/// The identity a named element contributes, from the element id GPUI recorded.
///
/// Matches [`decode_element_id`]'s reading of the debug dump, so a debug and a
/// release build of one app name their nodes the same way.
fn element_id_name(id: &gpui::ElementId) -> Option<String> {
    match id {
        gpui::ElementId::Name(name) => Some(name.to_string()),
        gpui::ElementId::NamedInteger(name, index) => Some(format!("{name}-{index}")),
        gpui::ElementId::Integer(index) => Some(index.to_string()),
        _ => None,
    }
}

fn decode_element_id(raw: &str) -> Option<String> {
    if let Some(inner) = variant_inner(raw, "Name") {
        return decode_quoted(inner);
    }
    if let Some(inner) = variant_inner(raw, "NamedInteger") {
        let (name, index) = inner.rsplit_once(", ")?;
        let name = decode_quoted(name)?;
        let index = index.parse::<u64>().ok()?;
        return Some(format!("{name}-{index}"));
    }
    if let Some(inner) = variant_inner(raw, "Integer") {
        return inner.parse::<u64>().ok().map(|index| index.to_string());
    }
    None
}

fn variant_inner<'a>(raw: &'a str, variant: &str) -> Option<&'a str> {
    raw.strip_prefix(variant)?
        .strip_prefix('(')?
        .strip_suffix(')')
}

fn decode_quoted(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(raw)
        .ok()
        .or_else(|| Some(raw.strip_prefix('"')?.strip_suffix('"')?.to_owned()))
}

/// Map an AccessKit role name onto the protocol's role vocabulary.
fn role_from_name(name: &str) -> Role {
    match name {
        "Application" => Role::Application,
        "Window" | "RootWebArea" => Role::Window,
        "Button" | "DefaultButton" | "DisclosureTriangle" => Role::Button,
        "CheckBox" => Role::Checkbox,
        "RadioButton" => Role::Radio,
        "Switch" => Role::Switch,
        "Link" => Role::Link,
        "Label" | "TextRun" | "Paragraph" | "Heading" | "Legend" | "Caption" | "FigureCaption"
        | "Term" | "Code" | "Emphasis" | "Strong" => Role::Text,
        "TextInput" | "MultilineTextInput" | "EmailInput" | "PasswordInput"
        | "PhoneNumberInput" | "UrlInput" => Role::TextInput,
        "SearchInput" | "Search" => Role::SearchInput,
        "Slider" | "SpinButton" => Role::Slider,
        "ProgressIndicator" | "Meter" => Role::Progress,
        "Image" | "GraphicsSymbol" => Role::Image,
        "List" | "ListBox" => Role::List,
        "ListItem" => Role::ListItem,
        "Tree" => Role::Tree,
        "TreeItem" => Role::TreeItem,
        "Table" | "Grid" | "TreeGrid" | "ListGrid" => Role::Table,
        "Row" | "LayoutTableRow" => Role::Row,
        "Cell" | "GridCell" | "LayoutTableCell" | "RowHeader" | "ColumnHeader" => Role::Cell,
        "Menu" | "MenuBar" | "MenuListPopup" => Role::Menu,
        "MenuItem" | "MenuItemCheckBox" | "MenuItemRadio" => Role::MenuItem,
        "ComboBox" | "EditableComboBox" => Role::Combobox,
        "ListBoxOption" | "MenuListOption" => Role::Option,
        "Splitter" => Role::Separator,
        "Tooltip" => Role::Tooltip,
        "TabList" => Role::TabList,
        "Tab" => Role::Tab,
        "Toolbar" => Role::Toolbar,
        "Dialog" | "AlertDialog" => Role::Dialog,
        "Alert" => Role::Alert,
        "ScrollBar" | "ScrollView" => Role::ScrollArea,
        "Group" | "Pane" | "RadioGroup" | "TabPanel" => Role::Group,
        _ => Role::Generic,
    }
}

/// Whether the role's value is its text content.
fn is_text_role_name(name: &str) -> bool {
    matches!(
        name,
        "TextInput"
            | "MultilineTextInput"
            | "EmailInput"
            | "PasswordInput"
            | "PhoneNumberInput"
            | "UrlInput"
            | "SearchInput"
            | "Label"
            | "TextRun"
            | "Paragraph"
            | "Heading"
            | "Legend"
            | "Caption"
            | "FigureCaption"
            | "Term"
            | "Code"
            | "Emphasis"
            | "Strong"
    )
}

/// Whether the role accepts text replacement through its value.
fn is_editable_text_role_name(name: &str) -> bool {
    matches!(
        name,
        "TextInput"
            | "MultilineTextInput"
            | "SearchInput"
            | "EmailInput"
            | "PasswordInput"
            | "PhoneNumberInput"
            | "UrlInput"
    )
}

/// Map a node's advertised AccessKit actions onto the bridge's vocabulary.
///
/// This is the rule the patched bridge applied to `supports_action`. The
/// patch's behaviour-inferred Hover and Drag have no AccessKit action, so they
/// can no longer be reported.
///
/// Dispatch is native: the platform adapter delivers action requests to the
/// app's [`Window::on_a11y_action`] listeners and otherwise falls back to
/// GPUI's built-in handling (Click clicks the node's bounds, Focus focuses it).
/// The bridge registers no listeners of its own because a matching listener
/// suppresses that fallback.
fn actions_from_names(role_name: &str, on_action: &[String]) -> Vec<NodeAction> {
    let supports = |name: &str| on_action.iter().any(|candidate| candidate == name);
    let mut actions = Vec::new();
    push_action(&mut actions, supports("Click"), NodeAction::Click);
    push_action(&mut actions, supports("Focus"), NodeAction::Focus);
    push_action(
        &mut actions,
        supports("ReplaceSelectedText")
            || (is_editable_text_role_name(role_name) && supports("SetValue")),
        NodeAction::SetText,
    );
    push_action(&mut actions, supports("SetValue"), NodeAction::SetValue);
    push_action(
        &mut actions,
        SCROLL_ACTION_NAMES.iter().any(|name| supports(name)),
        NodeAction::Scroll,
    );
    actions
}

fn push_action(actions: &mut Vec<NodeAction>, condition: bool, action: NodeAction) {
    if condition && !actions.contains(&action) {
        actions.push(action);
    }
}

const SCROLL_ACTION_NAMES: [&str; 7] = [
    "ScrollDown",
    "ScrollLeft",
    "ScrollRight",
    "ScrollUp",
    "ScrollIntoView",
    "ScrollToPoint",
    "SetScrollOffset",
];

/// Derive a label from descendant text for roles that have no label of their own.
fn label_from_content(name: &str, content: &str) -> Option<String> {
    let labelled = matches!(
        name,
        "Button"
            | "DefaultButton"
            | "CheckBox"
            | "RadioButton"
            | "Switch"
            | "Link"
            | "MenuItem"
            | "MenuItemCheckBox"
            | "MenuItemRadio"
            | "ListBoxOption"
            | "MenuListOption"
            | "Tab"
    );
    (labelled && !content.is_empty()).then(|| content.to_owned())
}

#[cfg(test)]
mod tests {
    use gpui::accesskit::NodeId as A11yNodeId;
    use gpui::{
        AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _,
        Styled as _, TestAppContext, Text, Window, div, px,
    };
    use gpui_mcp_protocol::{NodeAction, Rect, Role as McpRole, TextInfo, ValueInfo};

    use super::{NodeLookup, parse_frame};
    use crate::Automation;

    const FRAME_JSON: &str = r#"
    {
      "root": "a",
      "gpui_focus": "d",
      "active_descendant_focus": null,
      "frame": {
        "rendered_at": "2026-01-01T00:00:00.000+00:00",
        "frame_number": 7,
        "window_title": "Demo",
        "node_count": 11,
        "tab_stop_count": 2,
        "viewport_size": { "width": 800.0, "height": 600.0 },
        "scale_factor": 1.0
      },
      "nodes": {
        "a": { "accesskit_id": "0", "children": ["b"], "aria": { "role": "Window", "label": "Demo" } },
        "b": { "accesskit_id": "1", "children": ["c", "d", "g", "h", "i", "k", "l"], "element_id": "Name(\"root\")", "view": "demo::Root", "aria": { "role": "Application" } },
        "c": { "accesskit_id": "2", "children": ["e"], "element_id": "Name(\"save\")", "aria": { "role": "Button", "on_action": ["Click", "Focus", "ReplaceSelectedText", "SetValue", "ScrollDown"] } },
        "d": { "accesskit_id": "3", "element_id": "Name(\"volume\")", "aria": { "role": "Slider", "label": "Volume", "numeric_value": 3.0, "min_numeric_value": 0.0, "max_numeric_value": 11.0, "numeric_value_step": 1.0, "on_action": ["SetValue"] } },
        "e": { "accesskit_id": "4", "element_id": "Name(\"save-label\")", "aria": { "role": "Label", "value": "Save" } },
        "g": { "accesskit_id": "5", "element_id": "Name(\"panel\")", "aria": { "role": "Group" } },
        "h": { "accesskit_id": "6", "element_id": "Name(\"panel\")", "aria": { "role": "Group", "selected": true, "expanded": false, "toggled": "True" } },
        "i": { "accesskit_id": "7", "element_id": "Name(\"q/y\")", "aria": { "role": "Group" } },
        "k": { "accesskit_id": "8", "children": ["j"], "element_id": "Name(\"q\")", "aria": { "role": "Group" } },
        "j": { "accesskit_id": "9", "element_id": "Name(\"y\")", "aria": { "role": "Group" } },
        "l": { "accesskit_id": "10", "element_id": "Name(\"y\")", "aria": { "role": "Group" } }
      }
    }
    "#;

    #[test]
    fn parses_roles_labels_values_states_and_parentage() {
        let nodes = parse_frame(FRAME_JSON, |accesskit_id| NodeLookup {
            bounds: (accesskit_id == 2).then_some(Rect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            }),
            ..NodeLookup::default()
        })
        .unwrap_or_default();
        assert!(!nodes.is_empty(), "the fixture parses");
        let tree: std::collections::HashMap<&str, &gpui_mcp_protocol::UiNode> =
            nodes.iter().map(|node| (node.id.as_str(), node)).collect();

        assert_eq!(nodes.len(), 10, "the window host node is not published");
        assert_eq!(tree["root"].role, McpRole::Application);
        assert_eq!(tree["root"].parent, None);
        assert!(
            tree["root"].bounds.is_none(),
            "a node without a bounds lookup stays empty"
        );
        assert_eq!(
            tree["save"].bounds,
            Some(Rect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            }),
            "bounds resolve through the window's accessibility map"
        );
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
        assert_eq!(tree["save"].children, Vec::<String>::new());
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
        assert!(
            tree["volume"].state.focused,
            "gpui_focus names the focused node"
        );
        assert_eq!(tree["volume"].actions, [NodeAction::SetValue]);
    }

    #[test]
    fn colliding_element_ids_get_frame_unique_identities() {
        let nodes = parse_frame(FRAME_JSON, |_| NodeLookup::default()).unwrap_or_default();
        let tree: std::collections::HashMap<&str, &gpui_mcp_protocol::UiNode> =
            nodes.iter().map(|node| (node.id.as_str(), node)).collect();
        assert!(!tree.is_empty(), "the fixture parses");

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

    const STATE_JSON: &str = r#"
    {
      "root": "a",
      "nodes": {
        "a": { "accesskit_id": "0", "children": ["b"], "aria": { "role": "Window" } },
        "b": { "accesskit_id": "1", "children": ["c", "e", "f", "g"], "element_id": "Name(\"root\")", "aria": { "role": "Application" } },
        "c": { "accesskit_id": "2", "children": ["d"], "element_id": "Name(\"drawer\")", "aria": { "role": "Group", "hidden": true } },
        "d": { "accesskit_id": "3", "element_id": "Name(\"drawer-close\")", "aria": { "role": "Button", "on_action": ["Click"] } },
        "e": { "accesskit_id": "4", "element_id": "Name(\"locked\")", "aria": { "role": "Button", "disabled": true } },
        "f": { "accesskit_id": "5", "element_id": "Name(\"pin\")", "aria": { "role": "PasswordInput", "value": "1234", "on_action": ["Focus", "SetValue", "ReplaceSelectedText"] } },
        "g": { "accesskit_id": "6", "children": ["h"], "element_id": "Name(\"reveal\")", "aria": { "role": "Button" } },
        "h": { "accesskit_id": "7", "element_id": "Name(\"reveal-pin\")", "aria": { "role": "PasswordInput", "value": "1234" } }
      }
    }
    "#;

    #[test]
    fn hidden_inherits_disabled_reads_and_password_input_redacts() {
        let nodes = parse_frame(STATE_JSON, |_| NodeLookup::default()).unwrap_or_default();
        let tree: std::collections::HashMap<&str, &gpui_mcp_protocol::UiNode> =
            nodes.iter().map(|node| (node.id.as_str(), node)).collect();
        assert!(!tree.is_empty(), "the fixture parses");

        assert!(!tree["drawer"].state.visible);
        assert!(
            !tree["drawer-close"].state.visible,
            "a hidden container hides its descendants"
        );
        assert!(tree["locked"].state.visible);
        assert!(!tree["locked"].state.enabled);
        assert!(tree["root"].state.enabled);

        let pin = tree["pin"];
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
        visual.update(|window, cx| {
            assert!(
                window.simulate_next_frame(cx) > 0,
                "the observation callback is armed"
            );
        });

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

    /// Per-stage cost of observing a ~5,000-node frame. Run with
    /// `cargo test --release -p gpui-mcp --lib observation_stage_costs -- --ignored --nocapture`.
    #[gpui::test]
    #[ignore = "benchmark; prints timings rather than asserting"]
    fn observation_stage_costs(cx: &mut TestAppContext) {
        use std::time::Instant;
        type Stage<'a> = (&'static str, Box<dyn FnMut() + 'a>);
        let automation = Automation::isolated();
        let for_window = automation.clone();
        let (_view, visual) = cx.add_window_view(move |window, _| {
            for_window.attach(window);
            Heavy { rows: 2_500 }
        });
        visual.run_until_parked();
        visual.update(Window::simulate_next_frame);
        let json = visual
            .update(|window, _| window.debug_a11y_tree_json())
            .unwrap_or_default();
        let parsed = super::parse_frame(&json, |_| NodeLookup::default()).unwrap_or_default();
        let state = crate::registry::SharedState::new();
        let tree = automation.snapshot();
        let bytes = serde_json::to_vec(&tree).unwrap_or_default();
        let mut stages: Vec<Stage<'_>> = vec![
            (
                "debug_a11y_tree_json",
                Box::new(|| {
                    let _ = visual.update(|window, _| window.debug_a11y_tree_json());
                }),
            ),
            (
                "parse_frame",
                Box::new(|| {
                    let _ = super::parse_frame(&json, |_| NodeLookup::default());
                }),
            ),
            (
                "publish_frame",
                Box::new(|| {
                    state.begin_frame();
                    state.publish_frame(parsed.clone());
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

    #[gpui::test]
    #[ignore = "known gap: observation is armed only by bridge operations, so a frame the app draws on its own is not published"]
    fn app_driven_changes_reach_the_tree(cx: &mut TestAppContext) {
        let automation = Automation::isolated();
        let automation_for_window = automation.clone();
        let (view, visual) = cx.add_window_view(move |window, _| {
            automation_for_window.attach(window);
            Toggle { on: false }
        });
        visual.run_until_parked();
        visual.update(Window::simulate_next_frame);
        assert!(!automation.snapshot().nodes["switch"].state.enabled);

        // The app changes its own state; no bridge operation arms observation.
        visual.update(|_, cx| {
            view.update(cx, |toggle, cx| {
                toggle.on = true;
                cx.notify();
            });
        });
        visual.run_until_parked();
        visual.update(Window::simulate_next_frame);
        assert!(
            automation.snapshot().nodes["switch"].state.enabled,
            "a frame the app drew on its own is observed"
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

        assert!(
            visual.update(|window, _| window.debug_a11y_tree_json().is_some()),
            "the accessibility tree is available without assistive technology (C02)"
        );
        let dump = visual
            .update(|window, _| window.debug_a11y_tree_json())
            .unwrap_or_default();
        assert!(
            dump.contains(r#"Name(\"root\")"#),
            "the dump carries the application's elements: {dump}"
        );
        // Tests have no platform frame loop, so deliver the armed observation
        // callback the way the platform would after the frame above.
        visual.update(|window, cx| {
            assert!(
                window.simulate_next_frame(cx) > 0,
                "the observation callback is armed"
            );
        });
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
            visual.update(|window, cx| window.a11y_focus_handle(A11yNodeId(accesskit_id), cx));
        assert_eq!(resolved.as_ref(), Some(&focus));
        visual.update(|window, cx| {
            let Some(handle) = window.a11y_focus_handle(A11yNodeId(accesskit_id), cx) else {
                return;
            };
            window.focus(&handle, cx);
        });
        assert_eq!(
            visual.update(|window, cx| window.focused(cx)).as_ref(),
            Some(&focus)
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
        // Tests have no platform frame loop, so deliver the armed observation
        // callback the way the platform would after the frame above.
        visual.update(|window, cx| {
            assert!(
                window.simulate_next_frame(cx) > 0,
                "the observation callback is armed"
            );
        });

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
