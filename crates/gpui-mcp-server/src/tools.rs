use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::io::Cursor;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use gpui_mcp_protocol::{
    BridgeResult, Capability, ContextResourceDescriptor, FrameReport, FrameStats, InputCommand,
    LiveDocumentSource, MouseButton, NodeAction, NodeState, Operation, Point, PointerCommand,
    PointerScrollDelta, Rect, Role, Screenshot, ScreenshotTarget, SemanticAction, UiNode, UiTree,
    ValueInfo,
};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use rmcp::{
    Json, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ContentBlock, CreateTaskResult, ErrorData, GetTaskParams, GetTaskResult, Implementation,
        ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
        ResourceContents, ServerCapabilities, ServerInfo, SubscribeRequestParams,
        SubscriptionFilter, UnsubscribeRequestParams, UpdateTaskParams,
    },
    service::{MaybeSendFuture, RequestContext, SubscriptionContext},
    task_manager::{TaskExit, TaskManager, TaskOptions},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::client::{AppInfo, BridgeClient, BridgeRegistry};
use crate::recording::{ArtifactStore, RecordingArtifact};

mod annotations;
mod application_commands;
mod connection;
mod diagnostics;
mod input;
mod live_document;
mod messages;
mod tree;
mod visual;

use messages::{MESSAGES_URI, watch_app_messages};

/// Tools that can block for seconds. For a client that declares the MCP tasks
/// extension they run as tasks, so the client is not held on one call; other
/// clients get the same result synchronously.
const TASK_TOOLS: [&str; 4] = [
    "wait_for_messages",
    "wait_for_element",
    "wait_for_state",
    "record_performance",
];

const MAX_TREE_SNAPSHOTS: usize = 32;
const MAX_IMAGE_SNAPSHOTS: usize = 8;
const MAX_WAIT_MS: u64 = 30_000;

#[derive(Default)]
struct SnapshotStore {
    trees: BTreeMap<String, UiTree>,
    images: BTreeMap<String, Screenshot>,
}

/// MCP tool suite backed by a lazily selected, authenticated GPUI endpoint.
#[derive(Clone)]
pub(crate) struct GpuiMcp {
    registry: BridgeRegistry,
    tool_router: ToolRouter<Self>,
    snapshots: Arc<RwLock<SnapshotStore>>,
    recording_task: Arc<Mutex<Option<RecordingTask>>>,
    recording_session: Arc<AtomicU64>,
    pointer: Arc<Mutex<Point>>,
    artifacts: ArtifactStore,
    target_transition: Arc<tokio::sync::Mutex<()>>,
    /// Legacy `resources/subscribe` watchers by URI.
    resource_watchers: Arc<Mutex<BTreeMap<String, CancellationToken>>>,
    /// Tool calls running as SEP-2663 tasks for clients that declare tasks.
    tasks: TaskManager,
}

struct RecordingTask {
    cancellation: CancellationToken,
    join: JoinHandle<Result<RecordingArtifact, String>>,
    session_id: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ObjectOutput {
    #[serde(flatten)]
    fields: BTreeMap<String, JsonValue>,
}

type Value = ObjectOutput;

#[derive(Debug, Deserialize, JsonSchema)]
struct ElementArgs {
    /// Stable semantic node identifier.
    id: String,
}

/// Selection and reply-shape arguments for `get_ui_tree`; unset, they return
/// the whole tree in full.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct TreeArgs {
    /// Return only this node and its descendants instead of the tree's roots.
    #[serde(default)]
    root: Option<String>,
    /// Levels to include below the starting nodes; `0` returns the starting nodes only.
    #[serde(default)]
    max_depth: Option<u16>,
    /// Omit nodes whose state is not visible.
    #[serde(default)]
    visible_only: bool,
    /// Return the generation, the node count, the roots, and an ordered id list, without the nodes.
    #[serde(default)]
    ids_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SelectAppArgs {
    /// Opaque target ID returned by `list_apps`.
    target_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindArgs {
    /// Optional label substring (case-insensitive unless exact is true).
    query: Option<String>,
    /// Optional semantic role filter.
    role: Option<Role>,
    /// Match the full label exactly, case-sensitively.
    #[serde(default)]
    exact: bool,
    /// Return visible nodes only.
    #[serde(default = "default_true")]
    visible_only: bool,
    /// Maximum matches, capped at 200.
    #[serde(default = "default_result_limit")]
    limit: u16,
    /// Return only the match count and the matching ids, without the elements.
    #[serde(default)]
    ids_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClickElementArgs {
    /// Stable semantic node identifier.
    id: String,
    /// `left`, `right`, or `middle`.
    #[serde(default)]
    button: MouseButton,
    /// Click count from 1 through 3.
    #[serde(default = "default_click_count")]
    count: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClickPointArgs {
    /// Window-relative logical x coordinate.
    x: f32,
    /// Window-relative logical y coordinate.
    y: f32,
    /// `left`, `right`, or `middle`.
    #[serde(default)]
    button: MouseButton,
    /// Click count from 1 through 3.
    #[serde(default = "default_click_count")]
    count: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PointerMoveArgs {
    /// Window-relative logical x coordinate.
    x: f32,
    /// Window-relative logical y coordinate.
    y: f32,
    /// Button held during the move, when this is part of a manually controlled drag.
    held_button: Option<MouseButton>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PointerButtonArgs {
    /// Window-relative logical x coordinate.
    x: f32,
    /// Window-relative logical y coordinate.
    y: f32,
    /// `left`, `right`, or `middle`.
    #[serde(default)]
    button: MouseButton,
    /// Click count from 1 through 3.
    #[serde(default = "default_click_count")]
    count: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DragElementArgs {
    /// Source semantic node identifier.
    from_id: String,
    /// Destination semantic node identifier.
    to_id: String,
    /// Gesture interpolation steps, from 1 through 120.
    #[serde(default = "default_drag_steps")]
    steps: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DragPointArgs {
    /// Source window-relative x coordinate.
    from_x: f32,
    /// Source window-relative y coordinate.
    from_y: f32,
    /// Destination window-relative x coordinate.
    to_x: f32,
    /// Destination window-relative y coordinate.
    to_y: f32,
    /// Gesture interpolation steps, from 1 through 120.
    #[serde(default = "default_drag_steps")]
    steps: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct KeyArgs {
    /// GPUI keystroke syntax, for example `ctrl-a`, `secondary-s`, or `enter`.
    keystroke: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeTextArgs {
    /// UTF-8 text to insert into the currently focused input.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetTextArgs {
    /// Editable semantic node identifier.
    id: String,
    /// Replacement UTF-8 text.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetValueArgs {
    /// Value-bearing semantic node identifier.
    id: String,
    /// Replacement value, validated against exposed numeric bounds when present.
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PerformActionArgs {
    /// Semantic node identifier from the latest tree.
    id: String,
    /// Accessibility action to perform, the way assistive technology requests it:
    /// `click`, `increment`, `decrement`, `expand`, `collapse`, or `set_value`
    /// with the new `value`. Unlike `click_element`, this needs no coordinates
    /// and no keyboard: the element's own AccessKit handler runs on the UI
    /// thread. Use it for controls whose value cannot be typed, such as a
    /// slider or a disclosure trigger.
    action: SemanticAction,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScrollArgs {
    /// Optional semantic node identifier. Its center is used when supplied.
    id: Option<String>,
    /// Window-relative x coordinate when `id` is omitted.
    x: Option<f32>,
    /// Window-relative y coordinate when `id` is omitted.
    y: Option<f32>,
    /// Horizontal logical-pixel delta; positive values scroll content right.
    #[serde(default)]
    delta_x: f32,
    /// Vertical logical-pixel delta; positive values scroll content down.
    delta_y: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScrollPointArgs {
    /// Window-relative logical x coordinate.
    x: f32,
    /// Window-relative logical y coordinate.
    y: f32,
    /// Horizontal logical-pixel delta; positive values scroll content right.
    #[serde(default)]
    delta_x: f32,
    /// Vertical logical-pixel delta; positive values scroll content down.
    #[serde(default)]
    delta_y: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RegionArgs {
    /// Left logical-pixel coordinate.
    x: f32,
    /// Top logical-pixel coordinate.
    y: f32,
    /// Logical-pixel width.
    width: f32,
    /// Logical-pixel height.
    height: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitElementArgs {
    /// Label query.
    query: String,
    /// Optional semantic role filter.
    role: Option<Role>,
    /// Match the full label exactly.
    #[serde(default)]
    exact: bool,
    /// Deadline in milliseconds, capped at 30000.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitStateArgs {
    /// Stable semantic node identifier.
    id: String,
    /// Expected visibility, if specified.
    visible: Option<bool>,
    /// Expected enabled state, if specified.
    enabled: Option<bool>,
    /// Expected focus state, if specified.
    focused: Option<bool>,
    /// Expected read-only state reported by AccessKit, if specified.
    read_only: Option<bool>,
    /// Expected checked state, if specified.
    checked: Option<bool>,
    /// Expected selected state, if specified.
    selected: Option<bool>,
    /// Expected expanded or collapsed state, if specified.
    expanded: Option<bool>,
    /// Deadline in milliseconds, capped at 30000.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HighlightArgs {
    /// One through 64 stable semantic node identifiers.
    ids: Vec<String>,
    /// Eight-digit `#RRGGBBAA` outline color.
    #[serde(default = "default_highlight_color")]
    color: String,
    /// Remove the outlines automatically this many milliseconds after they are
    /// first drawn, from 1 through 3600000.
    ttl_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SnapshotArgs {
    /// In-memory snapshot name: 1-64 ASCII letters, digits, `.`, `_`, or `-`.
    name: String,
}

/// Arguments for `load_ui_snapshot`; `save_ui_snapshot` takes the bare name
/// without the compact flag.
#[derive(Debug, Deserialize, JsonSchema)]
struct LoadSnapshotArgs {
    /// In-memory snapshot name: 1-64 ASCII letters, digits, `.`, `_`, or `-`.
    name: String,
    /// Return the generation, the node count, the roots, and an ordered id list, without the nodes.
    #[serde(default)]
    ids_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffSnapshotsArgs {
    /// Left/base in-memory snapshot name.
    left: String,
    /// Right/target in-memory snapshot name.
    right: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffCurrentArgs {
    /// Saved in-memory tree snapshot compared with the current UI.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CaptureNamedArgs {
    /// In-memory image snapshot name.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StartVideoRecordingArgs {
    /// Portable MP4 filename written inside the server-configured artifact directory.
    /// This is selected when recording starts so invalid destinations fail before capture.
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = r"^[A-Za-z0-9][A-Za-z0-9._-]*[.]mp4$")
    )]
    artifact_name: String,
    /// Replace an existing regular artifact with the same name. Defaults to false.
    /// Symlinks and non-regular files are always rejected.
    #[serde(default)]
    overwrite: bool,
    /// Draw the current GPUI window-relative pointer into every captured frame. This is enabled
    /// by default and works identically on Windows, Linux, and macOS without global OS cursor access.
    #[serde(default = "default_true")]
    include_pointer: bool,
    /// Target capture and encoding cadence (1..=30; default 30).
    #[serde(default = "default_video_fps")]
    #[schemars(range(min = 1, max = 30))]
    frames_per_second: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CompareImagesArgs {
    /// Left/base in-memory image snapshot name.
    left: String,
    /// Right/target in-memory image snapshot name.
    right: String,
    /// Per-channel difference threshold from 0 through 255.
    #[serde(default)]
    tolerance: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecordPerformanceArgs {
    /// Sampling interval in milliseconds, capped at 30000.
    duration_ms: u64,
    /// Return the embedded frame report without per-frame samples or the last frame's view draws.
    #[serde(default)]
    summary_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FrameReportArgs {
    /// Report frames completed after this `frame_count` token instead of after the last
    /// `mark_frames`.
    since_frame_count: Option<u64>,
    /// Maximum per-frame samples returned, most recent last, from 1 through 512. The summary
    /// and view activity cover every retained frame either way.
    #[serde(default = "default_frame_limit")]
    #[schemars(range(min = 1, max = 512))]
    frame_limit: u16,
    /// Return the summary and view activity without per-frame samples.
    #[serde(default)]
    summary_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogsArgs {
    /// Maximum entries, capped at 512.
    #[serde(default = "default_log_limit")]
    limit: u16,
    /// Optional minimum level: trace, debug, info, warn, or error.
    min_level: Option<String>,
}

#[tool_router(router = core_router)]
impl GpuiMcp {
    pub(crate) fn new(registry: BridgeRegistry, artifacts: ArtifactStore) -> Self {
        Self {
            registry,
            tool_router: Self::production_router(),
            snapshots: Arc::new(RwLock::new(SnapshotStore::default())),
            recording_task: Arc::new(Mutex::new(None)),
            recording_session: Arc::new(AtomicU64::new(0)),
            pointer: Arc::new(Mutex::new(Point::default())),
            artifacts,
            target_transition: Arc::new(tokio::sync::Mutex::new(())),
            resource_watchers: Arc::new(Mutex::new(BTreeMap::new())),
            tasks: TaskManager::new(),
        }
    }

    fn production_router() -> ToolRouter<Self> {
        let mut router = Self::core_router();
        router.merge(annotations::router());
        router.merge(connection::router());
        router.merge(application_commands::router());
        router.merge(diagnostics::router());
        router.merge(input::router());
        router.merge(live_document::router());
        router.merge(messages::router());
        router.merge(tree::router());
        router.merge(visual::router());
        router
    }

    async fn tree(&self) -> Result<UiTree, String> {
        let result = self.call(Operation::GetTree).await?;
        match result {
            BridgeResult::Tree(tree) => Ok(tree),
            _ => Err("bridge returned the wrong result for the semantic tree".to_owned()),
        }
    }

    async fn ack(&self, operation: Operation) -> Result<(), String> {
        match self.call(operation).await? {
            BridgeResult::Ack => Ok(()),
            _ => Err("bridge returned the wrong acknowledgement".to_owned()),
        }
    }

    async fn ack_after_frame(&self, operation: Operation) -> Result<(), String> {
        self.ack(operation).await?;
        self.settle_pending(Duration::from_secs(2)).await?;
        Ok(())
    }

    async fn dispatch_input(&self, command: InputCommand) -> Result<(), String> {
        self.ack_after_frame(Operation::Input { command }).await
    }

    async fn dispatch_pointer_input(&self, command: PointerCommand) -> Result<(), String> {
        let point = match &command {
            PointerCommand::MouseMove { point, .. }
            | PointerCommand::MouseDown { point, .. }
            | PointerCommand::MouseUp { point, .. }
            | PointerCommand::ScrollWheel { point, .. } => *point,
        };
        self.ack_after_frame(Operation::PointerInput { command })
            .await?;
        *self
            .pointer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = point;
        Ok(())
    }

    async fn current_pointer_location(&self) -> Result<Point, String> {
        match self.call(Operation::GetPointerLocation).await? {
            BridgeResult::PointerLocation(point) => Ok(point),
            _ => Err("bridge returned the wrong result for pointer location".to_owned()),
        }
    }

    async fn client(&self) -> Result<BridgeClient, String> {
        self.registry.client().await
    }

    async fn call(&self, operation: Operation) -> Result<BridgeResult, String> {
        self.client().await?.call(operation).await
    }

    async fn select_target(&self, target_id: &str) -> Result<AppInfo, String> {
        let _transition = self.target_transition.lock().await;
        if self
            .recording_task
            .lock()
            .map_err(|_| "recording state lock is poisoned".to_owned())?
            .is_some()
        {
            return Err(
                "cannot switch GPUI targets while recording or encoding video; stop the recording first"
                    .to_owned(),
            );
        }
        let selected = self.registry.select(target_id).await?;
        *self.snapshots.write().await = SnapshotStore::default();
        Ok(selected)
    }

    async fn click_at(&self, point: Point, button: MouseButton, count: u8) -> Result<(), String> {
        validate_pointer_point(point)?;
        if !(1..=3).contains(&count) {
            return Err("click count must be between 1 and 3".to_owned());
        }
        for click_count in 1..=count {
            self.dispatch_pointer_input(PointerCommand::MouseDown {
                point,
                button,
                click_count,
            })
            .await?;
            if let Err(error) = self
                .dispatch_pointer_input(PointerCommand::MouseUp {
                    point,
                    button,
                    click_count,
                })
                .await
            {
                return Err(self.release_error(point, button, click_count, error).await);
            }
        }
        Ok(())
    }

    async fn drag_between(&self, from: Point, to: Point, steps: u8) -> Result<(), String> {
        validate_pointer_point(from)?;
        validate_pointer_point(to)?;
        if !(1..=120).contains(&steps) {
            return Err("drag steps must be between 1 and 120".to_owned());
        }
        let distance = (to.x - from.x).hypot(to.y - from.y);
        if distance <= 2.0 {
            return Err("drag endpoints must be more than 2 logical pixels apart".to_owned());
        }

        self.dispatch_pointer_input(PointerCommand::MouseDown {
            point: from,
            button: MouseButton::Left,
            click_count: 1,
        })
        .await?;

        let mut last_point = from;
        for step in 1..=steps {
            let progress = f32::from(step) / f32::from(steps);
            let point = Point {
                x: from.x + (to.x - from.x) * progress,
                y: from.y + (to.y - from.y) * progress,
            };
            if let Err(error) = self
                .dispatch_pointer_input(PointerCommand::MouseMove {
                    point,
                    pressed_button: Some(MouseButton::Left),
                })
                .await
            {
                return Err(self
                    .release_error(last_point, MouseButton::Left, 1, error)
                    .await);
            }
            last_point = point;
        }

        self.dispatch_pointer_input(PointerCommand::MouseUp {
            point: to,
            button: MouseButton::Left,
            click_count: 1,
        })
        .await
    }

    async fn scroll_at(&self, point: Point, delta_x: f32, delta_y: f32) -> Result<(), String> {
        validate_pointer_point(point)?;
        validate_scroll_delta(delta_x, delta_y)?;
        self.dispatch_pointer_input(PointerCommand::ScrollWheel {
            point,
            delta: PointerScrollDelta::Pixels {
                delta_x: -delta_x,
                delta_y: -delta_y,
            },
        })
        .await
    }

    async fn release_error(
        &self,
        point: Point,
        button: MouseButton,
        click_count: u8,
        error: String,
    ) -> String {
        match self
            .dispatch_pointer_input(PointerCommand::MouseUp {
                point,
                button,
                click_count,
            })
            .await
        {
            Ok(()) => error,
            Err(release_error) => {
                format!("{error}; native mouse release also failed: {release_error}")
            }
        }
    }

    async fn element_point(&self, id: &str, action: NodeAction) -> Result<Point, String> {
        let node = self.element_with_action(id, action).await?;
        Ok(require_bounds(&node)?.center())
    }

    async fn element_with_action(&self, id: &str, action: NodeAction) -> Result<UiNode, String> {
        let tree = self.tree().await?;
        let node = get_node(&tree, id)?;
        if !node.state.visible || !node.state.enabled {
            return Err(format!("element {id:?} is not visible and enabled"));
        }
        if !node.actions.contains(&action) {
            return Err(format!(
                "element {id:?} does not support {action:?} (it advertises {:?})",
                node.actions
            ));
        }
        Ok(node.clone())
    }

    async fn wait_for_tree(&self, generation: u64, wait: Duration) -> Result<UiTree, String> {
        let timeout_ms = u64::try_from(wait.as_millis())
            .unwrap_or(MAX_WAIT_MS)
            .clamp(1, MAX_WAIT_MS);
        match self
            .call(Operation::WaitForTree {
                after_generation: generation,
                timeout_ms,
            })
            .await?
        {
            BridgeResult::Tree(tree) => Ok(tree),
            _ => Err("bridge returned the wrong result for semantic tree wait".to_owned()),
        }
    }

    async fn wait_for_frame(&self, frame_count: u64, wait: Duration) -> Result<FrameStats, String> {
        let timeout_ms = u64::try_from(wait.as_millis())
            .unwrap_or(MAX_WAIT_MS)
            .clamp(1, MAX_WAIT_MS);
        match self
            .call(Operation::WaitForFrame {
                after_frame_count: frame_count,
                timeout_ms,
            })
            .await?
        {
            BridgeResult::FrameStats(stats) => Ok(stats),
            _ => Err("bridge returned the wrong result for frame wait".to_owned()),
        }
    }

    /// Wait for the frames already pending to be drawn, and for no others.
    async fn settle_pending(&self, wait: Duration) -> Result<FrameStats, String> {
        settle_pending_frames(wait, |operation| self.call(operation)).await
    }

    /// Draw fresh frames even if nothing is pending, replaying cached views.
    async fn settle_requested_frames(&self, wait: Duration) -> Result<FrameStats, String> {
        settle_requested_frames(wait, |operation| self.call(operation)).await
    }

    async fn capture(&self, target: ScreenshotTarget) -> Result<Screenshot, String> {
        self.settle_requested_frames(Duration::from_secs(2)).await?;
        let client = self.client().await?;
        crate::capture::capture(&client, target).await
    }

    async fn stored_images(
        &self,
        left: &str,
        right: &str,
    ) -> Result<(Screenshot, Screenshot), String> {
        let snapshots = self.snapshots.read().await;
        let left = snapshots
            .images
            .get(left)
            .cloned()
            .ok_or_else(|| format!("image snapshot {left:?} was not found"))?;
        let right = snapshots
            .images
            .get(right)
            .cloned()
            .ok_or_else(|| format!("image snapshot {right:?} was not found"))?;
        Ok((left, right))
    }

    async fn frame_stats(&self) -> Result<FrameStats, String> {
        match self.call(Operation::GetFrameStats).await? {
            BridgeResult::FrameStats(stats) => Ok(stats),
            _ => Err("bridge returned the wrong result for frame statistics".to_owned()),
        }
    }

    async fn frame_report(
        &self,
        after_frame_count: Option<u64>,
        frame_limit: u16,
    ) -> Result<FrameReport, String> {
        match self
            .call(Operation::GetFrameReport {
                after_frame_count,
                frame_limit,
            })
            .await?
        {
            BridgeResult::FrameReport(report) => Ok(report),
            _ => Err("bridge returned the wrong result for the frame report".to_owned()),
        }
    }
}

/// Frames an input may cause before settlement stops waiting, so a window that
/// animates, and is therefore always pending, still settles.
const MAX_SETTLE_FRAMES: usize = 4;

/// Wait until the window has drawn every frame that was pending, without asking
/// for any frame itself.
///
/// Injected input invalidates only what the same input from the platform would,
/// so the frames it causes are exactly the frames it costs. Settling by
/// requesting frames instead would add frames the input never caused to every
/// measurement.
async fn settle_pending_frames<F, Fut>(wait: Duration, mut call: F) -> Result<FrameStats, String>
where
    F: FnMut(Operation) -> Fut,
    Fut: Future<Output = Result<BridgeResult, String>>,
{
    let timeout_ms = u64::try_from(wait.as_millis())
        .unwrap_or(MAX_WAIT_MS)
        .clamp(1, MAX_WAIT_MS);
    let mut completed = None;
    for _ in 0..MAX_SETTLE_FRAMES {
        let BridgeResult::PendingFrame(pending) = call(Operation::GetPendingFrame).await? else {
            return Err("bridge returned the wrong result for the pending frame".to_owned());
        };
        if !pending.pending {
            return Ok(pending.completed);
        }
        let BridgeResult::FrameStats(stats) = call(Operation::WaitForFrame {
            after_frame_count: pending.completed.frame_count,
            timeout_ms,
        })
        .await?
        else {
            return Err("bridge returned the wrong result for frame wait".to_owned());
        };
        completed = Some(stats);
    }
    completed.ok_or_else(|| "frame settlement did not observe a frame".to_owned())
}

/// Draw two fresh frames, whether or not anything is pending. Cached views that
/// were not notified replay, so these frames cost what an idle frame costs.
async fn settle_requested_frames<F, Fut>(wait: Duration, mut call: F) -> Result<FrameStats, String>
where
    F: FnMut(Operation) -> Fut,
    Fut: Future<Output = Result<BridgeResult, String>>,
{
    let timeout_ms = u64::try_from(wait.as_millis())
        .unwrap_or(MAX_WAIT_MS)
        .clamp(1, MAX_WAIT_MS);
    let mut completed = None;
    for _ in 0..2 {
        let BridgeResult::FrameStats(before_request) = call(Operation::RequestFrame).await? else {
            return Err(
                "bridge returned the wrong completed-frame token for a frame request".to_owned(),
            );
        };
        let BridgeResult::FrameStats(stats) = call(Operation::WaitForFrame {
            after_frame_count: before_request.frame_count,
            timeout_ms,
        })
        .await?
        else {
            return Err("bridge returned the wrong result for frame wait".to_owned());
        };
        completed = Some(stats);
    }
    completed.ok_or_else(|| "frame settlement did not request a frame".to_owned())
}

/// SEP-2549 cache hints on the results a peer negotiating protocol version
/// 2026-07-28 or newer requires them on.
///
/// `#[tool_handler]` stamps `tools/list` itself; the resource handlers below are
/// hand-written, so they carry the hints through this trait.
trait CacheHints: Sized {
    /// Borrow this result's `ttlMs` and `cacheScope` fields together.
    fn cache_hints(&mut self) -> (&mut Option<u64>, &mut Option<CacheScope>);

    /// Mark the result as live, per-application state that no peer may cache.
    ///
    /// Peers on older protocol versions keep the legacy shape, which has neither
    /// field.
    fn uncacheable(mut self, context: &RequestContext<RoleServer>) -> Self {
        if context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28)
        {
            let (ttl_ms, cache_scope) = self.cache_hints();
            *ttl_ms = Some(0);
            *cache_scope = Some(CacheScope::Private);
        }
        self
    }
}

impl CacheHints for ListResourcesResult {
    fn cache_hints(&mut self) -> (&mut Option<u64>, &mut Option<CacheScope>) {
        (&mut self.ttl_ms, &mut self.cache_scope)
    }
}

impl CacheHints for ListResourceTemplatesResult {
    fn cache_hints(&mut self) -> (&mut Option<u64>, &mut Option<CacheScope>) {
        (&mut self.ttl_ms, &mut self.cache_scope)
    }
}

impl CacheHints for ReadResourceResult {
    fn cache_hints(&mut self) -> (&mut Option<u64>, &mut Option<CacheScope>) {
        (&mut self.ttl_ms, &mut self.cache_scope)
    }
}

// `tool_handler` expands to dispatcher methods that never await; that shape
// belongs to the macro, so it cannot be fixed in this impl.
#[allow(clippy::unused_async_trait_impl)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for GpuiMcp {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .enable_tasks()
            .build();
        ServerInfo::new(capabilities)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Discover, inspect, and automate explicitly instrumented GPUI windows. Call list_apps first when more than one app may be running, then select_app with the desired target_id; a single live app is selected automatically. Selection persists for this MCP transport. Prefer semantic element tools over coordinates. Pointer actions use GPUI's native event pipeline; keyboard input uses GPUI directly. Screenshots and snapshots remain in memory, and all coordinates are logical pixels relative to the selected window. Video recording continuously captures raw native-window frames and encodes them directly into H.264/MP4 while recording; keep one MCP transport open for start_video_recording and stop_video_recording. Targets cannot be switched during recording. The optional pointer overlay reflects the same GPUI pointer state used for hover and clicks without reading or moving the global OS cursor. Artifact names are portable filenames inside the configured artifact directory; overwrite is opt-in. Annotate elements with annotate_elements (labels that follow the element; give them a ttl_ms or remove them when done). Apps with a chat box post messages for you: read them with read_messages or wait_for_messages, or subscribe to gpui://messages, and answer with send_message."
                    .to_owned(),
            )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut resources = vec![apps_resource(), messages_resource()];
        if let Ok(client) = self.registry.client().await
            && client
                .descriptor()
                .capabilities
                .supports(Capability::ContextResources)
        {
            match client.call(Operation::ListContextResources).await {
                Ok(BridgeResult::ContextResources(context_resources)) => {
                    resources.extend(context_resources.into_iter().map(mcp_resource));
                }
                Ok(_) => tracing::warn!(
                    target_id = %client.target_id(),
                    "bridge returned the wrong result while listing context resources"
                ),
                Err(error) => tracing::warn!(
                    %error,
                    target_id = %client.target_id(),
                    "could not list selected bridge context resources"
                ),
            }
        }
        Ok(ListResourcesResult::with_all_items(resources).uncacheable(&context))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + MaybeSendFuture + '_
    {
        std::future::ready(Ok(
            ListResourceTemplatesResult::with_all_items(Vec::new()).uncacheable(&context)
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri == "gpui://apps" {
            let apps = self
                .registry
                .list_apps()
                .await
                .map_err(|message| ErrorData::internal_error(message, None))?;
            let text = serde_json::to_string_pretty(&json!({ "apps": apps })).map_err(|_| {
                ErrorData::internal_error("could not encode the GPUI application registry", None)
            })?;
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, request.uri).with_mime_type("application/json"),
            ])
            .uncacheable(&context)
            .into());
        }
        if request.uri == MESSAGES_URI {
            let text = self
                .messages_resource_text()
                .await
                .map_err(|message| ErrorData::internal_error(message, None))?;
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, request.uri).with_mime_type("application/json"),
            ])
            .uncacheable(&context)
            .into());
        }
        let result = self
            .call(Operation::ReadContextResource {
                uri: request.uri.clone(),
            })
            .await
            .map_err(context_resource_error)?;
        let BridgeResult::ContextResource(resource) = result else {
            return Err(ErrorData::internal_error(
                "bridge returned the wrong result for a context resource",
                None,
            ));
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(resource.text, resource.descriptor.uri)
                .with_mime_type(resource.descriptor.mime_type),
        ])
        .uncacheable(&context)
        .into())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let as_task = TASK_TOOLS.contains(&request.name.as_ref())
            && context
                .client_capabilities()
                .is_some_and(|capabilities| capabilities.supports_tasks());
        if !as_task {
            let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
            return self.tool_router.call(call).await;
        }
        let server = self.clone();
        let options = TaskOptions::new()
            .with_poll_interval_ms(500)
            .with_status_message(format!("running {}", request.name));
        let task = self.tasks.spawn(options, move |task| {
            Box::pin(async move {
                let call =
                    rmcp::handler::server::tool::ToolCallContext::new(&server, request, context);
                tokio::select! {
                    () = task.cancelled() => Err(TaskExit::Cancelled),
                    response = server.tool_router.call(call) => match response {
                        Ok(CallToolResponse::Complete(result)) => Ok(result),
                        Ok(_) => Err(TaskExit::Error(ErrorData::internal_error(
                            "the tool did not complete",
                            None,
                        ))),
                        Err(error) => Err(TaskExit::Error(error)),
                    },
                }
            })
        });
        Ok(CallToolResponse::Task(CreateTaskResult::new(task)))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        Ok(GetTaskResult::new(self.tasks.get_task(&request.task_id)?))
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.tasks
            .update_task(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.tasks.cancel_task(&request.task_id)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut accepted = SubscriptionFilter::new();
        accepted.resource_subscriptions = requested.resource_subscriptions.as_ref().map(|uris| {
            uris.iter()
                .filter(|uri| *uri == MESSAGES_URI)
                .cloned()
                .collect()
        });
        Some(accepted)
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let wants_messages = context
            .accepted()
            .resource_subscriptions
            .as_ref()
            .is_some_and(|uris| uris.iter().any(|uri| uri == MESSAGES_URI));
        if !wants_messages {
            context.cancelled().await;
            return Ok(());
        }
        let cancellation = CancellationToken::new();
        let sink = context.sink().clone();
        let watch = watch_app_messages(self.registry.clone(), cancellation.clone(), || {
            let sink = sink.clone();
            async move { sink.notify_resource_updated(MESSAGES_URI).await.is_ok() }
        });
        tokio::select! {
            () = context.cancelled() => cancellation.cancel(),
            () = watch => {}
        }
        Ok(())
    }

    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        if request.uri != MESSAGES_URI {
            return Err(ErrorData::invalid_params(
                format!(
                    "{} does not support subscriptions; only {MESSAGES_URI} does",
                    request.uri
                ),
                None,
            ));
        }
        let cancellation = CancellationToken::new();
        let previous = self
            .resource_watchers
            .lock()
            .map_err(|_| ErrorData::internal_error("subscription state is poisoned", None))?
            .insert(request.uri.clone(), cancellation.clone());
        if let Some(previous) = previous {
            previous.cancel();
        }
        let peer = context.peer;
        let uri = request.uri;
        tokio::spawn(watch_app_messages(
            self.registry.clone(),
            cancellation,
            move || {
                let peer = peer.clone();
                let uri = uri.clone();
                async move {
                    peer.notify_resource_updated(
                        rmcp::model::ResourceUpdatedNotificationParam::new(uri),
                    )
                    .await
                    .is_ok()
                }
            },
        ));
        Ok(())
    }

    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        if let Some(watcher) = self
            .resource_watchers
            .lock()
            .map_err(|_| ErrorData::internal_error("subscription state is poisoned", None))?
            .remove(&request.uri)
        {
            watcher.cancel();
        }
        Ok(())
    }
}

fn messages_resource() -> Resource {
    Resource::new(MESSAGES_URI, "gpui-messages")
        .with_title("Messages from the application")
        .with_description(
            "The newest messages exchanged with the selected application, such as its chat box. Subscribe to be notified when the application posts one; reply with send_message",
        )
        .with_mime_type("application/json")
}

fn apps_resource() -> Resource {
    Resource::new("gpui://apps", "gpui-applications")
        .with_title("Live GPUI applications")
        .with_description(
            "Live instrumented GPUI windows and their non-secret target IDs for select_app",
        )
        .with_mime_type("application/json")
}

fn mcp_resource(descriptor: ContextResourceDescriptor) -> Resource {
    let mut resource =
        Resource::new(descriptor.uri, descriptor.name).with_mime_type(descriptor.mime_type);
    if let Some(title) = descriptor.title {
        resource = resource.with_title(title);
    }
    if let Some(description) = descriptor.description {
        resource = resource.with_description(description);
    }
    if let Some(size) = descriptor.size {
        resource = resource.with_size(size);
    }
    resource
}

fn context_resource_error(message: String) -> ErrorData {
    if message.starts_with("NotFound:") {
        ErrorData::resource_not_found(message, None)
    } else if message.starts_with("InvalidRequest:") {
        ErrorData::invalid_params(message, None)
    } else {
        ErrorData::internal_error(message, None)
    }
}

fn default_true() -> bool {
    true
}

fn default_result_limit() -> u16 {
    50
}

fn default_click_count() -> u8 {
    1
}

fn default_video_fps() -> u8 {
    30
}

fn default_drag_steps() -> u8 {
    12
}

fn default_timeout_ms() -> u64 {
    5_000
}

fn default_highlight_color() -> String {
    "#00A8FFFF".to_owned()
}

fn default_log_limit() -> u16 {
    100
}

fn default_frame_limit() -> u16 {
    64
}

fn validate_pointer_point(point: Point) -> Result<(), String> {
    if !point.is_valid() {
        return Err("native input coordinates must be finite".to_owned());
    }
    if point.x.abs() > 1_000_000.0 || point.y.abs() > 1_000_000.0 {
        return Err("native input coordinates exceed the safety bound".to_owned());
    }
    Ok(())
}

fn validate_scroll_delta(delta_x: f32, delta_y: f32) -> Result<(), String> {
    if !delta_x.is_finite() || !delta_y.is_finite() {
        return Err("scroll deltas must be finite".to_owned());
    }
    if delta_x.abs() > 100_000.0 || delta_y.abs() > 100_000.0 {
        return Err("scroll delta exceeds the safety bound".to_owned());
    }
    Ok(())
}

fn find_nodes<'a>(tree: &'a UiTree, args: &FindArgs) -> Vec<&'a UiNode> {
    let limit = usize::from(args.limit.clamp(1, 200));
    let query_lower = args.query.as_ref().map(|query| query.to_lowercase());
    tree.nodes
        .values()
        .filter(|node| !args.visible_only || node.state.visible)
        .filter(|node| args.role.is_none_or(|role| node.role == role))
        .filter(|node| {
            let Some(query) = args.query.as_deref() else {
                return true;
            };
            let label = node.label.as_deref().unwrap_or_default();
            if args.exact {
                label == query
            } else {
                label
                    .to_lowercase()
                    .contains(query_lower.as_deref().unwrap_or_default())
            }
        })
        .take(limit)
        .collect()
}

fn get_node<'a>(tree: &'a UiTree, id: &str) -> Result<&'a UiNode, String> {
    tree.nodes
        .get(id)
        .ok_or_else(|| format!("semantic element {id:?} was not found"))
}

fn require_bounds(node: &UiNode) -> Result<Rect, String> {
    node.bounds
        .filter(|bounds| bounds.is_valid())
        .ok_or_else(|| format!("element {:?} has no valid current bounds", node.id))
}

fn validate_value(input: &str, value: &ValueInfo) -> Result<(), String> {
    if value.min.is_none() && value.max.is_none() && value.step.is_none() {
        return Ok(());
    }
    let number: f64 = input
        .parse()
        .map_err(|_| "numeric value requires a finite number".to_owned())?;
    if !number.is_finite() {
        return Err("numeric value requires a finite number".to_owned());
    }
    if value.min.is_some_and(|min| number < min) {
        return Err(format!(
            "value is below the minimum {min}",
            min = value.min.unwrap_or_default()
        ));
    }
    if value.max.is_some_and(|max| number > max) {
        return Err(format!(
            "value is above the maximum {max}",
            max = value.max.unwrap_or_default()
        ));
    }
    Ok(())
}

fn validate_timeout(timeout_ms: u64) -> Result<(), String> {
    if timeout_ms == 0 || timeout_ms > MAX_WAIT_MS {
        return Err("timeout/duration must be between 1 and 30000 milliseconds".to_owned());
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "snapshot name must contain 1-64 ASCII letters, digits, '.', '_' or '-'".to_owned(),
        );
    }
    Ok(())
}

fn state_matches(state: &NodeState, args: &WaitStateArgs) -> bool {
    args.visible
        .is_none_or(|expected| state.visible == expected)
        && args
            .enabled
            .is_none_or(|expected| state.enabled == expected)
        && args
            .focused
            .is_none_or(|expected| state.focused == expected)
        && args
            .read_only
            .is_none_or(|expected| state.read_only == Some(expected))
        && args
            .checked
            .is_none_or(|expected| state.checked == Some(expected))
        && args
            .selected
            .is_none_or(|expected| state.selected == Some(expected))
        && args
            .expanded
            .is_none_or(|expected| state.expanded == Some(expected))
}

fn tree_diff(left: &UiTree, right: &UiTree) -> JsonValue {
    let left_ids: BTreeSet<_> = left.nodes.keys().cloned().collect();
    let right_ids: BTreeSet<_> = right.nodes.keys().cloned().collect();
    let added: Vec<_> = right_ids.difference(&left_ids).cloned().collect();
    let removed: Vec<_> = left_ids.difference(&right_ids).cloned().collect();
    let changed: Vec<_> = left_ids
        .intersection(&right_ids)
        .filter(|id| left.nodes.get(*id) != right.nodes.get(*id))
        .cloned()
        .collect();
    json!({
        "left_generation": left.generation,
        "right_generation": right.generation,
        "added": added,
        "removed": removed,
        "changed": changed,
        "identical": added.is_empty() && removed.is_empty() && changed.is_empty(),
    })
}

fn image_result(screenshot: Screenshot) -> CallToolResult {
    let metadata = json!({
        "mime_type": screenshot.mime_type,
        "width": screenshot.width,
        "height": screenshot.height,
    });
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(metadata.to_string()),
        ContentBlock::image(screenshot.base64_data, screenshot.mime_type),
    ]);
    result.structured_content = Some(metadata);
    result
}

// Counts are bounded to 64 megapixels above, so conversion to f64 is well
// inside the exact-integer range needed for deterministic comparison metrics.
#[allow(clippy::cast_precision_loss)]
fn compare_images(
    left: &Screenshot,
    right: &Screenshot,
    tolerance: u8,
) -> Result<(JsonValue, Screenshot), String> {
    let left_image = decode_image(left)?;
    let right_image = decode_image(right)?;
    if left_image.dimensions() != right_image.dimensions() {
        return Err(format!(
            "image dimensions differ: {}x{} versus {}x{}",
            left_image.width(),
            left_image.height(),
            right_image.width(),
            right_image.height()
        ));
    }
    let pixel_count = u64::from(left_image.width()) * u64::from(left_image.height());
    if pixel_count > 64_000_000 {
        return Err("image comparison exceeds the 64 megapixel safety bound".to_owned());
    }
    let mut changed_pixels = 0_u64;
    let mut absolute_difference = 0_u64;
    let mut diff = RgbaImage::new(left_image.width(), left_image.height());
    for (x, y, left_pixel) in left_image.enumerate_pixels() {
        let right_pixel = right_image.get_pixel(x, y);
        let differences = [
            left_pixel[0].abs_diff(right_pixel[0]),
            left_pixel[1].abs_diff(right_pixel[1]),
            left_pixel[2].abs_diff(right_pixel[2]),
            left_pixel[3].abs_diff(right_pixel[3]),
        ];
        absolute_difference += differences
            .iter()
            .map(|value| u64::from(*value))
            .sum::<u64>();
        let changed = differences.iter().any(|value| *value > tolerance);
        if changed {
            changed_pixels = changed_pixels.saturating_add(1);
            diff.put_pixel(x, y, Rgba([255, differences[1], differences[2], 255]));
        } else {
            let gray = u8::try_from(
                (u16::from(right_pixel[0]) + u16::from(right_pixel[1]) + u16::from(right_pixel[2]))
                    / 6,
            )
            .unwrap_or(u8::MAX);
            diff.put_pixel(x, y, Rgba([gray, gray, gray, 255]));
        }
    }
    let channel_total = pixel_count.saturating_mul(4).saturating_mul(255);
    let similarity = if channel_total == 0 {
        1.0
    } else {
        1.0 - absolute_difference as f64 / channel_total as f64
    };
    let metrics = json!({
        "width": left_image.width(),
        "height": left_image.height(),
        "pixel_count": pixel_count,
        "changed_pixels": changed_pixels,
        "changed_ratio": if pixel_count == 0 { 0.0 } else { changed_pixels as f64 / pixel_count as f64 },
        "mean_absolute_channel_difference": if pixel_count == 0 { 0.0 } else { absolute_difference as f64 / (pixel_count * 4) as f64 },
        "similarity": similarity,
        "tolerance": tolerance,
    });
    let screenshot = encode_image(diff)?;
    Ok((metrics, screenshot))
}

fn decode_image(screenshot: &Screenshot) -> Result<RgbaImage, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&screenshot.base64_data)
        .map_err(|_| "stored screenshot base64 is invalid".to_owned())?;
    image::load_from_memory(&bytes)
        .map(DynamicImage::into_rgba8)
        .map_err(|_| "stored screenshot PNG is invalid".to_owned())
}

fn encode_image(image: RgbaImage) -> Result<Screenshot, String> {
    let width = image.width();
    let height = image.height();
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|_| "could not encode the screenshot diff".to_owned())?;
    Ok(Screenshot {
        mime_type: "image/png".to_owned(),
        base64_data: base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()),
        width,
        height,
    })
}

/// Mean application render work per frame: GPUI's whole draw less the bridge's
/// own observation, or prepaint plus paint from a bridge that reports no draw.
fn average_render_work_ms(stats: &FrameStats) -> f64 {
    if stats.draw_average_ms > 0.0 {
        (stats.draw_average_ms - stats.bridge_average_ms).max(0.0)
    } else {
        stats.prepaint_average_ms + stats.root_paint_average_ms
    }
}

fn performance_assessment(stats: &FrameStats) -> &'static str {
    let render_work_ms = average_render_work_ms(stats);
    if stats.sample_count == 0 {
        "no frame samples have been observed"
    } else if render_work_ms <= 16.67 {
        "average measured render work is within a 60 FPS frame budget"
    } else if render_work_ms <= 33.33 {
        "average measured render work is within a 30 FPS frame budget but above a 60 FPS budget"
    } else {
        "average measured render work is above a 30 FPS frame budget"
    }
}

fn ack_json(action: &'static str) -> Json<Value> {
    object_output(json!({ "ok": true, "action": action }))
}

fn object_output(value: JsonValue) -> Json<ObjectOutput> {
    let fields = match value {
        JsonValue::Object(fields) => fields.into_iter().collect(),
        other => BTreeMap::from([("value".to_owned(), other)]),
    };
    Json(ObjectOutput { fields })
}

/// A structured tool result built from one `serde_json::Value`.
///
/// Returning `Json<T>` instead makes rmcp convert the value into a second
/// `Value` before encoding its text, which a large tree pays for in full.
fn serialized_result(value: &impl Serialize) -> Result<CallToolResult, String> {
    let structured = serde_json::to_value(value).map_err(encode_error)?;
    Ok(CallToolResult::structured(structured))
}

/// The `get_ui_tree` reply for `args`, or the whole tree when it selects all of it.
///
/// The compact `ids_only` reply is shaped from the same selected-or-whole tree,
/// so its count, roots and ids describe exactly the nodes the full reply would
/// have carried.
fn tree_result(tree: &UiTree, args: &TreeArgs) -> Result<CallToolResult, String> {
    let selected = select_tree(tree, args)?;
    let tree = selected.as_ref().unwrap_or(tree);
    if args.ids_only {
        return serialized_result(&tree_ids_reply(tree));
    }
    serialized_result(tree)
}

/// The `find_elements` reply for `nodes`: every match, or just the count and
/// their ids in the order the full reply lists the elements.
fn find_reply(nodes: &[&UiNode], ids_only: bool) -> JsonValue {
    if ids_only {
        json!({
            "count": nodes.len(),
            "ids": nodes.iter().map(|node| node.id.as_str()).collect::<Vec<_>>(),
        })
    } else {
        json!({ "count": nodes.len(), "elements": nodes })
    }
}

/// The compact `get_ui_tree`/`load_ui_snapshot` reply: the generation, the node
/// count, the starting ids and every selected node's id, in the order the full
/// reply's `nodes` map lists them.
fn tree_ids_reply(tree: &UiTree) -> JsonValue {
    json!({
        "generation": tree.generation,
        "node_count": tree.nodes.len(),
        "roots": tree.roots,
        "ids": tree.nodes.keys().collect::<Vec<_>>(),
    })
}

/// The part of `tree` that `args` asks for, or `None` when it asks for all of it.
///
/// A returned node keeps its full `children` list, so a child left out by the
/// depth limit or the visibility filter is named but absent from `nodes`.
fn select_tree(tree: &UiTree, args: &TreeArgs) -> Result<Option<UiTree>, String> {
    if args.root.is_none() && args.max_depth.is_none() && !args.visible_only {
        return Ok(None);
    }
    let starts = match &args.root {
        Some(root) => vec![get_node(tree, root)?.id.clone()],
        None => tree.roots.clone(),
    };
    let mut nodes = BTreeMap::new();
    // A well-formed tree is a forest, so nothing is reached twice and this set
    // costs one lookup per node. It is what terminates the walk on a malformed
    // one: a `children` cycle, or a child named by two parents, would otherwise
    // be pushed again for every pass through it, without bound when no depth
    // limit was given.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut stack: Vec<(&str, u16)> = starts.iter().rev().map(|id| (id.as_str(), 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(node) = tree.nodes.get(id) else {
            continue;
        };
        // Saturating: an unlimited depth still walks a chain deeper than u16::MAX.
        if args.max_depth.is_none_or(|max| depth < max) {
            stack.extend(
                node.children
                    .iter()
                    .rev()
                    .map(|child| (child.as_str(), depth.saturating_add(1))),
            );
        }
        // Visibility filters the node, not the traversal: a hidden container's
        // visible descendant is still selected on its own.
        if !args.visible_only || node.state.visible {
            nodes.insert(node.id.clone(), node.clone());
        }
    }
    Ok(Some(UiTree {
        generation: tree.generation,
        roots: starts,
        nodes,
        diagnostics: tree.diagnostics.clone(),
    }))
}

fn encode_error(_error: serde_json::Error) -> String {
    "could not encode the tool result".to_owned()
}

fn map_wait_error(error: String, subject: &str) -> String {
    if error.starts_with("Timeout:") {
        format!("timed out waiting for the {subject}")
    } else {
        error
    }
}

#[cfg(test)]
fn default_result_limit_for_test() -> u16 {
    default_result_limit()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use gpui_mcp_protocol::{
        BridgeResult, FrameStats, NodeState, Operation, PendingFrame, SemanticDiagnostic,
        SemanticDiagnosticCode, UiNode,
    };
    use serde_json::json;

    use super::{
        FindArgs, MAX_SETTLE_FRAMES, Role, StartVideoRecordingArgs, TreeArgs, UiTree,
        WaitStateArgs, default_result_limit_for_test, find_nodes, find_reply, select_tree,
        settle_pending_frames, settle_requested_frames, state_matches, tree_diff, tree_result,
    };

    fn stats(frame_count: u64) -> BridgeResult {
        BridgeResult::FrameStats(FrameStats {
            frame_count,
            ..FrameStats::default()
        })
    }

    fn pending(pending: bool, frame_count: u64) -> BridgeResult {
        BridgeResult::PendingFrame(PendingFrame {
            pending,
            completed: FrameStats {
                frame_count,
                ..FrameStats::default()
            },
        })
    }

    /// Records every operation and answers from a script.
    #[derive(Clone)]
    struct Script {
        operations: Arc<Mutex<Vec<Operation>>>,
        responses: Arc<Mutex<VecDeque<BridgeResult>>>,
    }

    impl Script {
        fn new(responses: impl IntoIterator<Item = BridgeResult>) -> Self {
            Self {
                operations: Arc::new(Mutex::new(Vec::new())),
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            }
        }

        fn call(
            &self,
            operation: Operation,
        ) -> impl std::future::Future<Output = Result<BridgeResult, String>> + use<> {
            self.operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(operation);
            let response = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| "test response queue exhausted".to_owned());
            async move { response }
        }

        fn operations(&self) -> Vec<Operation> {
            self.operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[tokio::test]
    async fn requested_settlement_waits_from_each_request_token() -> Result<(), String> {
        let script = Script::new([stats(11), stats(12), stats(14), stats(15)]);
        let settled =
            settle_requested_frames(Duration::from_secs(2), |operation| script.call(operation))
                .await?;

        assert_eq!(settled.frame_count, 15);
        let operations = script.operations();
        assert!(matches!(operations[0], Operation::RequestFrame));
        assert!(matches!(
            operations[1],
            Operation::WaitForFrame {
                after_frame_count: 11,
                ..
            }
        ));
        assert!(matches!(operations[2], Operation::RequestFrame));
        assert!(matches!(
            operations[3],
            Operation::WaitForFrame {
                after_frame_count: 14,
                ..
            }
        ));
        assert!(
            operations
                .iter()
                .all(|operation| !matches!(operation, Operation::Refresh)),
            "settling must never discard every cached view"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_settlement_draws_nothing_when_nothing_is_pending() -> Result<(), String> {
        let script = Script::new([pending(false, 7)]);
        let settled =
            settle_pending_frames(Duration::from_secs(2), |operation| script.call(operation))
                .await?;

        assert_eq!(settled.frame_count, 7);
        assert!(matches!(
            script.operations().as_slice(),
            [Operation::GetPendingFrame]
        ));
        Ok(())
    }

    #[tokio::test]
    async fn pending_settlement_waits_for_each_pending_frame() -> Result<(), String> {
        let script = Script::new([
            pending(true, 7),
            stats(8),
            pending(true, 8),
            stats(9),
            pending(false, 9),
        ]);
        let settled =
            settle_pending_frames(Duration::from_secs(2), |operation| script.call(operation))
                .await?;

        assert_eq!(settled.frame_count, 9);
        let operations = script.operations();
        assert!(matches!(
            operations[1],
            Operation::WaitForFrame {
                after_frame_count: 7,
                ..
            }
        ));
        assert!(matches!(
            operations[3],
            Operation::WaitForFrame {
                after_frame_count: 8,
                ..
            }
        ));
        assert!(
            operations.iter().all(|operation| !matches!(
                operation,
                Operation::Refresh | Operation::RequestFrame
            ))
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_settlement_gives_up_on_a_window_that_always_animates() -> Result<(), String> {
        let script = Script::new(
            (0..MAX_SETTLE_FRAMES as u64)
                .flat_map(|frame| [pending(true, frame), stats(frame + 1)]),
        );
        let settled =
            settle_pending_frames(Duration::from_secs(2), |operation| script.call(operation))
                .await?;

        assert_eq!(settled.frame_count, MAX_SETTLE_FRAMES as u64);
        assert_eq!(script.operations().len(), MAX_SETTLE_FRAMES * 2);
        Ok(())
    }

    #[test]
    fn find_is_case_insensitive_by_default() {
        let node = UiNode {
            id: "save".to_owned(),
            parent: None,
            children: Vec::new(),
            role: Role::Button,
            label: Some("Save Document".to_owned()),
            description: None,
            bounds: None,
            state: NodeState::default(),
            actions: Vec::new(),
            text: None,
            value: None,
            metadata: BTreeMap::new(),
        };
        let tree = UiTree {
            generation: 1,
            roots: vec!["save".to_owned()],
            nodes: BTreeMap::from([("save".to_owned(), node)]),
            diagnostics: Vec::new(),
        };
        let found = find_nodes(
            &tree,
            &FindArgs {
                query: Some("document".to_owned()),
                role: Some(Role::Button),
                exact: false,
                visible_only: true,
                limit: default_result_limit_for_test(),
                ids_only: false,
            },
        );
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn state_wait_matches_expanded_disclosures() {
        let state = NodeState {
            expanded: Some(true),
            ..NodeState::default()
        };
        let expanded = WaitStateArgs {
            id: "details".to_owned(),
            visible: None,
            enabled: None,
            focused: None,
            read_only: None,
            checked: None,
            selected: None,
            expanded: Some(true),
            timeout_ms: 1_000,
        };
        assert!(state_matches(&state, &expanded));

        let collapsed = WaitStateArgs {
            expanded: Some(false),
            ..expanded
        };
        assert!(!state_matches(&state, &collapsed));
    }

    #[test]
    fn read_only_waits_require_a_known_matching_state() -> Result<(), String> {
        for expected in [true, false] {
            let args = serde_json::from_value::<WaitStateArgs>(
                json!({ "id": "input", "read_only": expected }),
            )
            .map_err(|error| error.to_string())?;
            for reported in [None, Some(true), Some(false)] {
                let state = NodeState {
                    read_only: reported,
                    ..NodeState::default()
                };
                assert_eq!(state_matches(&state, &args), reported == Some(expected));
            }
        }
        Ok(())
    }

    #[test]
    fn tree_diff_reports_added_ids() {
        let left = UiTree::default();
        let right = UiTree {
            generation: 1,
            roots: vec!["new".to_owned()],
            nodes: BTreeMap::from([(
                "new".to_owned(),
                UiNode {
                    id: "new".to_owned(),
                    parent: None,
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
                },
            )]),
            diagnostics: Vec::new(),
        };
        assert_eq!(tree_diff(&left, &right)["added"], json!(["new"]));
    }

    fn plain_node(id: &str, parent: Option<&str>, children: &[&str], visible: bool) -> UiNode {
        UiNode {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            children: children.iter().map(|child| (*child).to_owned()).collect(),
            role: Role::Group,
            label: None,
            description: None,
            bounds: None,
            state: NodeState {
                visible,
                ..NodeState::default()
            },
            actions: Vec::new(),
            text: None,
            value: None,
            metadata: BTreeMap::new(),
        }
    }

    /// A visible chain, and an invisible container with a visible child in it.
    fn selection_fixture() -> UiTree {
        let nodes = [
            plain_node("app", None, &["panel", "hidden"], true),
            plain_node("panel", Some("app"), &["row"], true),
            plain_node("row", Some("panel"), &[], true),
            plain_node("hidden", Some("app"), &["inside"], false),
            plain_node("inside", Some("hidden"), &[], true),
        ];
        UiTree {
            generation: 3,
            roots: vec!["app".to_owned()],
            nodes: nodes
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect(),
            diagnostics: vec![SemanticDiagnostic {
                code: SemanticDiagnosticCode::DuplicateId,
                node_id: Some("app".to_owned()),
                message: "kept so the selection test can see it survive".to_owned(),
            }],
        }
    }

    fn args(root: Option<&str>, max_depth: Option<u16>, visible_only: bool) -> TreeArgs {
        TreeArgs {
            root: root.map(str::to_owned),
            max_depth,
            visible_only,
            ids_only: false,
        }
    }

    /// The ids the selection returns, or the whole tree's when it selects all of it.
    fn selected_ids(tree: &UiTree, args: &TreeArgs) -> Result<Vec<String>, String> {
        Ok(match select_tree(tree, args)? {
            Some(selected) => selected.nodes.into_keys().collect(),
            None => tree.nodes.keys().cloned().collect(),
        })
    }

    #[test]
    fn tree_selection_limits_root_depth_and_visibility() -> Result<(), String> {
        let tree = selection_fixture();

        assert_eq!(
            selected_ids(&tree, &args(Some("panel"), None, false))?,
            ["panel", "row"],
            "root alone returns that node and its descendants, not its ancestors"
        );
        assert_eq!(
            selected_ids(&tree, &args(None, Some(0), false))?,
            ["app"],
            "max_depth 0 returns the starting nodes only"
        );
        assert_eq!(
            selected_ids(&tree, &args(None, Some(1), false))?,
            ["app", "hidden", "panel"],
            "max_depth 1 adds the starting nodes' children"
        );
        assert_eq!(
            selected_ids(&tree, &args(None, None, true))?,
            ["app", "inside", "panel", "row"],
            "visible_only omits the invisible node but judges its descendant on its own"
        );
        assert_eq!(
            selected_ids(&tree, &args(Some("app"), Some(1), true))?,
            ["app", "panel"],
            "root, max_depth and visible_only combine, depth counted from root"
        );
        Ok(())
    }

    #[test]
    fn tree_selection_walks_a_chain_deeper_than_its_level_counter() -> Result<(), String> {
        // An unlimited depth walks levels past u16::MAX; the counter must saturate
        // rather than overflow. `visible_only` makes this a real selection, not the
        // no-arguments early return.
        let depth = usize::from(u16::MAX) + 2;
        let mut nodes = BTreeMap::new();
        for level in 0..depth {
            let id = format!("level-{level}");
            let parent = level.checked_sub(1).map(|above| format!("level-{above}"));
            let child = (level + 1 < depth).then(|| format!("level-{}", level + 1));
            nodes.insert(
                id.clone(),
                plain_node(
                    &id,
                    parent.as_deref(),
                    &child.iter().map(String::as_str).collect::<Vec<_>>(),
                    true,
                ),
            );
        }
        let tree = UiTree {
            generation: 1,
            roots: vec!["level-0".to_owned()],
            nodes,
            diagnostics: Vec::new(),
        };

        assert_eq!(
            selected_ids(&tree, &args(None, None, true))?.len(),
            depth,
            "every level of an unbounded chain is selected"
        );
        Ok(())
    }

    #[test]
    fn tree_selection_counts_depth_from_the_requested_root() -> Result<(), String> {
        // "panel" sits one level below the tree's root, so an implementation that
        // measured depth from the whole tree instead of the requested start would
        // stop one level early here.
        let nodes = [
            plain_node("app", None, &["panel"], true),
            plain_node("panel", Some("app"), &["row"], true),
            plain_node("row", Some("panel"), &["cell"], true),
            plain_node("cell", Some("row"), &[], true),
        ];
        let tree = UiTree {
            generation: 1,
            roots: vec!["app".to_owned()],
            nodes: nodes
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect(),
            diagnostics: Vec::new(),
        };

        assert_eq!(
            selected_ids(&tree, &args(Some("panel"), Some(0), false))?,
            ["panel"],
            "depth 0 at a non-root start returns the start alone, not the whole tree"
        );
        assert_eq!(
            selected_ids(&tree, &args(Some("panel"), Some(1), false))?,
            ["panel", "row"],
            "the start's own depth does not count against its max_depth"
        );
        assert_eq!(
            selected_ids(&tree, &args(Some("panel"), Some(2), false))?,
            ["cell", "panel", "row"],
            "max_depth 2 reaches the grandchild of the start"
        );
        Ok(())
    }

    #[test]
    fn tree_selection_terminates_on_a_cyclic_child_list() -> Result<(), String> {
        // The bridge's tree builder drops cycles and duplicate ids, so this is a
        // malformed tree the tool never sees in practice; without the visited set
        // the walk below would push "loop" again on every pass and never end, with
        // max_depth absent, which is what `root` alone leaves it as.
        let nodes = [
            plain_node("loop", None, &["loop", "tail"], true),
            plain_node("tail", Some("loop"), &["loop"], true),
        ];
        let tree = UiTree {
            generation: 1,
            roots: vec!["loop".to_owned()],
            nodes: nodes
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect(),
            diagnostics: Vec::new(),
        };

        let selected = select_tree(&tree, &args(Some("loop"), None, false))?
            .ok_or("a root argument selects a subtree")?;
        assert_eq!(
            selected.nodes.keys().collect::<Vec<_>>(),
            ["loop", "tail"],
            "a self-referencing node is returned once, not walked again"
        );
        assert_eq!(selected.roots, ["loop"]);
        Ok(())
    }

    #[test]
    fn tree_selection_names_the_children_it_leaves_out() -> Result<(), String> {
        let tree = selection_fixture();

        let cut = select_tree(&tree, &args(Some("app"), Some(0), false))?
            .ok_or("a subtree is selected")?;
        assert_eq!(
            cut.nodes["app"].children,
            ["panel", "hidden"],
            "a node keeps its full child list, so a caller can see what the depth limit cut"
        );
        assert!(!cut.nodes.contains_key("panel"));
        assert_eq!(
            cut.roots,
            ["app"],
            "roots names the requested starting node"
        );
        assert_eq!(cut.generation, 3, "the frame generation survives selection");
        assert_eq!(
            cut.diagnostics.len(),
            1,
            "the full tree's diagnostics survive selection"
        );

        let error = select_tree(&tree, &args(Some("missing"), None, false))
            .err()
            .ok_or("an id that is not in the tree is an error")?;
        assert!(
            error.contains("missing"),
            "the error names the requested id, got {error:?}"
        );
        Ok(())
    }

    /// The reply `get_ui_tree` produced before it built its own `CallToolResult`:
    /// rmcp converts `Json<ObjectOutput>` through `into_call_tool_result`.
    fn legacy_tree_result(tree: &UiTree) -> Result<super::CallToolResult, String> {
        use rmcp::handler::server::tool::IntoCallToolResult as _;

        let value = serde_json::to_value(tree).map_err(|error| error.to_string())?;
        match super::object_output(value)
            .into_call_tool_result()
            .map_err(|error| error.to_string())?
        {
            rmcp::model::CallToolResponse::Complete(result) => Ok(result),
            _ => Err("a Json tool result completes the call".to_owned()),
        }
    }

    fn serialized_reply(result: &super::CallToolResult) -> Result<serde_json::Value, String> {
        serde_json::to_value(result).map_err(|error| error.to_string())
    }

    #[test]
    fn tree_reply_without_arguments_is_the_whole_tree_byte_for_byte() -> Result<(), String> {
        let tree = selection_fixture();
        assert!(
            select_tree(&tree, &TreeArgs::default())?.is_none(),
            "no arguments must not copy the tree at all"
        );

        let current = serialized_reply(&tree_result(&tree, &TreeArgs::default())?)?;
        assert_eq!(
            current,
            serialized_reply(&legacy_tree_result(&tree)?)?,
            "the whole reply, content text and structured content alike, is unchanged"
        );

        let filtered = serialized_reply(&tree_result(&tree, &args(None, None, true))?)?;
        assert_ne!(
            filtered, current,
            "an argument that selects less must change the reply"
        );
        Ok(())
    }

    #[test]
    fn find_reply_ids_only_lists_the_same_matches_in_the_same_order() -> Result<(), String> {
        let nodes = [
            plain_node("beta", None, &[], true),
            plain_node("alpha", None, &[], true),
        ];
        let tree = UiTree {
            generation: 1,
            roots: vec!["alpha".to_owned(), "beta".to_owned()],
            nodes: nodes
                .into_iter()
                .map(|node| (node.id.clone(), node))
                .collect(),
            diagnostics: Vec::new(),
        };
        let found = find_nodes(
            &tree,
            &FindArgs {
                query: None,
                role: None,
                exact: false,
                visible_only: true,
                limit: 100,
                ids_only: true,
            },
        );
        assert_eq!(
            found
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"],
            "matches come out in the tree's own order"
        );

        let full = find_reply(&found, false);
        let compact = find_reply(&found, true);
        let elements = full["elements"]
            .as_array()
            .ok_or("the full reply lists elements")?;
        let element_ids: Vec<&str> = elements
            .iter()
            .map(|element| {
                element["id"]
                    .as_str()
                    .ok_or_else(|| "an element carries an id".to_owned())
            })
            .collect::<Result<_, String>>()?;
        assert_eq!(compact["count"], full["count"]);
        assert_eq!(
            compact["ids"],
            json!(element_ids),
            "the compact ids are the full reply's element ids in the same order"
        );
        assert!(compact.get("elements").is_none());
        Ok(())
    }

    #[test]
    fn tree_ids_only_reply_describes_the_same_selection() -> Result<(), String> {
        let tree = selection_fixture();

        let visible = TreeArgs {
            ids_only: true,
            ..args(None, None, true)
        };
        let reply = serialized_reply(&tree_result(&tree, &visible)?)?;
        assert_eq!(
            reply.get("structuredContent").cloned(),
            Some(json!({
                "generation": 3,
                "node_count": 4,
                "roots": ["app"],
                "ids": ["app", "inside", "panel", "row"],
            })),
            "the compact reply counts and names exactly the selected nodes"
        );

        let rooted = TreeArgs {
            root: Some("panel".to_owned()),
            ids_only: true,
            ..TreeArgs::default()
        };
        let reply = serialized_reply(&tree_result(&tree, &rooted)?)?;
        assert_eq!(
            reply.get("structuredContent").cloned(),
            Some(json!({
                "generation": 3,
                "node_count": 2,
                "roots": ["panel"],
                "ids": ["panel", "row"],
            })),
            "a root argument shapes the compact reply from that subtree's nodes"
        );

        let whole = TreeArgs {
            ids_only: true,
            ..TreeArgs::default()
        };
        let reply = serialized_reply(&tree_result(&tree, &whole)?)?;
        assert_eq!(
            reply.get("structuredContent").cloned(),
            Some(json!({
                "generation": 3,
                "node_count": 5,
                "roots": ["app"],
                "ids": ["app", "hidden", "inside", "panel", "row"],
            })),
            "without a selection the compact reply lists the whole tree, hidden nodes included"
        );
        Ok(())
    }

    /// A wide tree: one list node whose `rows` children each hold a label child.
    fn stress_tree(rows: usize) -> UiTree {
        let row_ids: Vec<String> = (0..rows).map(|row| format!("stress-row-{row}")).collect();
        let mut nodes = BTreeMap::from([(
            "stress-list".to_owned(),
            plain_node(
                "stress-list",
                None,
                &row_ids.iter().map(String::as_str).collect::<Vec<_>>(),
                true,
            ),
        )]);
        for (row, row_id) in row_ids.into_iter().enumerate() {
            let label_id = format!("{row_id}/label");
            let mut node = plain_node(&row_id, Some("stress-list"), &[&label_id], row % 7 != 0);
            node.label = Some(format!("Row {row}"));
            node.bounds = Some(super::Rect {
                x: 32.0,
                y: 313.333_34,
                width: 576.0,
                height: 2.0,
            });
            node.metadata =
                BTreeMap::from([("accesskit_id".to_owned(), "17437630179299350513".to_owned())]);
            nodes.insert(row_id.clone(), node);
            nodes.insert(
                label_id.clone(),
                plain_node(&label_id, Some(&row_id), &[], true),
            );
        }
        UiTree {
            generation: 1,
            roots: vec!["stress-list".to_owned()],
            nodes,
            diagnostics: Vec::new(),
        }
    }

    /// What one `get_ui_tree` reply costs, whole and selected. Run with
    /// `cargo test --release -p gpui-mcp-server --bin gpui-mcp tree_reply_stage_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark; prints numbers rather than asserting"]
    fn tree_reply_stage_costs() -> Result<(), String> {
        use std::time::Instant as StdInstant;

        const ROUNDS: u32 = 10;
        let tree = stress_tree(2_500);
        let one_row = args(Some("stress-row-3"), Some(1), true);
        let wide = args(Some("stress-list"), Some(2), true);
        let reply_bytes = |args: &TreeArgs| -> Result<usize, String> {
            serialized_reply(&tree_result(&tree, args)?).map(|reply| reply.to_string().len())
        };

        // The first conversion every reply pays for, whatever shape it returns.
        let started = StdInstant::now();
        let mut floor_bytes = 0;
        for _ in 0..ROUNDS {
            floor_bytes = serde_json::to_value(&tree)
                .map_err(|error| error.to_string())?
                .to_string()
                .len();
        }
        let floor_ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ROUNDS);

        let started = StdInstant::now();
        let mut whole_bytes = 0;
        for _ in 0..ROUNDS {
            whole_bytes = reply_bytes(&TreeArgs::default())?;
        }
        let whole_ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ROUNDS);

        let started = StdInstant::now();
        let mut legacy_bytes = 0;
        for _ in 0..ROUNDS {
            legacy_bytes = serialized_reply(&legacy_tree_result(&tree)?)?
                .to_string()
                .len();
        }
        let legacy_ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ROUNDS);

        let select_only = |args: &TreeArgs, rounds: u32| -> Result<(usize, f64), String> {
            let nodes = select_tree(&tree, args)?
                .ok_or("the subtree arguments select")?
                .nodes
                .len();
            let started = StdInstant::now();
            for _ in 0..rounds {
                drop(select_tree(&tree, args)?);
            }
            Ok((
                nodes,
                started.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds),
            ))
        };
        let (row_nodes, row_select_ms) = select_only(&one_row, ROUNDS * 1_000)?;
        let (wide_nodes, wide_select_ms) = select_only(&wide, ROUNDS * 10)?;

        let started = StdInstant::now();
        let mut row_bytes = 0;
        for _ in 0..ROUNDS {
            row_bytes = reply_bytes(&one_row)?;
        }
        let row_ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(ROUNDS);

        eprintln!(
            "get_ui_tree costs over a {}-node tree ({floor_bytes} bytes as JSON)",
            tree.nodes.len()
        );
        eprintln!("  first conversion to Value, then to text  {floor_ms:8.2} ms");
        eprintln!(
            "  whole tree reply, serialized_result      {whole_bytes} bytes  {whole_ms:8.2} ms/reply"
        );
        eprintln!(
            "  whole tree reply, rmcp Json wrapper      {legacy_bytes} bytes  {legacy_ms:8.2} ms/reply"
        );
        eprintln!(
            "  one-row subtree: selection {row_select_ms:8.3} ms -> {row_nodes} nodes, \
             reply {row_bytes} bytes in {row_ms:8.3} ms"
        );
        eprintln!(
            "  wide subtree (root + max_depth 2 + visible_only): selection {wide_select_ms:8.3} ms \
             -> {wide_nodes} nodes"
        );
        Ok(())
    }

    #[test]
    fn complete_tool_suite_is_registered() {
        let names: BTreeSet<String> = super::GpuiMcp::production_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let expected = BTreeSet::from(
            [
                "ping",
                "check_connection",
                "list_apps",
                "select_app",
                "list_app_commands",
                "execute_app_command",
                "get_frame_stats",
                "mark_frames",
                "get_frame_report",
                "record_performance",
                "get_performance_report",
                "get_logs",
                "clear_logs",
                "click_element",
                "double_click_element",
                "click_coordinates",
                "hover_element",
                "drag_element",
                "drag_coordinates",
                "pointer_location",
                "pointer_move",
                "pointer_down",
                "pointer_up",
                "pointer_click",
                "pointer_drag",
                "pointer_scroll",
                "keyboard",
                "type_text",
                "focus_element",
                "get_text_info",
                "set_text",
                "get_value",
                "set_value",
                "perform_action",
                "get_selection_count",
                "get_element_state",
                "scroll",
                "get_live_document",
                "preview_live_document",
                "get_ui_tree",
                "find_elements",
                "get_element",
                "get_element_bounds",
                "wait_for_element",
                "wait_for_state",
                "save_ui_snapshot",
                "load_ui_snapshot",
                "diff_ui_snapshots",
                "diff_current_ui",
                "screenshot",
                "screenshot_region",
                "screenshot_element",
                "highlight_elements",
                "clear_highlights",
                "annotate_elements",
                "remove_annotations",
                "list_annotations",
                "read_messages",
                "wait_for_messages",
                "send_message",
                "capture_screenshot_snapshot",
                "compare_screenshots",
                "diff_screenshots",
                "start_video_recording",
                "stop_video_recording",
            ]
            .map(str::to_owned),
        );
        assert_eq!(names, expected);
    }

    #[test]
    fn default_video_recording_uses_live_thirty_fps_capture() -> Result<(), String> {
        let args = serde_json::from_value::<StartVideoRecordingArgs>(json!({
            "artifact_name": "demo.mp4"
        }))
        .map_err(|error| error.to_string())?;

        assert_eq!(args.frames_per_second, 30);
        assert!(args.include_pointer);
        Ok(())
    }

    #[test]
    fn start_video_recording_schema_exposes_destination_bounds() -> Result<(), String> {
        let router = super::GpuiMcp::production_router();
        let Some(tool) = router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "start_video_recording")
        else {
            return Err("start_video_recording was not registered".to_owned());
        };
        let properties = tool
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| "start_video_recording schema has no properties".to_owned())?;
        let artifact_name = properties
            .get("artifact_name")
            .ok_or_else(|| "artifact_name schema is missing".to_owned())?;
        assert_eq!(artifact_name.get("minLength"), Some(&json!(1)));
        assert_eq!(artifact_name.get("maxLength"), Some(&json!(128)));
        assert_eq!(
            artifact_name.get("pattern"),
            Some(&json!(r"^[A-Za-z0-9][A-Za-z0-9._-]*[.]mp4$"))
        );
        Ok(())
    }
}
