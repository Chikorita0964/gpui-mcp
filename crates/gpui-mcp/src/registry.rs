use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_mcp_protocol::{
    BridgeError, ErrorCode, FrameStats, Highlight, LogEntry, MAX_ID_BYTES, MAX_LABEL_BYTES,
    MAX_METADATA_FIELDS, MAX_METADATA_KEY_BYTES, MAX_METADATA_VALUE_BYTES, MAX_TEXT_BYTES,
    MAX_TREE_NODES, Rect, SemanticDiagnostic, SemanticDiagnosticCode, TreeDelta, UiNode, UiTree,
    WindowGeometry,
};
use tokio::sync::watch;
use tokio::time::timeout;

const MAX_TIMING_SAMPLES: usize = 240;
const MAX_DIAGNOSTICS: usize = 128;
/// Published generations whose changes stay available for deltas.
const MAX_CHANGE_RECORDS: usize = 64;

/// What one published generation changed relative to the one before it.
#[derive(Debug)]
struct ChangeRecord {
    generation: u64,
    ids: Vec<String>,
    roots: bool,
    diagnostics: bool,
}

#[derive(Debug, Default)]
struct Published {
    tree: UiTree,
    changes: VecDeque<ChangeRecord>,
}

impl Published {
    /// The changes from `known` to the current generation, while every step is retained.
    fn delta_since(&self, known: u64) -> Option<TreeDelta> {
        let tree = &self.tree;
        let first = known.checked_add(1)?;
        let start = self
            .changes
            .iter()
            .position(|change| change.generation == first)?;
        let mut ids = BTreeSet::new();
        let mut roots = false;
        let mut diagnostics = false;
        for change in self.changes.iter().skip(start) {
            ids.extend(change.ids.iter().map(String::as_str));
            roots |= change.roots;
            diagnostics |= change.diagnostics;
        }
        // A delta touching more ids than the tree holds costs more than the tree.
        if ids.len() > tree.nodes.len() {
            return None;
        }
        let mut delta = TreeDelta {
            base_generation: known,
            generation: tree.generation,
            roots: roots.then(|| tree.roots.clone()),
            upserted: Vec::new(),
            removed: Vec::new(),
            diagnostics: diagnostics.then(|| tree.diagnostics.clone()),
        };
        for id in ids {
            match tree.nodes.get(id) {
                Some(node) => delta.upserted.push(node.clone()),
                None => delta.removed.push(id.to_owned()),
            }
        }
        Some(delta)
    }
}

/// The latest tree relative to a generation the caller holds.
#[derive(Debug)]
pub(crate) enum TreeUpdate {
    Unchanged,
    Delta(TreeDelta),
    Full(UiTree),
}

#[derive(Debug, Default)]
struct PendingFrame {
    active: bool,
    nodes: BTreeMap<String, UiNode>,
    order: Vec<String>,
    invalid_ids: BTreeSet<String>,
    diagnostics: Vec<SemanticDiagnostic>,
}

#[derive(Debug)]
struct TimingState {
    previous_frame: Option<Instant>,
    frame_count: u64,
    intervals: VecDeque<Duration>,
    prepaint: VecDeque<Duration>,
    paint: VecDeque<Duration>,
}

impl Default for TimingState {
    fn default() -> Self {
        Self {
            previous_frame: None,
            frame_count: 0,
            intervals: VecDeque::with_capacity(MAX_TIMING_SAMPLES),
            prepaint: VecDeque::with_capacity(MAX_TIMING_SAMPLES),
            paint: VecDeque::with_capacity(MAX_TIMING_SAMPLES),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SharedState {
    published: RwLock<Published>,
    pending: Mutex<PendingFrame>,
    highlights: RwLock<Vec<Highlight>>,
    timings: Mutex<TimingState>,
    logs: Mutex<VecDeque<LogEntry>>,
    generation: watch::Sender<u64>,
    completed_frame: watch::Sender<FrameStats>,
    window_geometry: RwLock<Option<WindowGeometry>>,
    observing: AtomicBool,
    last_activity: Mutex<Instant>,
}

impl SharedState {
    pub(crate) fn new() -> Arc<Self> {
        let (generation, _) = watch::channel(0);
        let (completed_frame, _) = watch::channel(FrameStats::default());
        Arc::new(Self {
            published: RwLock::new(Published::default()),
            pending: Mutex::new(PendingFrame::default()),
            highlights: RwLock::new(Vec::new()),
            timings: Mutex::new(TimingState::default()),
            logs: Mutex::new(VecDeque::with_capacity(512)),
            generation,
            completed_frame,
            window_geometry: RwLock::new(None),
            observing: AtomicBool::new(false),
            last_activity: Mutex::new(Instant::now()),
        })
    }

    /// Whether the observed window builds its accessibility tree every frame.
    pub(crate) fn is_observing(&self) -> bool {
        self.observing.load(Ordering::Acquire)
    }

    pub(crate) fn set_observing(&self, observing: bool) {
        self.observing.store(observing, Ordering::Release);
    }

    /// Record that a client used the bridge.
    pub(crate) fn touch(&self) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    /// Time since a client last used the bridge.
    pub(crate) fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }

    /// Count one drawn frame and its measured phases, and wake frame waiters.
    pub(crate) fn complete_frame(&self, prepaint: Duration, paint: Duration) {
        let now = Instant::now();
        let stats = {
            let mut timings = self
                .timings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            timings.frame_count = timings.frame_count.saturating_add(1);
            if let Some(previous) = timings.previous_frame.replace(now) {
                push_sample(
                    &mut timings.intervals,
                    now.saturating_duration_since(previous),
                );
            }
            push_sample(&mut timings.prepaint, prepaint);
            push_sample(&mut timings.paint, paint);
            frame_stats_from_timings(&timings)
        };
        self.completed_frame.send_replace(stats);
    }

    pub(crate) fn begin_frame(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_was_incomplete = pending.active;
        pending.active = true;
        pending.nodes.clear();
        pending.order.clear();
        pending.invalid_ids.clear();
        pending.diagnostics.clear();
        if previous_was_incomplete {
            push_diagnostic(
                &mut pending,
                SemanticDiagnosticCode::InvalidNode,
                None,
                "the previous semantic frame did not reach root paint",
            );
        }
    }

    pub(crate) fn set_window_geometry(&self, content_bounds: Rect, scale_factor: f32) {
        let geometry = WindowGeometry {
            content_bounds,
            scale_factor,
        };
        if !geometry.is_valid() {
            return;
        }
        *self
            .window_geometry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(geometry);
    }

    pub(crate) fn window_geometry(&self) -> Option<WindowGeometry> {
        *self
            .window_geometry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, mut node: UiNode) -> bool {
        node.children.clear();

        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !pending.active {
            return false;
        }
        if let Err(message) = validate_node(&node) {
            let node_id = valid_diagnostic_id(&node.id).then(|| node.id.clone());
            push_diagnostic(
                &mut pending,
                SemanticDiagnosticCode::InvalidNode,
                node_id,
                message,
            );
            return false;
        }
        if pending.invalid_ids.contains(&node.id) {
            return false;
        }
        if pending.nodes.remove(&node.id).is_some() {
            pending.order.retain(|id| id != &node.id);
            pending.invalid_ids.insert(node.id.clone());
            push_diagnostic(
                &mut pending,
                SemanticDiagnosticCode::DuplicateId,
                Some(node.id),
                "every node with this duplicate semantic identifier was omitted",
            );
            return false;
        }
        if pending.nodes.len() >= MAX_TREE_NODES {
            push_diagnostic(
                &mut pending,
                SemanticDiagnosticCode::CapacityExceeded,
                None,
                "semantic tree capacity was exceeded",
            );
            return false;
        }
        pending.order.push(node.id.clone());
        pending.nodes.insert(node.id.clone(), node);
        true
    }

    pub(crate) fn publish_frame(&self, nodes: impl IntoIterator<Item = UiNode>) {
        self.begin_frame();
        for node in nodes {
            self.record(node);
        }
        self.finish_frame();
    }

    pub(crate) fn finish_frame(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !pending.active {
            return;
        }
        pending.active = false;

        discard_invalid_relationships(&mut pending);
        let roots = build_relationships(&mut pending);
        let nodes = std::mem::take(&mut pending.nodes);
        pending.order.clear();
        pending.invalid_ids.clear();
        let diagnostics = std::mem::take(&mut pending.diagnostics);
        drop(pending);

        let mut published = self
            .published
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tree = &published.tree;
        let mut ids: Vec<String> = nodes
            .iter()
            .filter(|(id, node)| tree.nodes.get(*id) != Some(*node))
            .map(|(id, _)| id.clone())
            .collect();
        ids.extend(
            tree.nodes
                .keys()
                .filter(|id| !nodes.contains_key(*id))
                .cloned(),
        );
        let roots_changed = tree.roots != roots;
        let diagnostics_changed = tree.diagnostics != diagnostics;
        if ids.is_empty() && !roots_changed && !diagnostics_changed {
            // Keep the published maps; the fresh copies are dropped after the lock.
            drop(published);
            return;
        }
        let generation = published.tree.generation.saturating_add(1);
        published.tree.generation = generation;
        let previous_nodes = std::mem::replace(&mut published.tree.nodes, nodes);
        published.tree.roots = roots;
        published.tree.diagnostics = diagnostics;
        if published.changes.len() == MAX_CHANGE_RECORDS {
            published.changes.pop_front();
        }
        published.changes.push_back(ChangeRecord {
            generation,
            ids,
            roots: roots_changed,
            diagnostics: diagnostics_changed,
        });
        drop(published);
        drop(previous_nodes);
        self.generation.send_replace(generation);
    }

    pub(crate) fn tree(&self) -> UiTree {
        self.published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tree
            .clone()
    }

    /// Read one published node without cloning the tree.
    pub(crate) fn with_node<R>(&self, id: &str, read: impl FnOnce(&UiNode) -> R) -> Option<R> {
        self.published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tree
            .nodes
            .get(id)
            .map(read)
    }

    /// The latest tree relative to `known_generation`: nothing when it is
    /// current, its changes while they are retained, and otherwise all of it.
    pub(crate) fn tree_since(&self, known_generation: u64) -> TreeUpdate {
        let published = self
            .published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if published.tree.generation == known_generation {
            return TreeUpdate::Unchanged;
        }
        published.delta_since(known_generation).map_or_else(
            || TreeUpdate::Full(published.tree.clone()),
            TreeUpdate::Delta,
        )
    }

    pub(crate) fn tree_generation(&self) -> u64 {
        self.published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tree
            .generation
    }

    /// Wait for a generation newer than `after_generation`, then answer as
    /// [`Self::tree_since`] does.
    pub(crate) async fn wait_for_tree(
        &self,
        after_generation: u64,
        wait: Duration,
    ) -> Result<TreeUpdate, BridgeError> {
        if self.tree_generation() > after_generation {
            return Ok(self.tree_since(after_generation));
        }

        let mut receiver = self.generation.subscribe();
        let changed = async {
            loop {
                if *receiver.borrow_and_update() > after_generation {
                    return Ok(());
                }
                receiver.changed().await.map_err(|_| {
                    BridgeError::new(ErrorCode::Internal, "semantic tree publisher stopped")
                })?;
            }
        };
        timeout(wait, changed)
            .await
            .map_err(|_| BridgeError::new(ErrorCode::Timeout, "semantic tree wait timed out"))??;
        Ok(self.tree_since(after_generation))
    }

    pub(crate) async fn wait_for_frame(
        &self,
        after_frame_count: u64,
        wait: Duration,
    ) -> Result<FrameStats, BridgeError> {
        let mut receiver = self.completed_frame.subscribe();
        let current = receiver.borrow_and_update().clone();
        if current.frame_count > after_frame_count {
            return Ok(current);
        }

        let changed = async {
            loop {
                receiver.changed().await.map_err(|_| {
                    BridgeError::new(ErrorCode::Internal, "frame publisher stopped")
                })?;
                let current = receiver.borrow_and_update().clone();
                if current.frame_count > after_frame_count {
                    return Ok(current);
                }
            }
        };
        timeout(wait, changed)
            .await
            .map_err(|_| BridgeError::new(ErrorCode::Timeout, "frame wait timed out"))?
    }

    /// Replace the highlight set the frame observer paints over the window.
    pub(crate) fn set_highlights(&self, highlights: Vec<Highlight>) {
        *self
            .highlights
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = highlights;
    }

    /// Read the current highlight set without cloning it.
    pub(crate) fn with_highlights<R>(&self, read: impl FnOnce(&[Highlight]) -> R) -> R {
        read(
            &self
                .highlights
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub(crate) fn frame_stats(&self) -> FrameStats {
        self.completed_frame.borrow().clone()
    }

    pub(crate) fn add_log(&self, level: &str, message: &str) {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let mut sanitized = message.replace(['\r', '\n'], " ");
        sanitized.truncate(sanitized.floor_char_boundary(4096));
        let mut logs = self
            .logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if logs.len() == 512 {
            logs.pop_front();
        }
        logs.push_back(LogEntry {
            timestamp_ms,
            level: normalize_level(level).to_owned(),
            message: sanitized,
        });
    }

    pub(crate) fn logs(&self, limit: u16, min_level: Option<&str>) -> Vec<LogEntry> {
        let threshold = min_level.map_or(0, level_rank);
        let take = usize::from(limit.min(512));
        self.logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .filter(|entry| level_rank(&entry.level) >= threshold)
            .take(take)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub(crate) fn clear_logs(&self) {
        self.logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

fn validate_node(node: &UiNode) -> Result<(), &'static str> {
    validate_id(&node.id)?;
    if let Some(parent) = &node.parent {
        validate_id(parent)?;
    }
    for text in [node.label.as_deref(), node.description.as_deref()]
        .into_iter()
        .flatten()
    {
        if text.len() > MAX_LABEL_BYTES || text.chars().any(char::is_control) {
            return Err("semantic label or description is invalid or exceeds 4 KiB");
        }
    }
    if node.bounds.is_some_and(|bounds| {
        !bounds.is_valid()
            || bounds.x.abs() > 1_000_000.0
            || bounds.y.abs() > 1_000_000.0
            || bounds.width > 1_000_000.0
            || bounds.height > 1_000_000.0
    }) {
        return Err("semantic bounds are invalid or exceed the coordinate limit");
    }
    if node
        .actions
        .iter()
        .enumerate()
        .any(|(index, action)| node.actions[..index].contains(action))
    {
        return Err("semantic actions contain a duplicate");
    }
    if let Some(text) = &node.text {
        if text.text.len() > MAX_TEXT_BYTES {
            return Err("semantic text exceeds 64 KiB");
        }
        if text.redacted && !text.text.is_empty() {
            return Err("redacted semantic text must not contain a value");
        }
        if text
            .caret
            .is_some_and(|caret| caret > text.text.len() || !text.text.is_char_boundary(caret))
        {
            return Err("semantic text caret is not a valid UTF-8 boundary");
        }
        if text.selection.is_some_and(|range| {
            range.start > range.end
                || range.end > text.text.len()
                || !text.text.is_char_boundary(range.start)
                || !text.text.is_char_boundary(range.end)
        }) {
            return Err("semantic text selection is not a valid UTF-8 range");
        }
    }
    if let Some(value) = &node.value {
        if value.value.len() > MAX_TEXT_BYTES {
            return Err("semantic value exceeds 64 KiB");
        }
        if [value.min, value.max, value.step]
            .into_iter()
            .flatten()
            .any(|number| !number.is_finite())
        {
            return Err("semantic numeric bounds must be finite");
        }
        if value.min.zip(value.max).is_some_and(|(min, max)| min > max) {
            return Err("semantic numeric minimum exceeds its maximum");
        }
        if value.step.is_some_and(|step| step <= 0.0) {
            return Err("semantic numeric step must be positive");
        }
    }
    if node.metadata.len() > MAX_METADATA_FIELDS {
        return Err("semantic metadata exceeds 32 fields");
    }
    if node.metadata.iter().any(|(key, value)| {
        key.is_empty()
            || key.len() > MAX_METADATA_KEY_BYTES
            || key.chars().any(char::is_control)
            || value.len() > MAX_METADATA_VALUE_BYTES
            || value.chars().any(char::is_control)
    }) {
        return Err("semantic metadata contains an invalid or oversized field");
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > MAX_ID_BYTES || id.chars().any(char::is_control) {
        return Err("semantic identifier must contain 1-256 bytes without control characters");
    }
    Ok(())
}

fn valid_diagnostic_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

fn push_diagnostic(
    pending: &mut PendingFrame,
    code: SemanticDiagnosticCode,
    node_id: Option<String>,
    message: &'static str,
) {
    if pending.diagnostics.len() < MAX_DIAGNOSTICS {
        pending.diagnostics.push(SemanticDiagnostic {
            code,
            node_id,
            message: message.to_owned(),
        });
    }
}

fn discard_invalid_relationships(pending: &mut PendingFrame) {
    let mut invalid = std::mem::take(&mut pending.invalid_ids);
    discard_missing_parents(pending, &mut invalid);
    discard_parent_cycles(pending, &mut invalid);
    discard_missing_parents(pending, &mut invalid);
    pending.nodes.retain(|id, _| !invalid.contains(id));
    pending.order.retain(|id| !invalid.contains(id));
}

/// Omit every node that sits on a parent cycle, in one linear pass.
///
/// Each walk stops at the first node an earlier walk already settled, so every
/// node is visited once rather than once per descendant.
fn discard_parent_cycles(pending: &mut PendingFrame, invalid: &mut BTreeSet<String>) {
    let order = std::mem::take(&mut pending.order);
    let position: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    let parent: Vec<Option<usize>> = order
        .iter()
        .map(|id| {
            pending
                .nodes
                .get(id)
                .and_then(|node| node.parent.as_deref())
                .and_then(|parent| position.get(parent).copied())
        })
        .collect();
    // 0 = unvisited, 1 = on the current walk, 2 = settled.
    let mut mark = vec![0_u8; order.len()];
    let mut path: Vec<usize> = Vec::new();
    for start in 0..order.len() {
        let mut current = Some(start);
        while let Some(index) = current {
            if mark[index] == 2 || invalid.contains(&order[index]) {
                break;
            }
            if mark[index] == 1 {
                let cycle_start = path.iter().position(|&node| node == index).unwrap_or(0);
                for &node in &path[cycle_start..] {
                    if invalid.insert(order[node].clone()) {
                        push_diagnostic(
                            pending,
                            SemanticDiagnosticCode::ParentCycle,
                            Some(order[node].clone()),
                            "semantic node in a parent cycle was omitted",
                        );
                    }
                }
                break;
            }
            mark[index] = 1;
            path.push(index);
            current = parent[index];
        }
        for node in path.drain(..) {
            mark[node] = 2;
        }
    }
    pending.order = order;
}

/// Omit every node whose parent is absent or omitted, transitively.
///
/// Diagnostics follow the same wave order a repeated scan would produce: all
/// nodes orphaned directly first, then their children, and so on.
fn discard_missing_parents(pending: &mut PendingFrame, invalid: &mut BTreeSet<String>) {
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut wave: Vec<String> = Vec::new();
    for id in &pending.order {
        if invalid.contains(id) {
            continue;
        }
        let Some(parent) = pending.nodes[id].parent.as_deref() else {
            continue;
        };
        if invalid.contains(parent) || !pending.nodes.contains_key(parent) {
            wave.push(id.clone());
        } else {
            children_of.entry(parent).or_default().push(id);
        }
    }
    let mut omitted: Vec<String> = Vec::new();
    while !wave.is_empty() {
        let mut next = Vec::new();
        for id in wave {
            if invalid.insert(id.clone()) {
                if let Some(children) = children_of.get(id.as_str()) {
                    next.extend(children.iter().map(|child| (*child).to_owned()));
                }
                omitted.push(id);
            }
        }
        wave = next;
    }
    drop(children_of);
    for id in omitted {
        push_diagnostic(
            pending,
            SemanticDiagnosticCode::MissingParent,
            Some(id),
            "semantic node whose parent was unavailable was omitted",
        );
    }
}

fn build_relationships(pending: &mut PendingFrame) -> Vec<String> {
    let relationships: Vec<_> = pending
        .order
        .iter()
        .filter_map(|id| {
            pending
                .nodes
                .get(id)
                .map(|node| (id.clone(), node.parent.clone()))
        })
        .collect();
    let mut roots = Vec::new();
    for (child, parent) in relationships {
        if let Some(parent) = parent {
            if let Some(parent_node) = pending.nodes.get_mut(&parent) {
                parent_node.children.push(child);
            }
        } else {
            roots.push(child);
        }
    }
    roots
}

fn push_sample(samples: &mut VecDeque<Duration>, sample: Duration) {
    if samples.len() == MAX_TIMING_SAMPLES {
        samples.pop_front();
    }
    samples.push_back(sample);
}

fn frame_stats_from_timings(timings: &TimingState) -> FrameStats {
    let (frame_interval_average_ms, frame_interval_max_ms) = timing_summary(&timings.intervals);
    let (prepaint_average_ms, prepaint_max_ms) = timing_summary(&timings.prepaint);
    let (root_paint_average_ms, root_paint_max_ms) = timing_summary(&timings.paint);
    FrameStats {
        frame_count: timings.frame_count,
        sample_count: u32::try_from(timings.intervals.len()).unwrap_or(u32::MAX),
        frame_interval_average_ms,
        frame_interval_max_ms,
        prepaint_average_ms,
        prepaint_max_ms,
        root_paint_average_ms,
        root_paint_max_ms,
        estimated_fps: if frame_interval_average_ms > 0.0 {
            1000.0 / frame_interval_average_ms
        } else {
            0.0
        },
    }
}

#[allow(clippy::cast_precision_loss)]
fn timing_summary(samples: &VecDeque<Duration>) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let total_ms = samples.iter().map(Duration::as_secs_f64).sum::<f64>() * 1000.0;
    let average_ms = total_ms / samples.len() as f64;
    let max_ms = samples
        .iter()
        .map(|duration| duration.as_secs_f64() * 1000.0)
        .fold(0.0, f64::max);
    (average_ms, max_ms)
}

fn normalize_level(level: &str) -> &'static str {
    match level.to_ascii_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "warn" | "warning" => "warn",
        "error" => "error",
        _ => "info",
    }
}

fn level_rank(level: &str) -> u8 {
    match normalize_level(level) {
        "trace" => 0,
        "debug" => 1,
        "warn" => 3,
        "error" => 4,
        _ => 2,
    }
}

pub(crate) fn rect_from_gpui(bounds: gpui::Bounds<gpui::Pixels>) -> Rect {
    Rect {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use gpui_mcp_protocol::{NodeState, Role, SemanticDiagnosticCode, UiNode};

    use super::{SharedState, TreeUpdate};

    fn node(id: &str, parent: Option<&str>) -> UiNode {
        UiNode {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            children: Vec::new(),
            role: Role::Generic,
            label: None,
            description: None,
            bounds: None,
            state: NodeState::default(),
            actions: Vec::new(),
            text: None,
            value: None,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn duplicate_ids_are_omitted_and_reported() {
        let state = SharedState::new();
        state.begin_frame();
        assert!(state.record(node("same", None)));
        assert!(!state.record(node("same", None)));
        state.finish_frame();

        let tree = state.tree();
        assert!(tree.nodes.is_empty());
        assert!(tree.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == SemanticDiagnosticCode::DuplicateId
                && diagnostic.node_id.as_deref() == Some("same")
        }));
    }

    #[test]
    fn missing_parents_and_cycles_are_rejected_without_rewriting_the_graph() {
        let state = SharedState::new();
        state.begin_frame();
        assert!(state.record(node("missing", Some("absent"))));
        assert!(state.record(node("a", Some("b"))));
        assert!(state.record(node("b", Some("a"))));
        state.finish_frame();

        let tree = state.tree();
        assert!(tree.nodes.is_empty());
        assert!(tree.roots.is_empty());
        assert!(
            tree.diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.code == SemanticDiagnosticCode::MissingParent })
        );
        assert!(
            tree.diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.code == SemanticDiagnosticCode::ParentCycle })
        );
    }

    #[test]
    fn invalid_relationships_report_each_omitted_node_once_in_a_stable_order() {
        let state = SharedState::new();
        state.begin_frame();
        for (id, parent) in [
            ("root", None),
            ("kept", Some("root")),
            ("orphan", Some("absent")),
            ("orphan-child", Some("orphan")),
            ("orphan-grandchild", Some("orphan-child")),
            ("a", Some("b")),
            ("b", Some("a")),
            ("cycle-child", Some("a")),
        ] {
            assert!(state.record(node(id, parent)));
        }
        state.finish_frame();

        let tree = state.tree();
        assert_eq!(tree.nodes.keys().collect::<Vec<_>>(), ["kept", "root"]);
        let omitted: Vec<(SemanticDiagnosticCode, &str)> = tree
            .diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.code, diagnostic.node_id.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            omitted,
            [
                (SemanticDiagnosticCode::MissingParent, "orphan"),
                (SemanticDiagnosticCode::MissingParent, "orphan-child"),
                (SemanticDiagnosticCode::MissingParent, "orphan-grandchild"),
                (SemanticDiagnosticCode::ParentCycle, "a"),
                (SemanticDiagnosticCode::ParentCycle, "b"),
                (SemanticDiagnosticCode::MissingParent, "cycle-child"),
            ]
        );
    }

    #[test]
    fn unchanged_semantics_do_not_advance_generation() {
        let state = SharedState::new();
        for _ in 0..2 {
            state.begin_frame();
            assert!(state.record(node("stable", None)));
            state.finish_frame();
        }
        assert_eq!(state.tree().generation, 1);
    }

    #[tokio::test]
    async fn tree_wait_wakes_with_the_changes_since_the_known_generation() -> Result<(), String> {
        let state = SharedState::new();
        state.publish_frame([node("kept", None)]);
        let waiter_state = state.clone();
        let waiter =
            tokio::spawn(
                async move { waiter_state.wait_for_tree(1, Duration::from_secs(1)).await },
            );
        tokio::task::yield_now().await;

        state.publish_frame([node("kept", None), node("added", Some("kept"))]);

        let update = waiter
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.message)?;
        let TreeUpdate::Delta(delta) = update else {
            return Err(format!("expected a delta, got {update:?}"));
        };
        assert_eq!(delta.base_generation, 1);
        assert_eq!(delta.generation, 2);
        let ids: Vec<&str> = delta.upserted.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["added", "kept"], "a new child changes its parent");
        Ok(())
    }

    #[test]
    fn deltas_chain_generations_and_rebuild_the_tree() {
        let state = SharedState::new();
        let stable = || node("stable", None);
        state.publish_frame([node("a", None), node("b", Some("a")), stable()]);
        let base = state.tree();
        state.publish_frame([node("a", None), node("c", Some("a")), stable()]);
        state.publish_frame([
            node("z", None),
            node("a", None),
            node("c", Some("a")),
            stable(),
        ]);

        let update = state.tree_since(base.generation);
        let TreeUpdate::Delta(delta) = update else {
            unreachable!("retained generations answer with a delta, got {update:?}");
        };
        assert_eq!(delta.removed, ["b"]);
        assert!(delta.upserted.iter().all(|node| node.id != "stable"));
        assert_eq!(
            delta.roots.as_deref(),
            Some(&["z".to_owned(), "a".to_owned(), "stable".to_owned()][..])
        );
        let mut patched = base;
        assert!(delta.apply(&mut patched));
        assert_eq!(patched, state.tree());

        assert!(matches!(
            state.tree_since(state.tree_generation()),
            TreeUpdate::Unchanged
        ));
        assert!(
            matches!(state.tree_since(999), TreeUpdate::Full(_)),
            "an unknown generation gets the whole tree"
        );
    }

    #[test]
    fn deltas_fall_back_to_the_full_tree_once_history_is_dropped() {
        let state = SharedState::new();
        state.publish_frame([node("a", None), node("b", None)]);
        let base = state.tree_generation();
        for index in 0..=super::MAX_CHANGE_RECORDS {
            let mut changed = node("a", None);
            changed.label = Some(index.to_string());
            state.publish_frame([changed, node("b", None)]);
        }
        assert!(matches!(state.tree_since(base), TreeUpdate::Full(_)));
        assert!(matches!(
            state.tree_since(state.tree_generation() - 1),
            TreeUpdate::Delta(_)
        ));
    }

    #[tokio::test]
    async fn frame_wait_wakes_for_frames_that_publish_nothing() -> Result<(), String> {
        let state = SharedState::new();
        state.publish_frame([node("stable", None)]);
        state.complete_frame(Duration::from_millis(2), Duration::from_millis(3));
        assert_eq!(state.tree().generation, 1);

        let waiter_state = state.clone();
        let waiter =
            tokio::spawn(
                async move { waiter_state.wait_for_frame(1, Duration::from_secs(1)).await },
            );
        tokio::task::yield_now().await;

        state.complete_frame(Duration::from_millis(4), Duration::from_millis(5));

        let observed = waiter
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.message)?;
        assert_eq!(observed.frame_count, 2);
        assert_eq!(observed.sample_count, 1);
        assert!((observed.prepaint_max_ms - 4.0).abs() < 0.01);
        assert!((observed.root_paint_average_ms - 4.0).abs() < 0.01);
        assert_eq!(state.tree().generation, 1);
        Ok(())
    }
}
