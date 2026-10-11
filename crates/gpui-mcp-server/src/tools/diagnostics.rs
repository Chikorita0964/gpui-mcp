use super::{
    BridgeResult, Duration, FrameReportArgs, GpuiMcp, Json, JsonValue, LogsArgs, Operation,
    Parameters, RecordPerformanceArgs, ToolRouter, Value, ack_json, average_render_work_ms,
    encode_error, json, object_output, performance_assessment, sleep, tool, tool_router,
    validate_timeout,
};
use gpui_mcp_protocol::FrameReport;

/// Per-frame samples `record_performance` returns alongside its summary.
const RECORDED_FRAME_LIMIT: u16 = 64;

/// The busiest views a `summary_only` report keeps: how many views an app draws
/// has no bound (every GPUI Kit component is one), the compact reply's size has.
const SUMMARY_VIEWS: usize = 5;

/// A frame report as JSON; `summary_only` removes the per-frame samples and the
/// last frame's view draws and keeps the busiest [`SUMMARY_VIEWS`] views of the
/// activity, with `views_omitted` counting the rest, keeping every scalar and the
/// summary, so a caller still learns how many frames the report covers.
fn report_value(report: &FrameReport, summary_only: bool) -> Result<JsonValue, String> {
    let mut value = serde_json::to_value(report).map_err(encode_error)?;
    if summary_only && let Some(object) = value.as_object_mut() {
        object.remove("frames");
        object.remove("last_frame_views");
        if let Some(JsonValue::Array(views)) = object.get_mut("views") {
            views.truncate(SUMMARY_VIEWS);
        }
        let omitted = report.views.len().saturating_sub(SUMMARY_VIEWS);
        object.insert("views_omitted".to_owned(), JsonValue::from(omitted));
    }
    Ok(value)
}

#[tool_router(router = diagnostics_router)]
impl GpuiMcp {
    #[tool(
        description = "Return frame timing averages since the last mark_frames: GPUI's whole draw (draw_*), the bridge's own share of it (bridge_*), prepaint, paint, and observed repaint cadence; cadence is not FPS capacity for an event-driven UI"
    )]
    async fn get_frame_stats(&self) -> Result<Json<Value>, String> {
        let stats = self.frame_stats().await?;
        Ok(object_output(serde_json::to_value(stats).map_err(encode_error)?))
    }

    #[tool(
        description = "Start a frame measurement window: wait for frames already pending, then mark the last completed frame. get_frame_stats and get_frame_report then cover only frames completed after the mark."
    )]
    async fn mark_frames(&self) -> Result<Json<Value>, String> {
        self.settle_pending(Duration::from_secs(2)).await?;
        let BridgeResult::FrameStats(stats) = self.call(Operation::MarkFrames).await? else {
            return Err("bridge returned the wrong result for a frame mark".to_owned());
        };
        Ok(object_output(json!({
            "mark_frame_count": stats.mark_frame_count,
            "frame_stats": stats,
        })))
    }

    #[tool(
        description = "Report every frame completed after the last mark_frames (or after since_frame_count): per-frame GPUI draw time, the application's share (app_draw_ms) and the bridge's (bridge_ms), p50/p95/max, and which views rendered, why, and which replayed from cache. Pass `summary_only` for the aggregates without per-frame samples, with only the 5 busiest views (views_omitted counts the rest)."
    )]
    async fn get_frame_report(
        &self,
        Parameters(args): Parameters<FrameReportArgs>,
    ) -> Result<Json<Value>, String> {
        let report = self.frame_report(args.since_frame_count, args.frame_limit).await?;
        Ok(object_output(report_value(&report, args.summary_only)?))
    }

    #[tool(
        description = "Observe frames over a bounded interval and report before/after statistics plus per-frame samples, percentiles, and view-cache activity for exactly the frames drawn in the interval. Pass `summary_only` for the same report without per-frame samples, with only the 5 busiest views (views_omitted counts the rest)."
    )]
    async fn record_performance(
        &self,
        Parameters(args): Parameters<RecordPerformanceArgs>,
    ) -> Result<Json<Value>, String> {
        validate_timeout(args.duration_ms)?;
        let before = self.frame_stats().await?;
        sleep(Duration::from_millis(args.duration_ms)).await;
        let after = self.frame_stats().await?;
        let report = self.frame_report(Some(before.frame_count), RECORDED_FRAME_LIMIT).await?;
        Ok(object_output(json!({
            "duration_ms": args.duration_ms,
            "before": before,
            "after": after,
            "observed_frame_delta": after.frame_count.saturating_sub(before.frame_count),
            "frames": report_value(&report, args.summary_only)?,
            "cadence_note": "Event-driven applications repaint only when needed; a low observed cadence while idle is healthy and is not an FPS-capacity measurement.",
        })))
    }

    #[tool(description = "Return a concise current performance report")]
    async fn get_performance_report(&self) -> Result<Json<Value>, String> {
        let stats = self.frame_stats().await?;
        Ok(object_output(json!({
            "frame_stats": stats,
            "assessment": performance_assessment(&stats),
            "average_render_work_ms": average_render_work_ms(&stats),
            "average_draw_ms": stats.draw_average_ms,
            "average_bridge_ms": stats.bridge_average_ms,
            "cadence_note": "estimated_fps is observed repaint cadence, not rendering capacity for an event-driven UI",
        })))
    }

    #[tool(
        description = "Return bounded, application-published diagnostic logs; secrets must not be published by the app"
    )]
    async fn get_logs(
        &self,
        Parameters(args): Parameters<LogsArgs>,
    ) -> Result<Json<Value>, String> {
        let result =
            self.call(Operation::GetLogs { limit: args.limit, min_level: args.min_level }).await?;
        let BridgeResult::Logs(logs) = result else {
            return Err("bridge returned the wrong result for logs".to_owned());
        };
        Ok(object_output(json!({ "count": logs.len(), "entries": logs })))
    }

    #[tool(description = "Clear all retained application-published diagnostic logs")]
    async fn clear_logs(&self) -> Result<Json<Value>, String> {
        self.ack(Operation::ClearLogs).await?;
        Ok(ack_json("logs_cleared"))
    }
}

pub(super) fn router() -> ToolRouter<GpuiMcp> {
    GpuiMcp::diagnostics_router()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use gpui_mcp_protocol::{
        FrameReport, FrameSample, FrameSummary, ViewActivity, ViewDraw, ViewOutcome,
    };

    use super::{JsonValue, report_value};

    fn fixture() -> FrameReport {
        FrameReport {
            after_frame_count: 41,
            latest_frame_count: 44,
            truncated: true,
            summary: FrameSummary { frames: 3, ..FrameSummary::default() },
            frames: vec![FrameSample { frame_count: 42, ..FrameSample::default() }],
            views: vec![ViewActivity {
                entity_id: 7,
                type_name: "DemoView".to_owned(),
                rendered: 1,
                reused: 2,
                causes: BTreeMap::new(),
            }],
            last_frame_views: vec![ViewDraw {
                entity_id: 7,
                type_name: "DemoView".to_owned(),
                outcome: ViewOutcome::Rendered,
                cause: None,
            }],
        }
    }

    #[test]
    fn report_value_defaults_to_the_unchanged_serialization() -> Result<(), String> {
        let report = fixture();
        let default = serde_json::to_string(&report_value(&report, false)?)
            .map_err(|error| error.to_string())?;
        // The tool has always converted the report through `to_value` before
        // answering, so this is the value - and the bytes - of the default
        // reply, whatever key order the serde_json build uses.
        let previous = serde_json::to_string(
            &serde_json::to_value(&report).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(
            default, previous,
            "without summary_only the reply value must be the one the tool returned before the flag"
        );
        Ok(())
    }

    #[test]
    fn report_value_summary_only_drops_only_the_per_frame_samples() -> Result<(), String> {
        let report = fixture();
        let full = report_value(&report, false)?;
        let compact = report_value(&report, true)?;
        let compact = compact.as_object().ok_or("the compact report is an object")?;
        assert!(!compact.contains_key("frames"), "the per-frame samples are dropped");
        assert!(
            !compact.contains_key("last_frame_views"),
            "the last frame's view draws are dropped"
        );
        assert_eq!(
            compact.get("views_omitted"),
            Some(&JsonValue::from(0)),
            "a report with few views omits none"
        );
        for field in ["after_frame_count", "latest_frame_count", "truncated", "summary", "views"] {
            assert_eq!(compact.get(field), full.get(field), "{field} survives the reduction");
        }
        Ok(())
    }

    #[test]
    fn report_value_summary_only_keeps_the_busiest_views() -> Result<(), String> {
        let mut report = fixture();
        report.views = (0..13)
            .map(|entity_id| ViewActivity {
                entity_id,
                type_name: "DemoView".to_owned(),
                rendered: 1,
                reused: 0,
                causes: BTreeMap::new(),
            })
            .collect();
        let compact = report_value(&report, true)?;
        let views = compact
            .get("views")
            .and_then(JsonValue::as_array)
            .ok_or("the compact report keeps views")?;
        let kept: Vec<u64> = views
            .iter()
            .filter_map(|view| view.get("entity_id").and_then(JsonValue::as_u64))
            .collect();
        assert_eq!(kept, (0..5).collect::<Vec<u64>>(), "the busiest five, in order");
        assert_eq!(compact.get("views_omitted"), Some(&JsonValue::from(8)));
        Ok(())
    }
}
