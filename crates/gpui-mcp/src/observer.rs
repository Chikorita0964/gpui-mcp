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
//! GPUI records `element_id` and `view` provenance only in `debug_assertions`
//! builds, so release builds identify nodes by their AccessKit node id.
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
//! - `hidden` and `disabled` state is not emitted, so nodes publish as visible
//!   and enabled.
//! - Application metadata and redaction are not emitted, so nodes publish no
//!   application metadata and text publishes unredacted; [`UiNode::metadata`]
//!   carries the node's `accesskit_id` for focus resolution.
//! - Hover and Drag have no AccessKit action, so only Click, Focus, SetText,
//!   SetValue, and Scroll can be reported.
//! - The overlay paint pass has no stock equivalent, so highlight overlays are
//!   not drawn.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Weak};

use gpui::Window;
use gpui::accesskit::NodeId;
use gpui_mcp_protocol::{NodeAction, NodeState, Rect, Role, TextInfo, UiNode, ValueInfo};
use serde_json::Value as Json;

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
        let Some(nodes) = parse_frame(&json, |accesskit_id| {
            window
                .a11y_node_bounds(NodeId(accesskit_id))
                .map(rect_from_gpui)
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

/// One node of `debug_a11y_tree_json()` before it is named.
struct RawNode {
    accesskit_id: String,
    children: Vec<String>,
    element_id: Option<String>,
    aria: Aria,
}

impl RawNode {
    /// The node's own identifier: its element id, or its AccessKit node id.
    fn own_id(&self) -> String {
        self.element_id
            .clone()
            .unwrap_or_else(|| self.accesskit_id.clone())
    }
}

/// Accessibility semantics carried by one JSON node.
#[derive(Default)]
struct Aria {
    role: String,
    label: Option<String>,
    description: Option<String>,
    value: Option<String>,
    numeric_value: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
    selected: Option<bool>,
    expanded: Option<bool>,
    toggled: Option<String>,
    on_action: Vec<String>,
}

/// Translate one `debug_a11y_tree_json()` document into protocol nodes.
///
/// The document's host node is not published: it is GPUI's window node, it
/// carries no element identity, and its children are the application's roots.
/// `bounds_for` resolves each node's AccessKit id to its logical bounds; each
/// published node also carries that id as `metadata["accesskit_id"]` so focus
/// requests can resolve a handle. Returns `None` when the document does not
/// carry a node map.
fn parse_frame(json: &str, bounds_for: impl Fn(u64) -> Option<Rect>) -> Option<Vec<UiNode>> {
    let frame: Json = serde_json::from_str(json).ok()?;
    let nodes_json = frame.get("nodes")?.as_object()?;
    let root_key = frame.get("root").and_then(Json::as_str).map(str::to_owned);
    let focus_key = frame
        .get("gpui_focus")
        .and_then(Json::as_str)
        .map(str::to_owned);

    let mut raw: HashMap<String, RawNode> = HashMap::with_capacity(nodes_json.len());
    for (key, node) in nodes_json {
        let Some(node) = node.as_object() else {
            continue;
        };
        let accesskit_id = node
            .get("accesskit_id")
            .and_then(Json::as_str)
            .unwrap_or(key.as_str())
            .to_owned();
        let children = node
            .get("children")
            .and_then(Json::as_array)
            .map(|children| {
                children
                    .iter()
                    .filter_map(Json::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let element_id = node
            .get("element_id")
            .and_then(Json::as_str)
            .and_then(decode_element_id);
        let aria = parse_aria(node.get("aria"));
        raw.insert(
            key.clone(),
            RawNode {
                accesskit_id,
                children,
                element_id,
                aria,
            },
        );
    }

    let host_key = root_key
        .as_ref()
        .filter(|root| raw.get(*root).is_some_and(|node| node.element_id.is_none()))
        .cloned();

    let mut parent_of: HashMap<String, String> = HashMap::with_capacity(raw.len());
    for (key, node) in &raw {
        for child in &node.children {
            if raw.contains_key(child) {
                parent_of.insert(child.clone(), key.clone());
            }
        }
    }

    let mut order = Vec::with_capacity(raw.len());
    let mut visited: HashSet<String> = HashSet::with_capacity(raw.len());
    let start = match &root_key {
        Some(root) => raw.get(root).map(|node| node.children.clone()).unwrap_or_default(),
        None => Vec::new(),
    };
    collect_order(&raw, &start, host_key.as_deref(), &mut visited, &mut order);
    let mut leftovers: Vec<String> = raw
        .keys()
        .filter(|key| !visited.contains(*key))
        .cloned()
        .collect();
    leftovers.sort();
    collect_order(
        &raw,
        &leftovers,
        host_key.as_deref(),
        &mut visited,
        &mut order,
    );

    let mut segments: HashMap<String, Vec<String>> = HashMap::with_capacity(order.len());
    for key in &order {
        let mut path = Vec::new();
        let mut current = Some(key.clone());
        while let Some(node_key) = current {
            if host_key.as_deref() == Some(node_key.as_str()) {
                break;
            }
            let Some(node) = raw.get(&node_key) else {
                break;
            };
            path.push(node.own_id());
            current = parent_of.get(&node_key).cloned();
        }
        path.reverse();
        segments.insert(key.clone(), path);
    }

    let identities = assign_identities(&order, &segments, &raw);

    let mut content: HashMap<String, String> = HashMap::with_capacity(order.len());
    let mut visiting: HashSet<String> = HashSet::with_capacity(order.len());
    for key in &order {
        content_text(key, &raw, &mut content, &mut visiting);
    }

    let mut nodes = Vec::with_capacity(order.len());
    for key in &order {
        let Some(node) = raw.get(key) else {
            continue;
        };
        let role_name = node.aria.role.as_str();
        let role = role_from_name(role_name);
        let node_content = content.get(key).cloned().unwrap_or_default();
        let label = node
            .aria
            .label
            .clone()
            .or_else(|| label_from_content(role_name, &node_content));
        let text_value = node
            .aria
            .value
            .clone()
            .or_else(|| (!node_content.is_empty()).then(|| node_content.clone()));
        let text = is_text_role_name(role_name).then(|| TextInfo {
            text: text_value.clone().unwrap_or_default(),
            caret: None,
            selection: None,
            redacted: false,
        });
        let value = (node.aria.numeric_value.is_some() || node.aria.value.is_some()).then(|| {
            ValueInfo {
                value: node
                    .aria
                    .numeric_value
                    .map(|number| number.to_string())
                    .or(text_value)
                    .unwrap_or_default(),
                min: node.aria.min,
                max: node.aria.max,
                step: node.aria.step,
            }
        });
        let actions = actions_from_names(role_name, &node.aria.on_action);
        let identity = identities
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.clone());
        let accesskit_id = node.accesskit_id.parse::<u64>().ok();
        let bounds = accesskit_id.and_then(&bounds_for);
        let mut metadata = BTreeMap::new();
        if let Some(accesskit_id) = accesskit_id {
            metadata.insert("accesskit_id".to_owned(), accesskit_id.to_string());
        }
        nodes.push(UiNode {
            id: identity,
            parent: parent_of
                .get(key)
                .and_then(|parent| identities.get(parent))
                .cloned(),
            children: Vec::new(),
            role,
            label,
            description: node.aria.description.clone(),
            bounds,
            state: NodeState {
                visible: true,
                enabled: true,
                focused: focus_key.as_deref() == Some(key.as_str()),
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
        });
    }
    Some(nodes)
}

/// Append every node reachable from `start` to `order`, depth first, in the
/// order the tree lists children.
fn collect_order(
    raw: &HashMap<String, RawNode>,
    start: &[String],
    host_key: Option<&str>,
    visited: &mut HashSet<String>,
    order: &mut Vec<String>,
) {
    let mut stack: Vec<String> = start.iter().rev().cloned().collect();
    while let Some(key) = stack.pop() {
        if host_key == Some(key.as_str()) || !visited.insert(key.clone()) {
            continue;
        }
        order.push(key.clone());
        if let Some(node) = raw.get(&key) {
            for child in node.children.iter().rev() {
                if raw.contains_key(child) {
                    stack.push(child.clone());
                }
            }
        }
    }
}

/// Name every published node so that no two nodes share an identity.
///
/// This is the same rule the patched GPUI applied to element paths, applied to
/// the ancestor path this module can reconstruct from the accessibility tree.
fn assign_identities(
    order: &[String],
    segments: &HashMap<String, Vec<String>>,
    raw: &HashMap<String, RawNode>,
) -> HashMap<String, String> {
    let mut identities: HashMap<String, String> = HashMap::with_capacity(order.len());
    let mut taken: HashSet<String> = HashSet::with_capacity(order.len());
    let mut unresolved: Vec<&String> = order.iter().collect();
    let mut depth = 1usize;
    while !unresolved.is_empty() {
        let mut counts: HashMap<String, usize> = HashMap::with_capacity(unresolved.len());
        for key in &unresolved {
            *counts.entry(path_suffix(&segments[*key], depth)).or_default() += 1;
        }
        let exhausted = unresolved
            .iter()
            .all(|key| segments[*key].len() <= depth);
        let mut settled: Vec<String> = Vec::new();
        unresolved.retain(|key| {
            let candidate = path_suffix(&segments[*key], depth);
            if counts.get(&candidate).copied() == Some(1) && !taken.contains(&candidate) {
                identities.insert((*key).clone(), candidate);
                settled.push((*key).clone());
                return false;
            }
            if exhausted {
                let unique = format!("{}#{}", segments[*key].join("/"), raw[*key].accesskit_id);
                identities.insert((*key).clone(), unique);
                settled.push((*key).clone());
                return false;
            }
            true
        });
        for key in settled {
            taken.insert(identities[&key].clone());
        }
        depth += 1;
    }
    identities
}

/// The trailing `depth` segments of a path, joined for display.
fn path_suffix(segments: &[String], depth: usize) -> String {
    let start = segments.len().saturating_sub(depth);
    segments[start..].join("/")
}

/// Normalized descendant text of a node: every text-role child's value and
/// every descendant's own collected text, in tree order.
fn content_text(
    key: &str,
    raw: &HashMap<String, RawNode>,
    memo: &mut HashMap<String, String>,
    visiting: &mut HashSet<String>,
) -> String {
    if let Some(text) = memo.get(key) {
        return text.clone();
    }
    if !visiting.insert(key.to_owned()) {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(node) = raw.get(key) {
        for child in &node.children {
            let Some(child_node) = raw.get(child) else {
                continue;
            };
            if is_text_role_name(&child_node.aria.role) {
                if let Some(value) = &child_node.aria.value {
                    let normalized = normalize_text(value);
                    if !normalized.is_empty() {
                        parts.push(normalized);
                    }
                }
            }
            let child_text = content_text(child, raw, memo, visiting);
            if !child_text.is_empty() {
                parts.push(child_text);
            }
        }
    }
    visiting.remove(key);
    let joined = parts.join(" ");
    memo.insert(key.to_owned(), joined.clone());
    joined
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode GPUI's `Debug` rendering of an [`ElementId`] into a usable identity.
///
/// GPUI records the leaf element id as `format!("{id:?}")`, so a named element
/// arrives as `Name("save")` and a named-and-indexed one as
/// `NamedInteger("row", 3)`. Anything else keeps no element identity and falls
/// back to the AccessKit node id.
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
    raw.strip_prefix(variant)?.strip_prefix('(')?.strip_suffix(')')
}

fn decode_quoted(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(raw)
        .ok()
        .or_else(|| Some(raw.strip_prefix('"')?.strip_suffix('"')?.to_owned()))
}

fn parse_aria(aria: Option<&Json>) -> Aria {
    let Some(aria) = aria else {
        return Aria::default();
    };
    Aria {
        role: aria
            .get("role")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        label: string_field(aria, "label"),
        description: string_field(aria, "description"),
        value: string_field(aria, "value"),
        numeric_value: number_field(aria, "numeric_value"),
        min: number_field(aria, "min_numeric_value"),
        max: number_field(aria, "max_numeric_value"),
        step: number_field(aria, "numeric_value_step"),
        selected: aria.get("selected").and_then(Json::as_bool),
        expanded: aria.get("expanded").and_then(Json::as_bool),
        toggled: string_field(aria, "toggled"),
        on_action: aria
            .get("on_action")
            .and_then(Json::as_array)
            .map(|actions| {
                actions
                    .iter()
                    .filter_map(Json::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn string_field(object: &Json, key: &str) -> Option<String> {
    object.get(key).and_then(Json::as_str).map(str::to_owned)
}

fn number_field(object: &Json, key: &str) -> Option<f64> {
    object.get(key).and_then(Json::as_f64)
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
        "TextInput" | "MultilineTextInput" | "EmailInput" | "PasswordInput" | "PhoneNumberInput"
        | "UrlInput" => Role::TextInput,
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
        ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
        TestAppContext, Text, Window, div, px,
    };
    use gpui_mcp_protocol::{NodeAction, Rect, Role as McpRole, TextInfo, ValueInfo};

    use super::parse_frame;
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
        let nodes = parse_frame(FRAME_JSON, |accesskit_id| {
            (accesskit_id == 2).then_some(Rect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            })
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
            tree["save"].metadata.get("accesskit_id").map(String::as_str),
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
        assert!(tree["volume"].state.focused, "gpui_focus names the focused node");
        assert_eq!(tree["volume"].actions, [NodeAction::SetValue]);

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
        assert!(accesskit_id.is_some(), "the field publishes its accesskit id");
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
            let text = tree.nodes[&panel.children[0]].text.clone().unwrap_or_default();
            titles.push(text.text);
        }
        titles.sort();
        assert_eq!(titles, ["Console", "Hierarchy"]);
    }
}
