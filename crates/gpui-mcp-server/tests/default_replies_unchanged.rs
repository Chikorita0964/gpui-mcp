//! The seven flagged tools' default replies, frozen before the compact-results
//! change, over the real MCP stdio surface.
//!
//! Goal 07 adds opt-in compact shapes to the tools whose default reply exceeds
//! 4 KiB on the demo or the runtime-showcase. The change is additive: a caller
//! that passes no new argument must get today's reply. This file pins that
//! promise. It drives the same JSON-RPC
//! surface a client uses against the demo fixture and compares the serialized
//! `result` of each flagged tool with the committed fixture under
//! `tests/fixtures/default_replies/`.
//!
//! Protocol (the check and the generator run exactly this): the demo is spawned
//! beside the server binary with a private endpoint directory; after the
//! handshake and readiness, one session does
//! `save_ui_snapshot "t4-baseline"`, `find_elements {}`, `get_ui_tree {}`,
//! `load_ui_snapshot {"name":"t4-baseline"}`, `get_live_document {}` and
//! `preview_live_document` (the demo answers both live-document calls with its
//! 98-byte "Unsupported" error), then `pointer_move` to the inert `heading`
//! control, `mark_frames`, `pointer_move` to `probe-left-target` - the
//! one-frame settling action `frame_cost.rs` proves deterministic -
//! `get_frame_report {}`, `mark_frames`, and `record_performance
//! {"duration_ms": 200}`.
//!
//! Opening the window draws a short burst of `refresh` frames - platform
//! activation, accessibility sync and viewport changes each request a full
//! refresh - and a refresh that is still pending when the hover frame begins
//! renders every cached view, replacing the hover's `notified` cause in the
//! report. The session therefore waits for the frame counter to stop advancing
//! before it measures. That wait reads the frame report and requests no frame,
//! so it cannot itself change the frames the session measures.
//!
//! The comparison is byte-for-byte on the *canonical* serialization of the raw
//! JSON-RPC `result` object, and the fixtures store that same form. The frame
//! tools answer with real timings and absolute frame counters that differ on
//! every run, so the canonicalizer zeroes exactly the volatile numbers; every
//! other key, string, boolean, array and number - the reply's shape, its
//! counts, its view activity lists - is compared as sent. Object keys are
//! rewritten in name order, the form the fixtures were written in: the
//! workspace build enables `serde_json/preserve_order` through the vendored
//! gpui-pre crate, which makes a live reply serialize its keys in insertion
//! order, and JSON object order carries no meaning - comparing name order is
//! what makes the check mean the same thing under `-p gpui-mcp-server` and
//! under the goal's workspace feature set. Volatile keys: any key ending in
//! `_ms` except `duration_ms` (the echoed input), plus `estimated_fps`,
//! `frame_count`, `mark_frame_count`, `sample_count`, `after_frame_count`,
//! `latest_frame_count`, `observed_frame_delta`, `views_rendered`,
//! `views_reused`, `rendered`, `reused`, `max`, `mean`, `p50` and `p95`, plus
//! `generation`: the tree's publish counter advances by one per tree change the
//! platform's opening activation burst happens to deliver before the session's
//! first capture, so its value differs per run while the tree's content does
//! not. `count` and `frames` are deliberately not volatile: a stray extra frame
//! fails the check. JSON text inside a reply (`content[0].text`) is parsed and
//! canonicalized recursively, so it is held to the same rule as the
//! `structuredContent` copy of the same payload.
//!
//! Running: needs the demo built beside the server binary - `cargo build -p
//! gpui-mcp-demo -p gpui-mcp-server` - and a desktop session; without `DISPLAY`
//! or `WAYLAND_DISPLAY` the check prints a note and passes. One window is open
//! at a time through the static `WINDOW` mutex. To regenerate the fixtures
//! after a deliberate default-reply change, run the ignored generator once:
//! `cargo test -p gpui-mcp-server --test default_replies_unchanged -- --ignored
//! capture_default_replies`, and say so in the report.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use cow_utils::CowUtils as _;
use serde_json::{Value as JsonValue, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::timeout;

const REPLY_TIMEOUT: Duration = Duration::from_mins(1);
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(45);

/// How long the frame counter must stand still, and for how long the session
/// waits for that, before it measures anything.
const QUIET_POLLS: usize = 2;
const QUIET_POLL_INTERVAL: Duration = Duration::from_millis(150);
const QUIET_DEADLINE: Duration = Duration::from_secs(10);

/// The fixture's inert control the pointer parks on, and the control the one
/// settling hover notifies.
const PARKING: &str = "heading";
const LEFT: &str = "probe-left-target";

/// Controls that must be in the rendered tree before the session starts, so
/// the fixtures always capture a fully drawn window.
const READY_NODES: [&str; 5] =
    [PARKING, "increment", "count", "probe-left-target", "probe-right-target"];

/// The tools whose default replies the fixtures freeze, in session order.
const FIXTURE_TOOLS: [&str; 7] = [
    "find_elements",
    "get_ui_tree",
    "load_ui_snapshot",
    "get_live_document",
    "preview_live_document",
    "get_frame_report",
    "record_performance",
];

/// One window at a time: each run opens a real window on the shared desktop.
static WINDOW: Mutex<()> = Mutex::const_new(());

/// A GPUI MCP server child process driven over its real JSON-RPC stdio surface.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: i64,
}

impl Server {
    fn start(endpoints: &Path, artifacts: &Path) -> Result<Self, String> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gpui-mcp"))
            .arg("--endpoint-dir")
            .arg(endpoints)
            .arg("--artifact-dir")
            .arg(artifacts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not spawn the server: {error}"))?;
        let stdin = child.stdin.take().ok_or_else(|| "the server has no stdin".to_owned())?;
        let stdout = child.stdout.take().ok_or_else(|| "the server has no stdout".to_owned())?;
        Ok(Self { child, stdin, stdout: BufReader::new(stdout).lines(), next_id: 1 })
    }

    async fn send(&mut self, message: &JsonValue) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| format!("could not write to the server: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("could not flush the server's stdin: {error}"))
    }

    async fn request(&mut self, method: &str, params: JsonValue) -> Result<JsonValue, String> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&message).await?;
        loop {
            let line = timeout(REPLY_TIMEOUT, self.stdout.next_line())
                .await
                .map_err(|_| format!("timed out waiting for the {method} reply"))?
                .map_err(|error| format!("could not read the {method} reply: {error}"))?
                .ok_or_else(|| format!("the server closed stdout before replying to {method}"))?;
            let Ok(reply) = serde_json::from_str::<JsonValue>(&line) else {
                continue;
            };
            if reply.get("id").and_then(JsonValue::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(format!("{method} failed: {error}"));
            }
            return reply
                .get("result")
                .cloned()
                .ok_or_else(|| format!("{method} returned neither a result nor an error"));
        }
    }

    /// The raw `result` of a `tools/call`, `isError` included, because two of
    /// the fixtures capture the demo's error replies.
    async fn call_raw(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        self.request("tools/call", json!({ "name": tool, "arguments": arguments })).await
    }

    /// The structured payload of a tool that answers with JSON; a call that
    /// reports `isError` is a failure here, for the session's setup calls.
    async fn call_json(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        let result = self.call_raw(tool, arguments).await?;
        if result.get("isError").and_then(JsonValue::as_bool) == Some(true) {
            return Err(format!("{tool} reported an error: {result}"));
        }
        if let Some(structured) = result.get("structuredContent") {
            return Ok(structured.clone());
        }
        let text = result
            .get("content")
            .and_then(JsonValue::as_array)
            .and_then(|content| {
                content.iter().find_map(|entry| entry.get("text").and_then(JsonValue::as_str))
            })
            .ok_or_else(|| format!("{tool} returned no JSON payload: {result}"))?;
        serde_json::from_str(text)
            .map_err(|error| format!("{tool} returned unreadable JSON: {error}"))
    }

    /// The window-relative logical center of a node in the current tree.
    async fn center(&mut self, id: &str) -> Result<(f64, f64), String> {
        let tree = self.call_json("get_ui_tree", json!({})).await?;
        let bounds = tree
            .get("nodes")
            .and_then(|nodes| nodes.get(id))
            .and_then(|node| node.get("bounds"))
            .ok_or_else(|| format!("the fixture published no bounds for {id}"))?;
        let field = |name: &str| {
            bounds
                .get(name)
                .and_then(JsonValue::as_f64)
                .ok_or_else(|| format!("the bounds of {id} carry no {name}"))
        };
        Ok((field("x")? + field("width")? / 2.0, field("y")? + field("height")? / 2.0))
    }

    async fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.kill().await;
    }
}

/// The instrumented GPUI application the workspace ships as its bridge demo.
struct Fixture {
    child: Child,
    log: PathBuf,
}

impl Fixture {
    fn start(endpoints: &Path) -> Result<Self, String> {
        let log = endpoints.with_extension("fixture.stderr.log");
        let stderr = std::fs::File::create(&log)
            .map_err(|error| format!("could not create fixture log: {error}"))?;
        let child = Command::new(fixture_executable()?)
            .arg("--endpoint-dir")
            .arg(endpoints)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not spawn the fixture application: {error}"))?;
        Ok(Self { child, log })
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

/// The demo application is a separate workspace member, so its binary is found
/// beside this test's own server binary rather than through `CARGO_BIN_EXE`.
fn fixture_executable() -> Result<PathBuf, String> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_gpui-mcp"));
    let directory =
        server.parent().ok_or_else(|| "the server binary has no parent directory".to_owned())?;
    let path = directory.join(format!("gpui-mcp-demo{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("the demo fixture is not built at {}", path.display()))
    }
}

/// Whether this machine can open a window at all. The Linux CI job runs the
/// test suite without a display server and drives windowed fixtures under Xvfb
/// from a separate step.
fn has_a_desktop_session() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    } else {
        true
    }
}

impl support::FixtureClient for Server {
    async fn call_json(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        Server::call_json(self, tool, arguments).await
    }
}

/// Run `scenario` against a fresh fixture and server.
async fn with_fixture<F>(window: MutexGuard<'_, ()>, scenario: F) -> Result<(), String>
where
    F: AsyncFnOnce(&mut Server) -> Result<(), String>,
{
    let _window = window;
    let directory = TempDir::new()
        .map_err(|error| format!("could not create a temporary directory: {error}"))?;
    let endpoints = directory.path().join("endpoints");
    std::fs::create_dir_all(&endpoints)
        .map_err(|error| format!("could not create the endpoint directory: {error}"))?;
    let fixture = Fixture::start(&endpoints)?;
    let mut server = Server::start(&endpoints, &directory.path().join("artifacts"))?;

    let outcome = async {
        server
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "gpui-mcp-default-replies-test", "version": "0.0.0" },
                }),
            )
            .await?;
        server
            .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }))
            .await?;
        support::wait_for_fixture(&mut server, &READY_NODES, DISCOVERY_DEADLINE).await?;
        wait_until_quiet(&mut server).await?;
        scenario(&mut server).await
    }
    .await;

    let outcome = outcome.map_err(|error| support::fixture_failure(&fixture.log, &error));
    server.stop().await;
    fixture.stop().await;
    outcome
}

/// Wait for the window's opening frame burst to finish, so the measurement
/// window opens on a window that is not about to redraw every cached view.
/// Platform activation, accessibility activation and viewport changes each
/// request a full refresh, and a refresh still pending when the hover frame
/// begins renders both regions with `cause: refresh` instead of the hovered one
/// with `cause: notified`. Reads `get_frame_report` only, which requests no
/// frame, so the wait itself cannot add to what is measured; bounded by
/// `QUIET_DEADLINE`, after which the session proceeds and any drift fails the
/// check as before.
async fn wait_until_quiet(server: &mut Server) -> Result<(), String> {
    let deadline = Instant::now() + QUIET_DEADLINE;
    let mut last = None;
    let mut stable = 0;
    while Instant::now() < deadline {
        let report = server.call_json("get_frame_report", json!({})).await?;
        let count = report
            .get("latest_frame_count")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| format!("the frame report carries no frame counter: {report}"))?;
        if last == Some(count) {
            stable += 1;
            if stable >= QUIET_POLLS {
                return Ok(());
            }
        } else {
            stable = 0;
        }
        last = Some(count);
        tokio::time::sleep(QUIET_POLL_INTERVAL).await;
    }
    Ok(())
}

/// The canonical form the fixtures are written in and the check compares
/// against: `value` with every volatile number replaced by `0`, every object's
/// keys in name order, and JSON text canonicalized recursively; everything else
/// kept exactly as sent. See the module comment for the volatile-key rule and
/// why key order is canonicalized.
fn canonical(value: &JsonValue, key: Option<&str>) -> JsonValue {
    match value {
        JsonValue::Object(fields) => {
            let mut names: Vec<&String> = fields.keys().collect();
            names.sort();
            let mut ordered = serde_json::Map::new();
            for name in names {
                ordered.insert(name.clone(), canonical(&fields[name], Some(name.as_str())));
            }
            JsonValue::Object(ordered)
        }
        JsonValue::Array(items) => {
            JsonValue::Array(items.iter().map(|item| canonical(item, None)).collect())
        }
        JsonValue::String(text) if text.starts_with('{') || text.starts_with('[') => {
            // Tool payloads ride in `content[].text` as JSON; the payload must
            // follow the same rule as the `structuredContent` copy.
            match serde_json::from_str::<JsonValue>(text) {
                Ok(parsed) => JsonValue::String(canonical(&parsed, None).to_string()),
                Err(_) => value.clone(),
            }
        }
        JsonValue::Number(_) if key.is_some_and(is_volatile) => json!(0),
        _ => value.clone(),
    }
}

/// Whether a key names a value that differs on every run: a measured duration,
/// a sample distribution statistic, an absolute frame counter, a per-view
/// render/reuse count, or the tree's publish counter.
fn is_volatile(key: &str) -> bool {
    const VOLATILE: [&str; 16] = [
        "estimated_fps",
        "frame_count",
        "mark_frame_count",
        "sample_count",
        "after_frame_count",
        "latest_frame_count",
        "observed_frame_delta",
        "views_rendered",
        "views_reused",
        "rendered",
        "reused",
        "max",
        "mean",
        "p50",
        "p95",
        "generation",
    ];
    // `duration_ms` echoes the caller's input, so it is not a measurement.
    (key.ends_with("_ms") && key != "duration_ms") || VOLATILE.contains(&key)
}

/// One canonical serialized `result` per fixture tool, captured in the order
/// `FIXTURE_TOOLS` lists.
async fn capture_session(server: &mut Server) -> Result<Vec<(&'static str, String)>, String> {
    let mut captured = Vec::new();

    server.call_json("save_ui_snapshot", json!({ "name": "t4-baseline" })).await?;
    capture(server, "find_elements", json!({}), &mut captured).await?;
    capture(server, "get_ui_tree", json!({}), &mut captured).await?;
    capture(server, "load_ui_snapshot", json!({ "name": "t4-baseline" }), &mut captured).await?;
    capture(server, "get_live_document", json!({}), &mut captured).await?;
    capture(
        server,
        "preview_live_document",
        json!({
            "html": "<p>t4</p>",
            "css": "",
            "bindings_ron": "()",
            "expected_revision": 1,
        }),
        &mut captured,
    )
    .await?;

    // One hover frame: park on the inert control, mark, then hover the left
    // region so the report covers exactly the frame that hover drew.
    let parking = server.center(PARKING).await?;
    let left = server.center(LEFT).await?;
    server.call_json("pointer_move", json!({ "x": parking.0, "y": parking.1 })).await?;
    server.call_json("mark_frames", json!({})).await?;
    server.call_json("pointer_move", json!({ "x": left.0, "y": left.1 })).await?;
    capture(server, "get_frame_report", json!({}), &mut captured).await?;

    // An idle window: the statistics cover no frames and the frame list stays
    // empty, which is the deterministic baseline for this tool.
    server.call_json("mark_frames", json!({})).await?;
    capture(server, "record_performance", json!({ "duration_ms": 200 }), &mut captured).await?;

    Ok(captured)
}

/// Capture one tool's raw `result` in canonical form.
async fn capture(
    server: &mut Server,
    tool: &'static str,
    arguments: JsonValue,
    captured: &mut Vec<(&'static str, String)>,
) -> Result<(), String> {
    let result = server.call_raw(tool, arguments).await?;
    let canonical = serde_json::to_string(&canonical(&result, None))
        .map_err(|error| format!("could not serialize the canonical {tool} reply: {error}"))?;
    captured.push((tool, canonical));
    Ok(())
}

/// Where the committed fixtures live.
fn fixtures_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/default_replies")
}

/// A failure line naming the tool, the fixture and the first differing byte.
fn mismatch(tool: &str, path: &Path, live: &str, fixture: &str) -> String {
    let differing = live.as_bytes().iter().zip(fixture.as_bytes()).position(|(a, b)| a != b);
    let at = differing.unwrap_or_else(|| live.len().min(fixture.len()));
    format!(
        "{tool}: the default reply no longer matches {} \
         (first difference at byte {at}; live {} bytes, fixture {} bytes)\n  \
         live:    {}...\n  \
         fixture: {}...\n  \
         (volatile timings and frame counters are zeroed before this comparison; \
         every other byte, including `count` and `frames`, is compared as sent)",
        path.display(),
        live.len(),
        fixture.len(),
        excerpt(live, at),
        excerpt(fixture, at),
    )
}

/// ~80 bytes of `text` around `at`, losing any partial UTF-8 at the edges.
fn excerpt(text: &str, at: usize) -> String {
    let bytes = text.as_bytes();
    let start = at.saturating_sub(40);
    let end = (at + 40).min(bytes.len());
    format!("[{}]", String::from_utf8_lossy(&bytes[start..end]).cow_replace('\n', "\\n"))
}

/// Check one captured session against the committed fixtures, reporting every
/// drifted tool rather than only the first.
fn compare_with_fixtures(captured: &[(&str, String)]) -> Result<(), String> {
    let directory = fixtures_directory();
    let mut failures = Vec::new();
    for (tool, live) in captured {
        let path = directory.join(format!("{tool}.json"));
        let fixture = match std::fs::read_to_string(&path) {
            Ok(fixture) => fixture,
            Err(error) => {
                failures.push(format!("could not read {}: {error}", path.display()));
                continue;
            }
        };
        if *live != fixture {
            failures.push(mismatch(tool, &path, live, &fixture));
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("\n")) }
}

#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "a test without a desktop session says why it passed without opening a window"
)]
async fn default_replies_match_the_fixtures() -> Result<(), String> {
    if !has_a_desktop_session() {
        eprintln!("skipping: this machine has no desktop session to open a window on");
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let captured = capture_session(server).await?;
        for tool in FIXTURE_TOOLS {
            if !captured.iter().any(|(name, _)| *name == tool) {
                return Err(format!("the session captured no reply for {tool}"));
            }
        }
        compare_with_fixtures(&captured)
    })
    .await
}

/// Write the fixtures from a fresh session. Ignored: run it deliberately after
/// a default-reply change, then say so in the report.
#[tokio::test]
#[ignore = "regenerates tests/fixtures/default_replies; run deliberately"]
#[expect(
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "the generator says what it wrote, or why it could not run"
)]
async fn capture_default_replies() -> Result<(), String> {
    if !has_a_desktop_session() {
        eprintln!("skipping: this machine has no desktop session to open a window on");
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let captured = capture_session(server).await?;
        let directory = fixtures_directory();
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
        for (tool, canonical) in &captured {
            let path = directory.join(format!("{tool}.json"));
            std::fs::write(&path, canonical)
                .map_err(|error| format!("could not write {}: {error}", path.display()))?;
            println!("wrote {} ({} bytes)", path.display(), canonical.len());
        }
        Ok(())
    })
    .await
}
