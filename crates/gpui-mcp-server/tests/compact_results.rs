//! Compact tool replies, over the real MCP stdio surface.
//!
//! Goal 07 adds opt-in compact replies to the tools whose default result exceeds
//! the 4096-byte budget on the demo or the runtime-showcase: `find_elements`,
//! `get_ui_tree` and `load_ui_snapshot` take `ids_only`, `get_frame_report` and
//! `record_performance` take `summary_only`, and the two live-document tools take
//! `summary_only` too.
//! Every option is additive: it answers under the budget while keeping the promised
//! reduced shape, and a call without it is unchanged (the fixture test covers the
//! default replies).
//!
//! These tests drive each flag through the same JSON-RPC surface a client uses,
//! against the demo fixture `tree_selection.rs` spawns beside the server binary
//! with a private endpoint directory. Sizes are measured the way T1 measured them -
//! the compact UTF-8 serialization of the raw `tools/call` `result` object - so a
//! number here is comparable with the table this goal was planned from.
//!
//! The implementation lands on worker2's branch, so on a tree without it the
//! compact argument is an unknown JSON field: rmcp deserializes arguments with
//! serde and the argument structs do not deny unknown fields, so the call succeeds
//! with the flag silently ignored and the compact tests fail on their shape and
//! size assertions instead of on a schema rejection. On the demo neither
//! live-document tool has a document to compact (the 81 KB document is published
//! by the showcase, which this suite does not drive), so those two tests pass with
//! or without the flag: they pin that the flag is accepted and the documented demo
//! error is unchanged.
//!
//! Running them needs the fixture built first (`cargo build -p gpui-mcp-server -p
//! gpui-mcp-demo`), since the demo binary is looked up beside the server binary,
//! and a desktop session: without `DISPLAY` or `WAYLAND_DISPLAY` each test prints a
//! note and passes without opening a window. One window is open at a time, because
//! the static `WINDOW` mutex is held for the whole of each test body.

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value as JsonValue, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::timeout;

const REPLY_TIMEOUT: Duration = Duration::from_mins(1);
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(45);

/// The quiet wait: `latest_frame_count` must repeat across this many polls,
/// `QUIET_POLL_INTERVAL` apart, before a session measures frames. Two repeated
/// readings mean three equal counts, which is what worker5's protocol (and the
/// `default_replies_unchanged` fixtures) use; `QUIET_DEADLINE` bounds the wait so
/// a never-quiet window proceeds exactly as before.
const QUIET_POLLS: usize = 2;
const QUIET_POLL_INTERVAL: Duration = Duration::from_millis(150);
const QUIET_DEADLINE: Duration = Duration::from_secs(10);

/// The size goal 07 calls compact; every `*_only` reply in this file must be under it.
const COMPACT_LIMIT_BYTES: usize = 4096;

/// The controls a fixture must have rendered before a test drives it: the parking
/// spot the frame test starts from, the leaf the tree composition check selects,
/// and the cached region whose hover draws a frame.
const READY_IDS: [&str; 3] = ["heading", "increment", "probe-left-target"];

/// The fixture node the tree composition check selects; the demo publishes it
/// without descendants, so its subtree is itself.
const LEAF: &str = "increment";

/// The demo's documented reply when no document host is registered: both
/// live-document tools answer exactly this on the demo (T1 measured 98 bytes).
const UNSUPPORTED: &str = "Unsupported: document host is not registered";

/// One window at a time: each test opens a real window on the shared desktop, and
/// seven at once make the fixture flaky. The guard is handed to `with_fixture` and
/// lives for the body.
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
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "the server has no stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the server has no stdout".to_owned())?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
        })
    }

    /// The MCP handshake, then wait until the fixture is discoverable and rendered.
    async fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "gpui-mcp-compact-results-test", "version": "0.0.0" },
            }),
        )
        .await?;
        self.send(
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
        )
        .await?;
        wait_for_the_fixture(self).await
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

    /// A `tools/call` reply exactly as it arrived, `isError` included, so a
    /// documented error path can be inspected instead of failing the call.
    async fn call_raw(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
        .await
    }

    /// A tool call that must succeed, with its JSON payload and the serialized
    /// byte size of the whole `result` object it arrived in.
    async fn call_measured(
        &mut self,
        tool: &str,
        arguments: JsonValue,
    ) -> Result<Measured, String> {
        let result = self.call_raw(tool, arguments).await?;
        if result.get("isError").and_then(JsonValue::as_bool) == Some(true) {
            return Err(format!("{tool} reported an error: {}", reply_text(&result)));
        }
        Ok(Measured {
            payload: payload(tool, &result)?,
            result_bytes: result_bytes(&result)?,
        })
    }

    /// The structured payload of a tool that answers with JSON.
    async fn call_json(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        Ok(self.call_measured(tool, arguments).await?.payload)
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
        Ok((
            field("x")? + field("width")? / 2.0,
            field("y")? + field("height")? / 2.0,
        ))
    }

    async fn pointer_move(&mut self, point: (f64, f64)) -> Result<(), String> {
        self.call_json("pointer_move", json!({ "x": point.0, "y": point.1 }))
            .await
            .map(drop)
    }

    async fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.kill().await;
    }
}

/// One successful compact call: its payload and the size of the reply it came on.
struct Measured {
    payload: JsonValue,
    /// The serialized size of the raw JSON-RPC `result` object, compact, UTF-8 -
    /// T1's convention (`harness.py`'s `result_bytes`), which its size table uses.
    result_bytes: usize,
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
    let directory = server
        .parent()
        .ok_or_else(|| "the server binary has no parent directory".to_owned())?;
    let path = directory.join(format!("gpui-mcp-demo{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "the demo fixture is not built at {}",
            path.display()
        ))
    }
}

/// Whether this machine can open a window at all. The Linux CI job runs the test
/// suite without a display server and drives windowed fixtures under Xvfb from a
/// separate step.
fn has_a_desktop_session() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    } else {
        true
    }
}

/// Wait until exactly the fixture is discoverable and has rendered the controls
/// every test anchors on.
async fn wait_for_the_fixture(server: &mut Server) -> Result<(), String> {
    support::wait_for_fixture(server, &READY_IDS, DISCOVERY_DEADLINE).await
}

impl support::FixtureClient for Server {
    async fn call_json(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        Server::call_json(self, tool, arguments).await
    }
}

/// Wait for the window's opening frame burst to finish, so a measurement window
/// opens on a window that is not about to redraw every cached view. Platform
/// activation, accessibility activation and viewport changes each request a full
/// refresh after the window opens, and those frames inflate a report's summary
/// and view activity: integrator1 measured the compact `record_performance` reply
/// at 4106-4472 bytes with the burst inside its 200 ms window and a stable 3028
/// bytes settled. The poll reads `get_frame_report` only, which requests no
/// frame, so the wait itself cannot add to what is measured; it is bounded by
/// `QUIET_DEADLINE`, after which the session proceeds as it did before.
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

/// Run `scenario` against a fresh fixture and server. `window` is the `WINDOW`
/// lock, held for the whole scenario so only one test opens a window at a time.
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
        server.initialize().await?;
        wait_until_quiet(&mut server).await?;
        scenario(&mut server).await
    }
    .await;

    let outcome = outcome.map_err(|error| support::fixture_failure(&fixture.log, &error));
    server.stop().await;
    fixture.stop().await;
    outcome
}

/// The skip note every test prints when it cannot open a window.
fn skip_without_a_desktop() -> bool {
    if has_a_desktop_session() {
        false
    } else {
        eprintln!("skipping: this machine has no desktop session to open a window on");
        true
    }
}

/// The structured payload of a tool reply: `structuredContent` when the server
/// sends it, else the first text content entry, which is the JSON payload. A
/// compact `get_ui_tree` reply may be text-only, because the tool's declared
/// output schema describes the whole-tree shape and cannot cover the compact one.
fn payload(tool: &str, result: &JsonValue) -> Result<JsonValue, String> {
    if let Some(structured) = result.get("structuredContent") {
        return Ok(structured.clone());
    }
    let text = result
        .get("content")
        .and_then(JsonValue::as_array)
        .and_then(|content| {
            content
                .iter()
                .find_map(|entry| entry.get("text").and_then(JsonValue::as_str))
        })
        .ok_or_else(|| format!("{tool} returned no JSON payload: {result}"))?;
    serde_json::from_str(text).map_err(|error| format!("{tool} returned unreadable JSON: {error}"))
}

/// Every `text` content entry of a tool reply joined by newlines, or the whole
/// reply when it carries none.
fn reply_text(result: &JsonValue) -> String {
    let text: Vec<&str> = result
        .get("content")
        .and_then(JsonValue::as_array)
        .map(|content| {
            content
                .iter()
                .filter_map(|entry| entry.get("text").and_then(JsonValue::as_str))
                .collect()
        })
        .unwrap_or_default();
    if text.is_empty() {
        result.to_string()
    } else {
        text.join("\n")
    }
}

/// The serialized size of the raw JSON-RPC `result` object, compact, UTF-8: the
/// field T1's size table measured.
fn result_bytes(result: &JsonValue) -> Result<usize, String> {
    serde_json::to_string(result)
        .map(|text| text.len())
        .map_err(|error| format!("could not serialize the reply: {error}"))
}

/// One named field of an object, as the value it carries.
fn field<'a>(value: &'a JsonValue, name: &str, what: &str) -> Result<&'a JsonValue, String> {
    value
        .get(name)
        .ok_or_else(|| format!("{what} carries no {name}: {value}"))
}

/// The object a value must be.
fn object<'a>(
    value: &'a JsonValue,
    what: &str,
) -> Result<&'a serde_json::Map<String, JsonValue>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} is not a JSON object: {value}"))
}

/// One array field of an object.
fn array_at<'a>(value: &'a JsonValue, name: &str, what: &str) -> Result<&'a [JsonValue], String> {
    field(value, name, what)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{what} carries no {name} array: {value}"))
}

/// One integer field of an object.
fn u64_at(value: &JsonValue, name: &str, what: &str) -> Result<u64, String> {
    field(value, name, what)?
        .as_u64()
        .ok_or_else(|| format!("{what} carries no integer {name}: {value}"))
}

/// One string field of an object.
fn string_at<'a>(value: &'a JsonValue, name: &str, what: &str) -> Result<&'a str, String> {
    field(value, name, what)?
        .as_str()
        .ok_or_else(|| format!("{what} carries no string {name}: {value}"))
}

/// One boolean field of an object.
fn bool_at(value: &JsonValue, name: &str, what: &str) -> Result<bool, String> {
    field(value, name, what)?
        .as_bool()
        .ok_or_else(|| format!("{what} carries no boolean {name}: {value}"))
}

/// One string-array field of an object.
fn strings_at(value: &JsonValue, name: &str, what: &str) -> Result<Vec<String>, String> {
    array_at(value, name, what)?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{what} carries a non-string in {name}: {item}"))
        })
        .collect()
}

/// Whether the compact reply carries exactly the promised fields: the kept ones
/// present and the dropped ones absent, with nothing else invented.
fn assert_compact_shape(tool: &str, payload: &JsonValue, expected: &[&str]) -> Result<(), String> {
    let actual: Vec<&String> = object(payload, tool)?.keys().collect();
    let expected: BTreeSet<&str> = expected.iter().copied().collect();
    let actual_set: BTreeSet<&str> = actual.iter().map(|key| key.as_str()).collect();
    if actual_set != expected {
        return Err(format!(
            "the compact {tool} reply must carry exactly {expected:?}, it carries {actual:?}"
        ));
    }
    Ok(())
}

/// The compact reply's measured size, printed for the report.
fn assert_under_the_cap(tool: &str, measured: &Measured) -> Result<(), String> {
    println!(
        "measure {tool} compact reply: result_bytes={}",
        measured.result_bytes
    );
    if measured.result_bytes >= COMPACT_LIMIT_BYTES {
        return Err(format!(
            "the compact {tool} reply is {} bytes; goal 07 requires under {COMPACT_LIMIT_BYTES}",
            measured.result_bytes
        ));
    }
    Ok(())
}

/// The default reply's measured size, printed and checked to be over the cap, so
/// the compact test above it is not vacuously passing on a small fixture.
fn assert_default_over_the_cap(tool: &str, measured: &Measured) -> Result<(), String> {
    println!(
        "measure {tool} default reply: result_bytes={}",
        measured.result_bytes
    );
    if measured.result_bytes <= COMPACT_LIMIT_BYTES {
        return Err(format!(
            "the {tool} default reply is only {} bytes, so its compact-size assertion proves nothing",
            measured.result_bytes
        ));
    }
    Ok(())
}

/// The demo's documented unsupported live-document reply, whatever the flag says:
/// the call must be answered (not rejected) and the error text must be unchanged.
fn assert_unsupported(tool: &str, result: &JsonValue) -> Result<(), String> {
    if result.get("isError").and_then(JsonValue::as_bool) != Some(true) {
        return Err(format!(
            "{tool} must answer the documented demo error with the compact flag, it answered: {result}"
        ));
    }
    let text = reply_text(result);
    if text.trim() != UNSUPPORTED {
        return Err(format!(
            "{tool} must answer {UNSUPPORTED:?} on the demo, it answered {text:?}"
        ));
    }
    println!(
        "measure {tool} summary_only unsupported reply: result_bytes={}",
        result_bytes(result)?
    );
    Ok(())
}

#[tokio::test]
async fn find_elements_ids_only_returns_the_count_and_the_ordered_ids() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // The measured default: `{}` is what T1 sized at 11,594 bytes on the demo.
        let full = server.call_measured("find_elements", json!({})).await?;
        assert_default_over_the_cap("find_elements", &full)?;
        let ordered: Vec<String> = array_at(&full.payload, "elements", "find_elements")?
            .iter()
            .map(|element| {
                field(element, "id", "a find_elements element")?
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("a find_elements element has no string id: {element}"))
            })
            .collect::<Result<_, String>>()?;
        if ordered.is_empty() {
            return Err("the demo matched no element, so the compact reply proves nothing".to_owned());
        }

        let compact = server
            .call_measured("find_elements", json!({ "ids_only": true }))
            .await?;
        assert_compact_shape("find_elements", &compact.payload, &["count", "ids"])?;
        let count = u64_at(&compact.payload, "count", "find_elements ids_only")?;
        assert_eq!(
            count,
            u64::try_from(ordered.len())
                .map_err(|error| format!("could not compare the match count: {error}"))?,
            "count must be the full reply's match count"
        );
        assert_eq!(
            strings_at(&compact.payload, "ids", "find_elements ids_only")?,
            ordered,
            "ids must be the matching ids in the same order the full reply's elements array lists them"
        );
        assert_under_the_cap("find_elements", &compact)
    })
    .await
}

#[tokio::test]
async fn get_ui_tree_ids_only_returns_the_same_nodes_plus_their_count() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // The measured default: `{}` is what T1 sized at 12,002 bytes on the demo.
        let full = server.call_measured("get_ui_tree", json!({})).await?;
        assert_default_over_the_cap("get_ui_tree", &full)?;
        let full_ids: BTreeSet<String> = object(&full.payload, "get_ui_tree")?
            .get("nodes")
            .and_then(JsonValue::as_object)
            .ok_or_else(|| format!("the whole tree carries no nodes map: {}", full.payload))?
            .keys()
            .cloned()
            .collect();
        let roots = strings_at(&full.payload, "roots", "get_ui_tree")?;
        let generation = u64_at(&full.payload, "generation", "get_ui_tree")?;

        let compact = server
            .call_measured("get_ui_tree", json!({ "ids_only": true }))
            .await?;
        assert_compact_shape(
            "get_ui_tree",
            &compact.payload,
            &["generation", "node_count", "roots", "ids"],
        )?;
        assert_eq!(
            u64_at(&compact.payload, "generation", "get_ui_tree ids_only")?,
            generation,
            "the compact reply must carry the generation of the tree it describes"
        );
        assert_eq!(
            strings_at(&compact.payload, "roots", "get_ui_tree ids_only")?,
            roots,
            "the compact reply must carry the same roots"
        );
        let ids: BTreeSet<String> = strings_at(&compact.payload, "ids", "get_ui_tree ids_only")?
            .into_iter()
            .collect();
        assert!(
            !ids.is_empty(),
            "the demo renders nodes, so ids must not be empty"
        );
        assert_eq!(
            u64::try_from(ids.len())
                .map_err(|error| format!("could not compare the node count: {error}"))?,
            u64_at(&compact.payload, "node_count", "get_ui_tree ids_only")?,
            "node_count must be the number of ids"
        );
        assert_eq!(
            ids, full_ids,
            "ids must be exactly the nodes the full reply would carry"
        );
        assert_under_the_cap("get_ui_tree", &compact)?;

        // The existing selection still applies first (T2): selecting the
        // descendant-free leaf with ids_only returns that node alone.
        let leaf = server
            .call_measured("get_ui_tree", json!({ "root": LEAF, "ids_only": true }))
            .await?;
        assert_compact_shape(
            "get_ui_tree root+ids_only",
            &leaf.payload,
            &["generation", "node_count", "roots", "ids"],
        )?;
        assert_eq!(
            strings_at(&leaf.payload, "roots", "get_ui_tree root+ids_only")?,
            vec![LEAF.to_owned()],
            "roots must name the requested node"
        );
        assert_eq!(
            strings_at(&leaf.payload, "ids", "get_ui_tree root+ids_only")?,
            vec![LEAF.to_owned()],
            "{LEAF} has no descendants in the demo, so its subtree is itself"
        );
        assert_eq!(
            u64_at(&leaf.payload, "node_count", "get_ui_tree root+ids_only")?,
            1,
            "the leaf's compact subtree is one node"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn load_ui_snapshot_ids_only_returns_the_saved_tree_ids() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // The measured default: loading T1's saved snapshot was 12,004 bytes.
        const SNAPSHOT: &str = "compact-results-t3";
        server
            .call_json("save_ui_snapshot", json!({ "name": SNAPSHOT }))
            .await?;

        let full = server
            .call_measured("load_ui_snapshot", json!({ "name": SNAPSHOT }))
            .await?;
        assert_default_over_the_cap("load_ui_snapshot", &full)?;
        let full_ids: BTreeSet<String> = object(&full.payload, "load_ui_snapshot")?
            .get("nodes")
            .and_then(JsonValue::as_object)
            .ok_or_else(|| format!("the loaded snapshot carries no nodes map: {}", full.payload))?
            .keys()
            .cloned()
            .collect();
        let roots = strings_at(&full.payload, "roots", "load_ui_snapshot")?;
        let generation = u64_at(&full.payload, "generation", "load_ui_snapshot")?;

        let compact = server
            .call_measured(
                "load_ui_snapshot",
                json!({ "name": SNAPSHOT, "ids_only": true }),
            )
            .await?;
        assert_compact_shape(
            "load_ui_snapshot",
            &compact.payload,
            &["generation", "node_count", "roots", "ids"],
        )?;
        assert_eq!(
            u64_at(&compact.payload, "generation", "load_ui_snapshot ids_only")?,
            generation,
            "the compact reply must carry the snapshot's generation"
        );
        assert_eq!(
            strings_at(&compact.payload, "roots", "load_ui_snapshot ids_only")?,
            roots,
            "the compact reply must carry the snapshot's roots"
        );
        let ids: BTreeSet<String> =
            strings_at(&compact.payload, "ids", "load_ui_snapshot ids_only")?
                .into_iter()
                .collect();
        assert!(
            !ids.is_empty(),
            "the saved snapshot has nodes, so ids must not be empty"
        );
        assert_eq!(
            u64::try_from(ids.len())
                .map_err(|error| format!("could not compare the node count: {error}"))?,
            u64_at(&compact.payload, "node_count", "load_ui_snapshot ids_only")?,
            "node_count must be the number of ids"
        );
        assert_eq!(ids, full_ids, "ids must be exactly the snapshot's nodes");
        assert_under_the_cap("load_ui_snapshot", &compact)
    })
    .await
}

#[tokio::test]
async fn get_frame_report_summary_only_keeps_the_scalars_summary_and_views() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // Park the pointer, start a measurement window, then move it across the
        // demo's two hover-styled control regions: every move hover-notifies one
        // region's view, so the report has frames and view activity to summarize
        // and the compact reply has something to drop. The moves go through
        // `pointer_move` because `hover_element` refuses a node that advertises no
        // Hover action, and the plain controls advertise none.
        let parking = server.center("heading").await?;
        let left = server.center("probe-left-target").await?;
        let right = server.center("probe-right-target").await?;
        server.pointer_move(parking).await?;
        server.call_json("mark_frames", json!({})).await?;
        for point in [left, right, left, right, left, right, left, right] {
            server.pointer_move(point).await?;
        }

        let full = server.call_measured("get_frame_report", json!({})).await?;
        let frames = array_at(&full.payload, "frames", "get_frame_report")?;
        if frames.is_empty() {
            return Err(
                "the settling hovers drew no frame, so dropping the frames array proves nothing"
                    .to_owned(),
            );
        }
        println!(
            "measure get_frame_report default reply: result_bytes={} frames={}",
            full.result_bytes,
            frames.len()
        );

        let compact = server
            .call_measured("get_frame_report", json!({ "summary_only": true }))
            .await?;
        assert_compact_shape(
            "get_frame_report",
            &compact.payload,
            &[
                "after_frame_count",
                "latest_frame_count",
                "truncated",
                "summary",
                "views",
            ],
        )?;
        assert_eq!(
            u64_at(
                &compact.payload,
                "after_frame_count",
                "get_frame_report summary_only"
            )?,
            u64_at(&full.payload, "after_frame_count", "get_frame_report")?,
            "the compact report must describe the same measurement window"
        );
        let compact_latest = u64_at(
            &compact.payload,
            "latest_frame_count",
            "get_frame_report summary_only",
        )?;
        let full_latest = u64_at(&full.payload, "latest_frame_count", "get_frame_report")?;
        if compact_latest < full_latest {
            return Err(format!(
                "latest_frame_count must not move backwards: {compact_latest} < {full_latest}"
            ));
        }
        bool_at(
            &compact.payload,
            "truncated",
            "get_frame_report summary_only",
        )?;
        let summary = object(
            field(&compact.payload, "summary", "get_frame_report summary_only")?,
            "get_frame_report summary",
        )?;
        if summary.get("frames").and_then(JsonValue::as_u64).is_none() {
            return Err(format!(
                "the compact summary must still carry the summarized frame count: {}",
                compact.payload
            ));
        }
        array_at(&compact.payload, "views", "get_frame_report summary_only")?;
        if compact.result_bytes >= full.result_bytes {
            return Err(format!(
                "the compact report must be smaller than the default one: {} vs {} bytes",
                compact.result_bytes, full.result_bytes
            ));
        }
        assert_under_the_cap("get_frame_report", &compact)
    })
    .await
}

#[tokio::test]
async fn record_performance_summary_only_stays_well_formed_and_bounded() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // The demo's default already fits (T1: 3,560 bytes), so this test does not
        // assert the default is over the cap: it proves the option itself is safe -
        // the compact reply keeps the reply's top-level shape and stays bounded.
        server.call_json("mark_frames", json!({})).await?;
        let compact = server
            .call_measured(
                "record_performance",
                json!({ "duration_ms": 200, "summary_only": true }),
            )
            .await?;
        // Printed before the shape assertions so a tree without the compact path
        // (where this test fails on the shape) still shows the settled size the
        // quiet wait produced.
        println!(
            "measure record_performance summary_only reply: result_bytes={}",
            compact.result_bytes
        );
        assert_compact_shape(
            "record_performance",
            &compact.payload,
            &[
                "duration_ms",
                "before",
                "after",
                "observed_frame_delta",
                "frames",
                "cadence_note",
            ],
        )?;
        assert_eq!(
            u64_at(
                &compact.payload,
                "duration_ms",
                "record_performance summary_only"
            )?,
            200,
            "duration_ms must be the requested interval"
        );
        let before = u64_at(
            field(
                &compact.payload,
                "before",
                "record_performance summary_only",
            )?,
            "frame_count",
            "record_performance before",
        )?;
        let after = u64_at(
            field(&compact.payload, "after", "record_performance summary_only")?,
            "frame_count",
            "record_performance after",
        )?;
        assert_eq!(
            u64_at(
                &compact.payload,
                "observed_frame_delta",
                "record_performance summary_only"
            )?,
            after.saturating_sub(before),
            "observed_frame_delta must be the frames completed in the interval"
        );
        let note = string_at(
            &compact.payload,
            "cadence_note",
            "record_performance summary_only",
        )?;
        if note.is_empty() {
            return Err("the cadence note must not be empty".to_owned());
        }
        // The embedded frames report stays, reduced exactly like get_frame_report.
        assert_compact_shape(
            "record_performance frames",
            field(
                &compact.payload,
                "frames",
                "record_performance summary_only",
            )?,
            &[
                "after_frame_count",
                "latest_frame_count",
                "truncated",
                "summary",
                "views",
            ],
        )?;
        assert_under_the_cap("record_performance", &compact)
    })
    .await
}

#[tokio::test]
async fn get_live_document_summary_only_is_accepted_on_the_unsupported_path() -> Result<(), String>
{
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // The demo registers no document host, so the documented reply is the
        // "Unsupported" error either way; the 81,210-byte document lives on the
        // runtime showcase, which this suite does not drive, so there is no compact
        // shape to assert here. What is pinned: with the flag the call is still
        // accepted (it answers a tools/call result, not a schema rejection) and the
        // documented reply is unchanged.
        let result = server
            .call_raw("get_live_document", json!({ "summary_only": true }))
            .await?;
        assert_unsupported("get_live_document", &result)
    })
    .await
}

#[tokio::test]
async fn preview_live_document_summary_only_is_accepted_on_the_unsupported_path()
-> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // Same caveat as get_live_document: the required preview arguments are
        // supplied, and on the demo the flag is accepted while the reply stays the
        // documented unsupported path.
        let result = server
            .call_raw(
                "preview_live_document",
                json!({
                    "expected_revision": 1,
                    "html": "<p>compact results</p>",
                    "css": "",
                    "bindings_ron": "()",
                    "summary_only": true,
                }),
            )
            .await?;
        assert_unsupported("preview_live_document", &result)
    })
    .await
}
