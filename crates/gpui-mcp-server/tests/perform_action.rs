//! `perform_action` on the demo fixture, over the real MCP stdio surface.
//!
//! `perform_action` is the tool that drives a control the way assistive
//! technology does: the caller names a semantic node and one accessibility
//! action — `click`, `increment`, `decrement`, `expand`, `collapse`, or
//! `set_value` — and the element's own handler runs on the UI thread with no
//! coordinates and no keystrokes. The bridge's unit tests cover the mapping
//! from the wire action to AccessKit and the element handler; what they cannot
//! show is the path a client actually takes: an MCP tool call, the bridge
//! request, GPUI's own dispatch, and a change in the running fixture.
//!
//! Three cases, one fixture window each:
//!
//! 1. A click on `increment` changes the counter, exactly once per call. The
//!    fixture's increment control registers a plain `on_click` handler and no
//!    accessibility action listener, so this is GPUI's built-in click
//!    handling, reached through the patched window.
//! 2. An action the node does not handle — a `set_value` on the click-only
//!    increment control, an `increment` on the counter that advertises
//!    nothing — is refused as a tool error that names the node and says it
//!    does not handle the action, and leaves the application alone.
//! 3. An unknown node id is refused as not found, never accepted.
//!
//! Where the click's effect is read is the one thing the wire contract does not
//! decide for the test: the demo's `count` node shows the counter as its text,
//! and the tree does not publish text for a plain container (a node is named
//! from its accessible label, or from its text for button-like roles, and the
//! tree's generation advances only when the semantic tree changed), so a click
//! that moves the counter leaves `get_ui_tree` and `get_element_state`
//! identical. The count *is* published on the same wire surface, though: every
//! increment writes `counter changed to N` through the bridge's automation and
//! `get_logs` returns those lines. Case 1 therefore reads the counter through
//! `get_logs` before and after — the change, not merely the acceptance — and
//! checks the tree stays readable across the calls. Making the count itself
//! visible in the tree would need a change under `src/`, which this test task
//! may not make; the report says so.
//!
//! Running needs the fixture built first — `cargo build -p gpui-mcp-demo -p
//! gpui-mcp-server`, since the demo binary is looked up beside the server
//! binary — and a desktop session: without `DISPLAY` or `WAYLAND_DISPLAY` each
//! test prints a note and passes without opening a window. One window is open
//! at a time, because the static `WINDOW` mutex is held for the whole of each
//! test body, and every wait is bounded by a deadline.

mod support;

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
/// How long the fixture is given to publish the line a performed action
/// produces. The call returns once the application has handled the action, so
/// the line is normally there on the first read; the deadline only bounds the
/// wait, it is not an expected delay.
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);

/// The fixture's counter control, and the node that shows the count it drives.
const INCREMENT: &str = "increment";
const COUNTER: &str = "count";

/// An id the fixture cannot publish: the only way to tell a not-found refusal
/// from an action that was accepted and silently did nothing.
const UNKNOWN_ID: &str = "no-such-node-2f9c";

/// The prefix the fixture stamps on every line its counter writes.
const COUNTER_LOG_PREFIX: &str = "counter changed to ";

/// One window at a time: each test opens a real window on the shared desktop,
/// and several at once make the fixture flaky.
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

    /// The MCP handshake, then wait until the fixture is discoverable.
    async fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "gpui-mcp-perform-action-test", "version": "0.0.0" },
            }),
        )
        .await?;
        self.send(
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
        )
        .await?;
        support::wait_for_fixture(self, &[INCREMENT, COUNTER], DISCOVERY_DEADLINE).await
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

    /// A `tools/call` reply exactly as it arrived, `isError` included.
    async fn call_raw(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
        .await
    }

    /// The structured payload of a tool that answers with JSON.
    async fn call_json(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        let result = self.call_raw(tool, arguments).await?;
        if result.get("isError").and_then(JsonValue::as_bool) == Some(true) {
            return Err(format!("{tool} reported an error: {}", reply_text(&result)));
        }
        payload(tool, &result)
    }

    /// The error text of a tool call that must fail. A call that succeeds is
    /// returned as an `Err`, so the caller's `?` reports it as a test failure:
    /// a refused action must never be a success.
    async fn call_error(&mut self, tool: &str, arguments: JsonValue) -> Result<String, String> {
        match self.call_raw(tool, arguments).await {
            // A protocol-level error is an error too, and its text must name
            // the refused id as well.
            Err(error) => Ok(error),
            Ok(result) => {
                if result.get("isError").and_then(JsonValue::as_bool) == Some(true) {
                    Ok(reply_text(&result))
                } else {
                    Err(format!(
                        "{tool} succeeded; this call must be refused, never accepted: {result}"
                    ))
                }
            }
        }
    }

    /// The counter lines the fixture has published, oldest first. The fixture
    /// writes one per performed step and one on reset, so the lines are both
    /// the count's published value and its change history.
    async fn counter_lines(&mut self) -> Result<Vec<String>, String> {
        let logs = self
            .call_json("get_logs", json!({ "limit": 512 }))
            .await
            .map_err(|error| format!("could not read the fixture logs: {error}"))?;
        let entries = logs
            .get("entries")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| format!("the log reply carries no entries list: {logs}"))?;
        Ok(entries
            .iter()
            .filter_map(|entry| entry.get("message").and_then(JsonValue::as_str))
            .filter(|message| message.starts_with(COUNTER_LOG_PREFIX))
            .map(str::to_owned)
            .collect())
    }

    /// Wait, bounded, until the fixture has published exactly `expected`
    /// counter lines, and return them. The count is deliberately exact: a
    /// handler that ran twice for one call, or once and then repeated on its
    /// own, publishes more lines than the call it answered.
    async fn wait_for_counter(
        &mut self,
        expected: usize,
        deadline: Duration,
    ) -> Result<Vec<String>, String> {
        let started = Instant::now();
        loop {
            let observation = match self.counter_lines().await {
                Ok(lines) if lines.len() == expected => return Ok(lines),
                Ok(lines) => format!("it published {} counter lines: {lines:?}", lines.len()),
                Err(error) => error,
            };
            if started.elapsed() >= deadline {
                return Err(format!(
                    "the fixture never published {expected} counter lines within {deadline:?}: {observation}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

/// Whether this machine can open a window at all. The Linux CI job runs the
/// workspace suite without a display server, so these cases skip there;
/// windowed coverage comes from a desktop session — a local run, or the
/// Windows and macOS CI jobs.
fn has_a_desktop_session() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    } else {
        true
    }
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

impl support::FixtureClient for Server {
    async fn call_json(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        Server::call_json(self, tool, arguments).await
    }
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
        scenario(&mut server).await
    }
    .await;

    let outcome = outcome.map_err(|error| support::fixture_failure(&fixture.log, &error));
    server.stop().await;
    fixture.stop().await;
    outcome
}

/// The structured payload of a tool reply: `structuredContent` when the server
/// sends it, else the first text content entry, which is the JSON payload.
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
/// reply when it carries none, so an error whose text is elsewhere still gets
/// searched by the caller.
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

/// The nodes map of a tree reply.
fn nodes(tree: &JsonValue) -> Result<&serde_json::Map<String, JsonValue>, String> {
    tree.get("nodes")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| format!("the tree reply carries no nodes map: {tree}"))
}

/// One node of the reply.
fn node<'a>(tree: &'a JsonValue, id: &str) -> Result<&'a JsonValue, String> {
    nodes(tree)?.get(id).ok_or_else(|| {
        format!(
            "the tree carries no node {id}: {:?}",
            nodes(tree).map(|nodes| nodes.keys().collect::<Vec<_>>())
        )
    })
}

/// The action names a node of the reply advertises.
fn actions_of(tree: &JsonValue, id: &str) -> Result<Vec<String>, String> {
    node(tree, id)?
        .get("actions")
        .and_then(JsonValue::as_array)
        .map(|actions| {
            actions
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_owned)
                .collect()
        })
        .ok_or_else(|| format!("the node {id} carries no actions list"))
}

/// Whether the reply reports the node as visible.
fn is_visible(tree: &JsonValue, id: &str) -> Result<bool, String> {
    node(tree, id)?
        .get("state")
        .and_then(|state| state.get("visible"))
        .and_then(JsonValue::as_bool)
        .ok_or_else(|| format!("the node {id} carries no boolean state.visible"))
}

/// Whether an error says the node does not accept the action. Both spellings
/// the two layers of the stack use pass: the tool's own gate
/// (`element "increment" does not support SetValue (it advertises [Click])`)
/// and the bridge's refusal (`Unsupported: the semantic node does not handle
/// this action`), so the test states the contract rather than one layer's
/// wording.
fn says_it_does_not_handle(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("does not support") || error.contains("does not handle")
}

/// Whether an error says the requested node was not found, in either the
/// human-readable (`was not found`) or the protocol's code spelling
/// (`NotFound` / `not_found`).
fn says_not_found(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("not found") || error.contains("not_found")
}

#[tokio::test]
async fn a_click_changes_the_counter_exactly_once_per_call() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        // The tree must advertise the action this case performs: the increment
        // control carries a plain click handler, which is the GPUI fallback
        // `perform_action` reaches without any accessibility action listener.
        let before = server.call_json("get_ui_tree", json!({})).await?;
        assert!(
            actions_of(&before, INCREMENT)?.contains(&"click".to_owned()),
            "{INCREMENT} must advertise click, the action this case performs; it advertises {:?}",
            actions_of(&before, INCREMENT)?
        );
        assert!(
            is_visible(&before, COUNTER)?,
            "the node showing the count must be visible before the click"
        );
        let lines = server.counter_lines().await?;
        assert!(
            lines.is_empty(),
            "the fixture must start with a fresh counter: {lines:?} would make the per-click \
             count below meaningless"
        );

        // One call, one step. A handler that fired twice per call, or once and
        // then repeated on its own, publishes more lines than the calls below,
        // and one that never fired leaves the count at zero and fails the wait.
        for steps in 1..=2_usize {
            let acknowledgement = server
                .call_json(
                    "perform_action",
                    json!({ "id": INCREMENT, "action": { "action": "click" } }),
                )
                .await?;
            assert_eq!(
                acknowledgement,
                json!({ "ok": true, "action": "action_performed" }),
                "a handled action is acknowledged with the tool's own success payload"
            );
            let expected: Vec<String> = (1..=steps)
                .map(|count| format!("counter changed to {count}"))
                .collect();
            assert_eq!(
                server.wait_for_counter(steps, SETTLE_DEADLINE).await?,
                expected,
                "after {steps} click(s) the counter lines must be exactly one step per call"
            );
        }

        // The semantic surface stays readable and consistent across the
        // actions, so dispatching them did not disturb the tree a client reads.
        let after = server.call_json("get_ui_tree", json!({})).await?;
        assert!(
            actions_of(&after, INCREMENT)?.contains(&"click".to_owned()),
            "the increment control must still advertise click after the actions"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_action_the_node_does_not_handle_is_refused() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let tree = server.call_json("get_ui_tree", json!({})).await?;
        let advertised = actions_of(&tree, INCREMENT)?;
        assert!(
            !advertised.contains(&"set_value".to_owned()),
            "this case needs a node without a value handler, but {INCREMENT} advertises \
             {advertised:?}"
        );

        // A value replacement on a control that only handles clicks: refused
        // and explained, never accepted and silently ignored.
        let refusal = server
            .call_error(
                "perform_action",
                json!({ "id": INCREMENT, "action": { "action": "set_value", "value": "7" } }),
            )
            .await?;
        assert!(
            refusal.contains(INCREMENT),
            "the refusal must name the node it refused: {refusal}"
        );
        assert!(
            says_it_does_not_handle(&refusal),
            "the refusal must say the node does not handle the action: {refusal}"
        );

        // A node that advertises no action at all refuses the same way: the
        // caller must not be able to aim an action at whatever node happens to
        // be addressable.
        let bare = server
            .call_error(
                "perform_action",
                json!({ "id": COUNTER, "action": { "action": "increment" } }),
            )
            .await?;
        assert!(
            bare.contains(COUNTER),
            "the refusal must name the node it refused: {bare}"
        );
        assert!(
            says_it_does_not_handle(&bare),
            "the refusal must say the node does not handle the action: {bare}"
        );

        // A refused action must leave the application alone.
        let lines = server.counter_lines().await?;
        assert!(
            lines.is_empty(),
            "a refused action must not change the counter: {lines:?}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_unknown_node_id_is_not_found() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let tree = server.call_json("get_ui_tree", json!({})).await?;
        assert!(
            nodes(&tree)?.get(UNKNOWN_ID).is_none(),
            "the fixture must not publish {UNKNOWN_ID}, or this case tests nothing"
        );

        let refusal = server
            .call_error(
                "perform_action",
                json!({ "id": UNKNOWN_ID, "action": { "action": "click" } }),
            )
            .await?;
        assert!(
            refusal.contains(UNKNOWN_ID),
            "the refusal must name the requested id {UNKNOWN_ID}: {refusal}"
        );
        assert!(
            says_not_found(&refusal),
            "the refusal must say the node was not found: {refusal}"
        );
        Ok(())
    })
    .await
}
