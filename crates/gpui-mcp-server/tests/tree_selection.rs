//! `get_ui_tree` subtree selection, over the real MCP stdio surface.
//!
//! Goal 03 lets a caller ask for part of the rendered tree instead of all of it:
//! `root` selects one node and its descendants, `max_depth` bounds the levels
//! below the starting node(s), and `visible_only` drops nodes the application
//! reports as invisible. The point of the goal is cost — a reader that needs one
//! panel should not pay for the whole window — so the reply has to shrink
//! without the caller losing the shape of what it asked for.
//!
//! These tests drive `get_ui_tree` through the same JSON-RPC surface a client
//! uses, against the demo fixture: the fixture `disabled_state.rs` drives,
//! spawned beside the server binary with a private endpoint directory. The file
//! names a few fixture ids as constants (`ROOT`, `LEAF`, `BRANCH`,
//! `BRANCH_CHILD`, `SIBLING`, `NAMED_NODES`) to choose what to exercise, but
//! every expected id set, roots list and children list is computed from the
//! fixture's own whole-tree reply, so the assertions are about the argument
//! semantics and not about the fixture's layout: a subtree call must return
//! exactly the id set reachable from its root through the tree's own `children`
//! lists, and every returned node keeps the full `children` list the whole tree
//! gave it, even when the reply left a child out.
//!
//! Seven tests cover the three arguments, an unknown root, and the measurement.
//! Running them needs the fixture built first — `cargo build -p gpui-mcp-demo
//! -p gpui-mcp-server`, since the demo binary is looked up beside the server
//! binary — and a desktop session: without `DISPLAY` or `WAYLAND_DISPLAY` each
//! test prints a note and passes without opening a window. One window is open
//! at a time, because the static `WINDOW` mutex is held for the whole of each
//! test body. The last test records what one real subtree call saves against
//! the whole-tree baseline (node count, reply bytes, wall-clock), one printed
//! line per call, which `-- --nocapture` shows.

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

/// The fixture's nodes this file names: its root, a named node without
/// descendants, a named node with one descendant, that descendant, and the
/// sibling whose subtree must never leak into the other's.
const ROOT: &str = "demo-root";
const LEAF: &str = "increment";
const BRANCH: &str = "probe-left";
const BRANCH_CHILD: &str = "probe-left-target";
const SIBLING: &str = "probe-right";

/// An id the fixture cannot publish: the only way to tell an implementation
/// that reports an unknown `root` from one that answers it with an empty tree.
const UNKNOWN_ROOT: &str = "no-such-node-2f9c";

/// The named nodes the fixture publishes, all of which the whole tree carries.
const NAMED_NODES: [&str; 12] = [
    "heading",
    "count",
    "search",
    "filter",
    "increment",
    "reset",
    "lock-toggle",
    "locked-action",
    "probe-left",
    "probe-left-target",
    "probe-right",
    "probe-right-target",
];

/// One window at a time: each test opens a real window on the shared desktop,
/// and seven at once make the fixture flaky and the wall-clock measurement
/// meaningless. The guard is handed to `with_fixture` and lives for the body.
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

    /// The MCP handshake, then wait until the fixture is discoverable.
    async fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "gpui-mcp-tree-selection-test", "version": "0.0.0" },
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
        Ok(self.request_measured(method, params).await?.0)
    }

    /// One JSON-RPC reply, with the byte size of the line it arrived on.
    async fn request_measured(
        &mut self,
        method: &str,
        params: JsonValue,
    ) -> Result<(JsonValue, usize), String> {
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
            let result = reply
                .get("result")
                .cloned()
                .ok_or_else(|| format!("{method} returned neither a result nor an error"))?;
            return Ok((result, line.len()));
        }
    }

    /// A `tools/call` reply exactly as it arrived, `isError` included.
    async fn call_raw(
        &mut self,
        tool: &str,
        arguments: JsonValue,
    ) -> Result<(JsonValue, usize), String> {
        self.request_measured("tools/call", json!({ "name": tool, "arguments": arguments })).await
    }

    /// The structured payload of a tool that answers with JSON, with the byte
    /// size of the reply it came on.
    async fn call_measured(
        &mut self,
        tool: &str,
        arguments: JsonValue,
    ) -> Result<Measured, String> {
        let started = Instant::now();
        let (result, reply_bytes) = self.call_raw(tool, arguments).await?;
        let elapsed_ms = started.elapsed().as_millis();
        if result.get("isError").and_then(JsonValue::as_bool) == Some(true) {
            return Err(format!("{tool} reported an error: {}", reply_text(&result)));
        }
        Ok(Measured { payload: payload(tool, &result)?, reply_bytes, elapsed_ms })
    }

    /// The structured payload of a tool that answers with JSON.
    async fn call_json(&mut self, tool: &str, arguments: JsonValue) -> Result<JsonValue, String> {
        Ok(self.call_measured(tool, arguments).await?.payload)
    }

    /// The error text of a tool call that must fail. A call that succeeds is
    /// returned as an `Err`, so the caller's `?` reports it as a test failure.
    async fn call_error(&mut self, tool: &str, arguments: JsonValue) -> Result<String, String> {
        match self.call_raw(tool, arguments).await {
            // A protocol-level error is an error too, and its text must name the
            // id as well.
            Err(error) => Ok(error),
            Ok((result, _)) => {
                if result.get("isError").and_then(JsonValue::as_bool) == Some(true) {
                    Ok(reply_text(&result))
                } else {
                    Err(format!(
                        "{tool} succeeded; an unknown root must be an error, never a success: {result}"
                    ))
                }
            }
        }
    }

    async fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.kill().await;
    }
}

/// One `get_ui_tree` call with the numbers the acceptance measurement asks for:
/// the node count, the byte size of the reply, and the wall-clock time.
struct Measured {
    payload: JsonValue,
    reply_bytes: usize,
    elapsed_ms: u128,
}

impl Measured {
    fn node_count(&self) -> Result<usize, String> {
        Ok(nodes(&self.payload)?.len())
    }

    /// The byte size of the tree payload alone, without the JSON-RPC envelope.
    fn payload_bytes(&self) -> Result<usize, String> {
        serde_json::to_string(&self.payload)
            .map(|text| text.len())
            .map_err(|error| format!("could not serialize the tree payload: {error}"))
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

/// Wait until exactly the fixture is discoverable through the private endpoint
/// directory and has rendered the nodes every test reads: discovery precedes
/// the first frame, and a tree read before it is empty.
async fn wait_for_the_fixture(server: &mut Server) -> Result<(), String> {
    support::wait_for_fixture(server, &NAMED_NODES, DISCOVERY_DEADLINE).await
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
            content.iter().find_map(|entry| entry.get("text").and_then(JsonValue::as_str))
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
    if text.is_empty() { result.to_string() } else { text.join("\n") }
}

/// The nodes map of a tree reply.
fn nodes(tree: &JsonValue) -> Result<&serde_json::Map<String, JsonValue>, String> {
    tree.get("nodes")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| format!("the tree reply carries no nodes map: {tree}"))
}

/// The ids the reply carries.
fn ids(tree: &JsonValue) -> Result<BTreeSet<String>, String> {
    Ok(nodes(tree)?.keys().cloned().collect())
}

/// The roots the reply selected.
fn roots(tree: &JsonValue) -> Result<Vec<String>, String> {
    tree.get("roots")
        .and_then(JsonValue::as_array)
        .map(|roots| roots.iter().filter_map(JsonValue::as_str).map(str::to_owned).collect())
        .ok_or_else(|| format!("the tree reply carries no roots list: {tree}"))
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

/// The ordered child ids a node of the reply lists, which is the list the whole
/// tree published even when this reply left a child out.
fn children_of(tree: &JsonValue, id: &str) -> Result<Vec<String>, String> {
    node(tree, id)?
        .get("children")
        .and_then(JsonValue::as_array)
        .map(|children| children.iter().filter_map(JsonValue::as_str).map(str::to_owned).collect())
        .ok_or_else(|| format!("the node {id} carries no children list"))
}

/// Whether the reply reports the node as visible.
fn is_visible(tree: &JsonValue, id: &str) -> Result<bool, String> {
    node(tree, id)?
        .get("state")
        .and_then(|state| state.get("visible"))
        .and_then(JsonValue::as_bool)
        .ok_or_else(|| format!("the node {id} carries no boolean state.visible"))
}

/// The ids the reply reports as visible.
fn visible_ids(tree: &JsonValue) -> Result<BTreeSet<String>, String> {
    let mut visible = BTreeSet::new();
    for id in ids(tree)? {
        if is_visible(tree, &id)? {
            visible.insert(id);
        }
    }
    Ok(visible)
}

/// The ids within `max_depth` levels of `start`, read out of `tree`'s own
/// children lists. `None` is unlimited, `Some(0)` is the starting node alone.
/// It must be run on a whole tree: a child the reply does not carry is an error,
/// since a pruned reply legitimately leaves one out.
fn ids_within_depth(
    tree: &JsonValue,
    start: &str,
    max_depth: Option<usize>,
) -> Result<BTreeSet<String>, String> {
    children_of(tree, start)?; // the starting node must exist
    let carried = nodes(tree)?;
    let mut seen = BTreeSet::new();
    let mut pending = vec![(start.to_owned(), 0_usize)];
    while let Some((id, depth)) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue; // a cycle the tree's diagnostics should have broken
        }
        if max_depth.is_some_and(|limit| depth >= limit) {
            continue;
        }
        for child in children_of(tree, &id)? {
            if !carried.contains_key(&child) {
                return Err(format!(
                    "the tree lists {child} as a child of {id} without carrying it"
                ));
            }
            pending.push((child, depth + 1));
        }
    }
    Ok(seen)
}

/// The ids reachable from the reply's roots through its children lists. Unlike
/// `ids`, a node that names a child the reply does not carry is an error, so the
/// whole-tree baseline cannot hide a broken tree.
fn reachable_ids(tree: &JsonValue) -> Result<BTreeSet<String>, String> {
    let mut seen = BTreeSet::new();
    let mut pending = roots(tree)?;
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        pending.extend(children_of(tree, &id)?);
    }
    Ok(seen)
}

/// The skip note every test prints when it cannot open a window.
#[expect(
    clippy::print_stderr,
    reason = "a test without a desktop session says why it passed without opening a window"
)]
fn skip_without_a_desktop() -> bool {
    if has_a_desktop_session() {
        false
    } else {
        eprintln!("skipping: this machine has no desktop session to open a window on");
        true
    }
}

#[tokio::test]
async fn no_arguments_returns_the_whole_fixture_tree() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let whole = server.call_json("get_ui_tree", json!({})).await?;
        if whole.get("generation").and_then(JsonValue::as_u64).is_none() {
            return Err(format!("the whole tree carries no generation: {whole}"));
        }
        let tree_ids = ids(&whole)?;
        for named in NAMED_NODES {
            if !tree_ids.contains(named) {
                return Err(format!(
                    "the whole tree must carry the fixture's {named}; it carries {tree_ids:?}"
                ));
            }
        }
        assert_eq!(
            roots(&whole)?,
            vec![ROOT.to_owned()],
            "the fixture publishes {ROOT} as its only root"
        );
        assert_eq!(
            reachable_ids(&whole)?,
            tree_ids,
            "every node of the whole tree must be reachable from its roots, and every child a node names must be present"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn root_selects_one_node_and_its_subtree() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let whole = server.call_json("get_ui_tree", json!({})).await?;

        // A named node with no descendants: the reply is exactly that node, so a
        // call that ignores `root` and answers with the whole tree fails here.
        let leaf = server.call_json("get_ui_tree", json!({ "root": LEAF })).await?;
        assert_eq!(roots(&leaf)?, vec![LEAF.to_owned()], "roots must name the requested node");
        assert_eq!(
            ids(&leaf)?,
            ids_within_depth(&whole, LEAF, None)?,
            "{LEAF} has no descendants in the fixture, so its subtree is itself"
        );
        assert_eq!(
            children_of(&leaf, LEAF)?,
            children_of(&whole, LEAF)?,
            "the requested node keeps its whole children list"
        );

        // A named node with a descendant: the reply carries the descendant, and
        // nothing of the sibling's subtree.
        let branch = server.call_json("get_ui_tree", json!({ "root": BRANCH })).await?;
        let expected = ids_within_depth(&whole, BRANCH, None)?;
        assert!(
            expected.contains(BRANCH_CHILD),
            "the fixture's {BRANCH} publishes {BRANCH_CHILD}, so the subtree must include it"
        );
        assert_eq!(ids(&branch)?, expected, "{BRANCH} must return exactly its own subtree");
        assert_eq!(roots(&branch)?, vec![BRANCH.to_owned()]);
        assert!(
            !ids(&branch)?.contains(SIBLING),
            "{SIBLING} is outside {BRANCH}'s subtree and must not be returned"
        );
        assert_eq!(
            children_of(&branch, BRANCH)?,
            children_of(&whole, BRANCH)?,
            "the requested node keeps its whole children list"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn max_depth_bounds_the_levels_below_the_starting_node() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let whole = server.call_json("get_ui_tree", json!({})).await?;
        let all = ids(&whole)?;

        let only_start = server
            .call_json("get_ui_tree", json!({ "root": ROOT, "max_depth": 0 }))
            .await?;
        assert_eq!(roots(&only_start)?, vec![ROOT.to_owned()]);
        assert_eq!(
            ids(&only_start)?,
            BTreeSet::from([ROOT.to_owned()]),
            "max_depth: 0 returns the starting node alone"
        );
        let root_children = children_of(&whole, ROOT)?;
        assert!(
            !root_children.is_empty(),
            "the fixture's {ROOT} publishes children"
        );
        assert_eq!(
            children_of(&only_start, ROOT)?,
            root_children,
            "the starting node keeps its full children list even though max_depth: 0 left every child out"
        );

        let direct = server
            .call_json("get_ui_tree", json!({ "root": ROOT, "max_depth": 1 }))
            .await?;
        let one_level = ids_within_depth(&whole, ROOT, Some(1))?;
        assert_eq!(
            ids(&direct)?,
            one_level,
            "max_depth: 1 returns the starting node and its direct children, nothing deeper"
        );
        assert_eq!(roots(&direct)?, vec![ROOT.to_owned()]);
        let mut pruned_a_child = false;
        for id in &one_level {
            let whole_children = children_of(&whole, id)?;
            assert_eq!(
                children_of(&direct, id)?,
                whole_children,
                "the node {id} must keep its full children list even when max_depth left a child out"
            );
            pruned_a_child |= whole_children.iter().any(|child| !one_level.contains(child));
        }
        assert!(
            pruned_a_child,
            "max_depth: 1 must leave a child of a returned node out, or the kept-children checks above are vacuous"
        );

        let unlimited = server
            .call_json("get_ui_tree", json!({ "root": ROOT }))
            .await?;
        assert_eq!(
            ids(&unlimited)?,
            all,
            "without max_depth the whole subtree of {ROOT} comes back"
        );
        assert!(
            all.len() > one_level.len(),
            "an unlimited call must return more than max_depth: 1: {} vs {}",
            all.len(),
            one_level.len()
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[expect(
    clippy::print_stdout,
    reason = "the measured reply goes into the run's log for the report"
)]
async fn visible_only_drops_the_nodes_reported_as_invisible() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let whole = server.call_json("get_ui_tree", json!({})).await?;
        let all = ids(&whole)?;
        let filtered = server
            .call_json("get_ui_tree", json!({ "visible_only": true }))
            .await?;
        let kept = ids(&filtered)?;
        assert!(!kept.is_empty(), "visible_only must not empty the tree");
        assert_eq!(
            kept,
            visible_ids(&whole)?,
            "visible_only must return exactly the nodes the whole tree reports as visible"
        );
        let dropped: Vec<String> = all.difference(&kept).cloned().collect();
        if dropped.is_empty() {
            println!(
                "note: the fixture publishes no node with state.visible == false ({} nodes checked), so visible_only has nothing to drop on it; the filtering itself is pinned by the implementation's in-memory unit tests",
                all.len()
            );
        }
        let explicit_false = server
            .call_json("get_ui_tree", json!({ "visible_only": false }))
            .await?;
        assert_eq!(
            ids(&explicit_false)?,
            all,
            "visible_only defaults to false, so an explicit false is the whole tree"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn root_max_depth_and_visible_only_compose() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let whole = server.call_json("get_ui_tree", json!({})).await?;
        let by_root = server
            .call_json("get_ui_tree", json!({ "root": ROOT }))
            .await?;
        let by_depth = server
            .call_json("get_ui_tree", json!({ "max_depth": 1 }))
            .await?;
        let by_visibility = server
            .call_json("get_ui_tree", json!({ "visible_only": true }))
            .await?;
        let combined = server
            .call_json(
                "get_ui_tree",
                json!({ "root": ROOT, "max_depth": 1, "visible_only": true }),
            )
            .await?;

        // max_depth without a root starts at the tree's own roots, and the
        // fixture's only root is `demo-root`.
        assert_eq!(
            ids(&by_depth)?,
            ids_within_depth(&whole, ROOT, Some(1))?,
            "max_depth without root starts at the tree's roots"
        );
        assert_eq!(
            ids(&by_root)?,
            ids(&whole)?,
            "root: {ROOT} is the whole fixture tree"
        );

        let root_ids = ids(&by_root)?;
        let depth_ids = ids(&by_depth)?;
        let visible = ids(&by_visibility)?;
        let mut expected = root_ids.clone();
        expected.retain(|id| depth_ids.contains(id) && visible.contains(id));
        assert_eq!(
            ids(&combined)?,
            expected,
            "the combined call must equal the root set ∩ the max_depth set ∩ the visible set, never a different set with the same size"
        );
        assert_eq!(
            roots(&combined)?,
            vec![ROOT.to_owned()],
            "roots names the requested starting node, not the nodes the filters kept"
        );
        assert_eq!(
            children_of(&combined, ROOT)?,
            children_of(&whole, ROOT)?,
            "the starting node keeps its full children list after both filters"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_unknown_root_is_an_error_naming_the_requested_id() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let error = server.call_error("get_ui_tree", json!({ "root": UNKNOWN_ROOT })).await?;
        assert!(
            error.contains(UNKNOWN_ROOT),
            "the error must name the requested id {UNKNOWN_ROOT}: {error}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[expect(
    clippy::print_stdout,
    reason = "the measured reply goes into the run's log for the report"
)]
async fn a_whole_tree_and_a_subtree_call_are_measured() -> Result<(), String> {
    if skip_without_a_desktop() {
        return Ok(());
    }
    with_fixture(WINDOW.lock().await, async |server: &mut Server| {
        let whole = server.call_measured("get_ui_tree", json!({})).await?;
        println!(
            "measure get_ui_tree {{}} whole tree: nodes={} reply_bytes={} tree_bytes={} elapsed_ms={}",
            whole.node_count()?,
            whole.reply_bytes,
            whole.payload_bytes()?,
            whole.elapsed_ms
        );
        let subtree = server
            .call_measured("get_ui_tree", json!({ "root": ROOT, "max_depth": 1 }))
            .await?;

        // The measured call has to be the selection it claims to be, or the
        // numbers are not the ones the acceptance asks for.
        assert_eq!(
            ids(&subtree.payload)?,
            ids_within_depth(&whole.payload, ROOT, Some(1))?,
            "the measured subtree call must still be the max_depth: 1 selection"
        );
        println!(
            "measure get_ui_tree {{\"root\":\"demo-root\",\"max_depth\":1}} subtree: nodes={} reply_bytes={} tree_bytes={} elapsed_ms={}",
            subtree.node_count()?,
            subtree.reply_bytes,
            subtree.payload_bytes()?,
            subtree.elapsed_ms
        );
        assert!(
            whole.node_count()? > subtree.node_count()?,
            "the subtree call must return fewer nodes than the whole tree: {} vs {}",
            whole.node_count()?,
            subtree.node_count()?
        );
        assert!(
            subtree.reply_bytes < whole.reply_bytes,
            "the subtree reply must be smaller than the whole-tree reply: {} vs {}",
            subtree.reply_bytes,
            whole.reply_bytes
        );
        Ok(())
    })
    .await
}
