//! Hardened, cross-platform MCP automation for GPUI applications.
//!
//! Install [`BridgeHandle`] for a window and retain it for that window's
//! lifetime. GPUI emits semantics automatically from stable element IDs,
//! registered interaction behavior, and its standard `Role` and `aria_*` APIs.
//! Control stays on owner-restricted native local IPC and authenticated
//! requests are dispatched through GPUI's foreground executor.

mod input;
mod native_window;
mod observer;
mod registry;
mod service;

use std::sync::Arc;

pub use gpui_mcp_protocol::MouseButton;
pub use gpui_mcp_protocol::{
    AppId, ApplicationCommandDescriptor, ApplicationCommandResult, BridgeError, ContextResource,
    ContextResourceDescriptor, ErrorCode, InstanceId, LiveDocument, LiveDocumentDiagnostic,
    LiveDocumentPreview, LiveDocumentSource, LogEntry, MAX_LABEL_BYTES,
    MAX_LIVE_DOCUMENT_DIAGNOSTICS, MAX_LIVE_DOCUMENT_SOURCE_BYTES, MAX_TEXT_BYTES, NativeWindowId,
    NodeAction, NodeState, Point, ProcessId, Rect, RequestId, Role, TextInfo, TextRange, UiNode,
    UiTree, ValueInfo,
};
pub use service::{
    ApplicationCommandRequest, ApplicationCommandResponse, BridgeConfig, BridgeConfigError,
    BridgeHandle, ContextResourceRequest, ContextResourceResponse, HostError, LiveDocumentRequest,
    LiveDocumentResponse, StartError,
};

use observer::BridgeObserver;
use registry::SharedState;

/// Return the operating system identifier used by window capture APIs.
///
/// Wayland and other platforms without a stable system window identifier
/// return `None`.
#[must_use]
pub fn native_window_id(window: &gpui::Window) -> Option<NativeWindowId> {
    native_window::id(window).and_then(NativeWindowId::new)
}

/// Cloneable application-side handle for semantic snapshots and diagnostic logs.
#[derive(Clone)]
pub struct Automation {
    pub(crate) state: Arc<SharedState>,
    observer: Arc<BridgeObserver>,
    /// Build the tree only while a client uses the bridge, instead of always.
    on_demand: bool,
}

impl Automation {
    /// Create isolated in-process automation without starting an MCP bridge.
    ///
    /// This is intended for offline application modes and embedded previews that
    /// still want one local semantic tree. No endpoint, listener, descriptor, or
    /// background thread is created.
    #[must_use]
    pub fn isolated() -> Self {
        Self::new(SharedState::new(), false)
    }

    pub(crate) fn new(state: Arc<SharedState>, on_demand: bool) -> Self {
        let observer = BridgeObserver::new(&state);
        Self {
            state,
            observer,
            on_demand,
        }
    }

    /// Attach semantic observation to a window.
    ///
    /// Every frame the window draws is counted, and the semantic tree is
    /// published whenever a frame changes it, including frames the application
    /// draws on its own. Isolated automation builds the tree from the next
    /// frame on; a bridge builds it only while a client uses the bridge.
    ///
    /// Calling this more than once for the same automation and window is a no-op.
    pub fn attach(&self, window: &mut gpui::Window) {
        window.add_a11y_frame_observer(self.observer.clone());
        if !self.on_demand {
            self.set_observed(window, true);
        }
    }

    /// Build the window's accessibility tree every frame, or stop building it.
    pub(crate) fn set_observed(&self, window: &mut gpui::Window, observed: bool) {
        self.observer.set_observed(window, observed);
    }

    /// Create isolated in-process automation without IPC for GPUI runtime tests.
    ///
    /// This constructor is available only with the `test-support` feature and
    /// must not be used as a substitute for [`BridgeHandle`] in an application.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn for_test() -> Self {
        Self::isolated()
    }

    /// Return the most recently completed semantic frame.
    #[must_use]
    pub fn snapshot(&self) -> UiTree {
        self.state.tree()
    }

    /// Return the generation of the most recently completed semantic frame without cloning it.
    ///
    /// Consumers that maintain a small derived view of the semantic tree can use this as a cheap
    /// invalidation guard and call [`Self::snapshot`] only after the generation changes.
    #[must_use]
    pub fn semantic_generation(&self) -> u64 {
        self.state.tree_generation()
    }

    /// Return the count of frames the attached window has drawn.
    ///
    /// This is available only to deterministic runtime tests. A frame is not
    /// counted until the observed window has finished painting.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn completed_frames(&self) -> u64 {
        self.state.frame_stats().frame_count
    }

    /// Retain a bounded, sanitized diagnostic log entry for MCP inspection.
    ///
    /// Do not pass secrets. Newlines are replaced and messages are capped at 4 KiB.
    pub fn log(&self, level: &str, message: &str) {
        self.state.add_log(level, message);
    }

    /// Most recent log entries in chronological order, filtered to `min_level`
    /// ("debug" < "info" < "warn" < "error") when given, capped at `limit`.
    #[must_use]
    pub fn logs(&self, limit: u16, min_level: Option<&str>) -> Vec<LogEntry> {
        self.state.logs(limit, min_level)
    }
}
