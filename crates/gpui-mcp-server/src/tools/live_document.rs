use super::{
    BridgeResult, GpuiMcp, Json, JsonValue, LiveDocumentSource, Operation, Parameters, ToolRouter,
    Value, encode_error, json, object_output, tool, tool_router,
};
use gpui_mcp_protocol::{LiveDocument, LiveDocumentPreview};
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Instant;

#[derive(Debug, Deserialize, JsonSchema)]
struct GetLiveDocumentArgs {
    /// Return only the active revision and per-source byte counts, not the sources.
    #[serde(default)]
    summary_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PreviewLiveDocumentArgs {
    /// Active document revision this complete edit was based on.
    expected_revision: u64,
    /// Complete standard HTML source for the candidate revision.
    html: String,
    /// Complete standard CSS source for the candidate revision.
    css: String,
    /// Complete versioned binding document encoded as RON.
    bindings_ron: String,
    /// Return only the applied status and error count, not the full preview.
    #[serde(default)]
    summary_only: bool,
}

/// The compact live-document reply: the revision and each source's byte count.
fn live_document_summary(document: &LiveDocument) -> JsonValue {
    json!({
        "revision": document.revision,
        "byte_counts": {
            "html": document.source.html.len(),
            "css": document.source.css.len(),
            "bindings_ron": document.source.bindings_ron.len(),
        },
    })
}

/// The compact preview: whether the candidate applied, and how many errors it
/// reported. The count is over the candidate's own diagnostics - its warnings
/// are not errors - and the document the preview carries is not included.
fn preview_summary(preview: &LiveDocumentPreview) -> JsonValue {
    json!({
        "applied": preview.applied,
        "error_count": preview
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == "error")
            .count(),
    })
}

#[tool_router(router = live_document_router)]
impl GpuiMcp {
    #[tool(
        description = "Return the active revisioned HTML/CSS/RON preview document; this capability is app opt-in and performs no filesystem access. Pass `summary_only` for the revision and per-source byte counts without the sources."
    )]
    async fn get_live_document(
        &self,
        Parameters(args): Parameters<GetLiveDocumentArgs>,
    ) -> Result<Json<Value>, String> {
        let result = self.call(Operation::GetLiveDocument).await?;
        let BridgeResult::LiveDocument(document) = result else {
            return Err("bridge returned the wrong live document result".to_owned());
        };
        if args.summary_only {
            return Ok(object_output(live_document_summary(&document)));
        }
        Ok(object_output(json!({ "document": document })))
    }

    #[tool(
        description = "Compile and atomically preview a complete in-memory HTML/CSS/RON document against an expected revision; never writes files and keeps the last-good preview on failure. Pass `summary_only` for the applied status and error count without the full preview."
    )]
    async fn preview_live_document(
        &self,
        Parameters(args): Parameters<PreviewLiveDocumentArgs>,
    ) -> Result<Json<Value>, String> {
        let started = Instant::now();
        let frame_count = self.frame_stats().await?.frame_count;
        let apply_started = Instant::now();
        let result = self
            .call(Operation::PreviewLiveDocument {
                expected_revision: args.expected_revision,
                source: LiveDocumentSource {
                    html: args.html,
                    css: args.css,
                    bindings_ron: args.bindings_ron,
                },
            })
            .await?;
        let apply_round_trip_ms = apply_started.elapsed().as_secs_f64() * 1_000.0;
        let BridgeResult::LiveDocumentPreview(preview) = result else {
            return Err("bridge returned the wrong live document preview result".to_owned());
        };
        let frame_wait_started = Instant::now();
        if preview.applied {
            self.wait_for_frame(frame_count, std::time::Duration::from_secs(2))
                .await?;
        }
        let frame_wait_ms = frame_wait_started.elapsed().as_secs_f64() * 1_000.0;
        let ok = preview.applied;
        let preview_reply = if args.summary_only {
            preview_summary(&preview)
        } else {
            serde_json::to_value(&preview).map_err(encode_error)?
        };
        Ok(object_output(json!({
            "ok": ok,
            "preview": preview_reply,
            "timing": {
                "apply_round_trip_ms": apply_round_trip_ms,
                "frame_wait_ms": frame_wait_ms,
                "total_ms": started.elapsed().as_secs_f64() * 1_000.0,
            },
        })))
    }
}

pub(super) fn router() -> ToolRouter<GpuiMcp> {
    GpuiMcp::live_document_router()
}

#[cfg(test)]
mod tests {
    use gpui_mcp_protocol::{
        LiveDocument, LiveDocumentDiagnostic, LiveDocumentPreview, LiveDocumentSource,
    };
    use serde_json::json;

    use super::{live_document_summary, preview_summary};

    fn source() -> LiveDocumentSource {
        LiveDocumentSource {
            html: "<p>hi</p>".to_owned(),
            css: "p { color: red }".to_owned(),
            bindings_ron: "(hooks: [])".to_owned(),
        }
    }

    #[test]
    fn live_document_summary_reports_each_source_byte_count() {
        let document = LiveDocument {
            revision: 7,
            source: source(),
            diagnostics: Vec::new(),
        };
        let summary = live_document_summary(&document);
        assert_eq!(summary["revision"], json!(7));
        assert_eq!(
            summary["byte_counts"]["html"],
            json!(document.source.html.len())
        );
        assert_eq!(
            summary["byte_counts"]["css"],
            json!(document.source.css.len())
        );
        assert_eq!(
            summary["byte_counts"]["bindings_ron"],
            json!(document.source.bindings_ron.len())
        );
        assert!(
            summary.get("document").is_none(),
            "the sources are not part of the compact reply"
        );
    }

    #[test]
    fn preview_summary_counts_errors_and_not_warnings() {
        let preview = LiveDocumentPreview {
            applied: false,
            document: LiveDocument {
                revision: 7,
                source: source(),
                diagnostics: Vec::new(),
            },
            diagnostics: vec![
                LiveDocumentDiagnostic {
                    severity: "warning".to_owned(),
                    message: "slow selector".to_owned(),
                },
                LiveDocumentDiagnostic {
                    severity: "error".to_owned(),
                    message: "unclosed tag".to_owned(),
                },
                LiveDocumentDiagnostic {
                    severity: "error".to_owned(),
                    message: "unknown hook".to_owned(),
                },
            ],
        };
        let summary = preview_summary(&preview);
        assert_eq!(summary, json!({ "applied": false, "error_count": 2 }));
        assert!(
            summary.get("document").is_none() && summary.get("diagnostics").is_none(),
            "the compact preview drops the full preview detail"
        );
    }
}
