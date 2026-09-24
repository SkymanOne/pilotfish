#![allow(clippy::unwrap_used)]

//! In-process port of the `tests/mcp-server.test.ts` assertions that do not
//! need a spawned worker: the tool list and schemas, result rendering through
//! the wire format, protocol errors, wait's exit codes, and `fleet_answer`
//! resolving a pending `fleet_ask` question or a pi dialog with orchestrator
//! provenance. The full spawn → wait → report flow and stdio discipline run
//! against the real binary in `mcp_stdio.rs`.

use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use pilotfish::cli::ExitCode;
use pilotfish::fleet::envelope::{DEFAULT_ORCHESTRATOR_SESSION, Decoded, Envelope, Party};
use pilotfish::fleet::run::{self, PendingDialog, PendingQuestion, RunState, RunStatus};
use pilotfish::mcp::server::{FLEET_TOOL_NAMES, FleetServer};
use pilotfish::paths::FleetPaths;
use rmcp::service::ServiceExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};

/// One fleet dir anchored at `<dir>/.pilotfish` with a run whose state is already
/// on disk — the same shape the ops tests prepare.
struct Fleet {
    dir: tempfile::TempDir,
    paths: FleetPaths,
    run_id: String,
}

impl Fleet {
    fn new(_prefix: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = FleetPaths::new(dir.path().join(pilotfish::paths::STATE_DIR_NAME));
        // The run id derives from the name the way spawn stamps it, so
        // `find_run("w")` matches it.
        let run_id = "w-20260828141530".to_owned();
        std::fs::create_dir_all(paths.run_dir(&run_id)).unwrap();
        Self { dir, paths, run_id }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write_state_with(&self, tweak: impl FnOnce(&mut RunState)) {
        let mut state = RunState::new(
            self.paths.root().to_str().unwrap(),
            &self.run_id,
            "w",
            self.root().to_str().unwrap(),
            "write hello",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        tweak(&mut state);
        run::save_state(&self.paths.run_dir(&self.run_id), &state).unwrap();
    }

    fn write_state(&self, status: RunStatus) {
        self.write_state_with(|state| state.status = status);
    }

    fn inbox(&self) -> Vec<Envelope> {
        let raw = std::fs::read_to_string(self.paths.run_inbox(&self.run_id)).unwrap_or_default();
        raw.lines()
            .filter(|line| !line.is_empty())
            .map(Envelope::parse_line)
            .collect::<Option<Vec<_>>>()
            .unwrap()
    }
}

// ---------------------------------------------------------------------------
// A tiny async JSON-RPC client over one half of a tokio duplex; the server
// gets the other half, split into its read and write sides.
// ---------------------------------------------------------------------------

/// The client end of the duplex: reads and writes both go through this half,
/// cross-connected to the server's half.
struct DuplexClientEnd {
    read: ReadHalf<tokio::io::DuplexStream>,
    write: WriteHalf<tokio::io::DuplexStream>,
}

impl AsyncRead for DuplexClientEnd {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl AsyncWrite for DuplexClientEnd {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

/// A minimal MCP client: one request in flight at a time, responses matched
/// by id, notifications skipped.
struct Client {
    io: tokio::io::BufReader<DuplexClientEnd>,
    next_id: u64,
}

impl Client {
    async fn connect(fleet: &Fleet) -> (Self, tokio::task::JoinHandle<()>) {
        // The fleet dir is pinned: the ambient `PILOTFISH_DIR` must never
        // redirect the server's per-call resolution to another fleet.
        let server = FleetServer::with_pilotfish_dir(
            Some(fleet.root().to_path_buf()),
            Some(fleet.paths.root().to_string_lossy().into_owned()),
        );
        let (client_half, server_half) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server_half);
        let handle = tokio::spawn(async move {
            let running: rmcp::service::RunningService<rmcp::service::RoleServer, FleetServer> =
                match server.serve((server_read, server_write)).await {
                    Ok(running) => running,
                    Err(_) => return,
                };
            let _ = running.waiting().await;
        });
        let (client_read, client_write) = tokio::io::split(client_half);
        let mut client = Self {
            io: tokio::io::BufReader::new(DuplexClientEnd {
                read: client_read,
                write: client_write,
            }),
            next_id: 0,
        };
        client.initialize().await;
        (client, handle)
    }

    async fn send(&mut self, value: &Value) {
        use tokio::io::AsyncWriteExt as _;
        self.io
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        self.io.flush().await.unwrap();
    }

    /// Read the next line-bearing message, skipping notifications.
    async fn read_message(&mut self) -> Value {
        use tokio::io::AsyncBufReadExt as _;
        let mut line = String::new();
        loop {
            line.clear();
            let read = self.io.read_line(&mut line).await.unwrap();
            assert!(read > 0, "server closed its stdout mid-test");
            let message: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|err| panic!("stdout line was not JSON: {line:?}: {err}"));
            if message.get("method").is_some() && message.get("id").is_none() {
                continue; // a notification, not a response
            }
            return message;
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let response = self.read_message().await;
        assert_eq!(
            response["id"],
            json!(id),
            "response id mismatch: {response}"
        );
        response["result"].clone()
    }

    /// A request whose JSON-RPC error is part of the assertion.
    async fn request_error(&mut self, method: &str, params: Value) -> Value {
        let response = self.read_full_response(method, params).await;
        let error = &response["error"];
        assert!(!error.is_null(), "expected an error response: {response}");
        error.clone()
    }

    /// Send a request and get the whole response message back.
    async fn read_full_response(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let response = self.read_message().await;
        assert_eq!(
            response["id"],
            json!(id),
            "response id mismatch: {response}"
        );
        response
    }

    async fn initialize(&mut self) -> Value {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "pilotfish-test", "version": "0"},
                }),
            )
            .await;
        // The negotiated version comes back, not an error.
        assert_eq!(result["protocolVersion"], "2025-06-18", "{result}");
        assert_eq!(result["serverInfo"]["name"], "fleet", "{result}");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        result
    }

    async fn list_tools(&mut self) -> Vec<Value> {
        self.request("tools/list", json!({}))
            .await
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap()
    }

    /// The `tools/call` result object (content, isError, structuredContent).
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// The `tools/call` error object, for protocol-level failures.
    async fn call_tool_error(&mut self, name: &str, arguments: Value) -> Value {
        self.request_error("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }
}

/// The joined text of a tool result's content blocks.
fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Close the client and give the server a bounded window to quit: the
/// service loop ends only when the transport's other half is dropped, so a
/// stuck loop must fail the test instead of hanging it.
async fn shutdown(client: Client, server: tokio::task::JoinHandle<()>) {
    drop(client);
    if tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .is_err()
    {
        panic!("server did not quit after the client disconnected");
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lists_fleet_tools() {
    let fleet = Fleet::new("mcp-list-");
    let (mut client, server) = Client::connect(&fleet).await;
    let tools = client.list_tools().await;
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    let mut expected = FLEET_TOOL_NAMES.to_vec();
    expected.sort_unstable();
    assert_eq!(sorted, expected, "{names:?}");

    let spawn = tools.iter().find(|t| t["name"] == "fleet_spawn").unwrap();
    assert_eq!(spawn["inputSchema"]["required"], json!(["name", "brief"]));
    assert!(
        spawn["outputSchema"].is_object(),
        "fleet_spawn declares structured output"
    );
    let send = tools.iter().find(|t| t["name"] == "fleet_send").unwrap();
    assert!(
        send["outputSchema"].is_null(),
        "fleet_send has no structured output"
    );
    // Only spawn and status carry output schemas.
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        let declared = tool["outputSchema"].is_object();
        assert_eq!(
            declared,
            matches!(name, "fleet_spawn" | "fleet_status"),
            "{name}"
        );
    }
    shutdown(client, server).await;
}

#[tokio::test]
async fn status_and_wait_tools() {
    {
        let fleet = Fleet::new("mcp-empty-");
        let (mut client, server) = Client::connect(&fleet).await;
        let status = client.call_tool("fleet_status", json!({})).await;
        assert_eq!(status["isError"], json!(false), "{status}");
        assert_eq!(text_of(&status), "(no runs)\nexit: 0");
        assert_eq!(status["structuredContent"]["runs"], json!([]));
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-wait-");
        // A live pid keeps the run non-terminal until the deadline lapses.
        fleet.write_state_with(|state| {
            state.status = RunStatus::Running;
            state.pid = Some(1);
        });
        let (mut client, server) = Client::connect(&fleet).await;
        let timed_out = client
            .call_tool("fleet_wait", json!({"name": "w", "timeoutSec": 1}))
            .await;
        assert_eq!(timed_out["isError"], json!(true), "{timed_out}");
        assert!(
            text_of(&timed_out).contains("timed out after 1s"),
            "{timed_out}"
        );
        assert!(text_of(&timed_out).ends_with("exit: 3"), "{timed_out}");

        let stopped = Fleet::new("mcp-wait2-");
        stopped.write_state(RunStatus::Stopped);
        let (mut client2, server2) = Client::connect(&stopped).await;
        let ended = client2.call_tool("fleet_wait", json!({"name": "w"})).await;
        assert_eq!(ended["isError"], json!(true), "{ended}");
        assert_eq!(text_of(&ended), "w stopped\nexit: 4");
        shutdown(client, server).await;
        shutdown(client2, server2).await;
    }
    {
        let fleet = Fleet::new("mcp-wait3-");
        fleet.write_state(RunStatus::Settled);
        let (mut client, server) = Client::connect(&fleet).await;
        let settled = client.call_tool("fleet_wait", json!({"name": "w"})).await;
        assert_eq!(settled["isError"], json!(false), "{settled}");
        assert_eq!(text_of(&settled), "w settled\nexit: 0");
        shutdown(client, server).await;
    }
}

#[tokio::test]
async fn error_kinds() {
    {
        let fleet = Fleet::new("mcp-ghost-");
        let (mut client, server) = Client::connect(&fleet).await;
        let report = client
            .call_tool("fleet_report", json!({"name": "nope"}))
            .await;
        assert_eq!(report["isError"], json!(true), "{report}");
        let text = text_of(&report);
        assert!(
            text.contains("No run found matching \"nope\"") && text.ends_with("exit: 1"),
            "{text}"
        );
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-args-");
        let (mut client, server) = Client::connect(&fleet).await;
        // Missing required argument.
        let error = client
            .call_tool_error("fleet_spawn", json!({"name": "x"}))
            .await;
        assert_eq!(error["code"], json!(-32602), "{error}");
        // Empty string where min(1) applies.
        let error = client
            .call_tool_error("fleet_send", json!({"name": "w", "message": ""}))
            .await;
        assert_eq!(error["code"], json!(-32602), "{error}");
        // Wrong type.
        let error = client
            .call_tool_error("fleet_wait", json!({"name": "w", "timeoutSec": "soon"}))
            .await;
        assert_eq!(error["code"], json!(-32602), "{error}");
        // timeoutSec above the 600 cap.
        let error = client
            .call_tool_error("fleet_wait", json!({"name": "w", "timeoutSec": 601}))
            .await;
        assert_eq!(error["code"], json!(-32602), "{error}");
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-unknown-");
        let (mut client, server) = Client::connect(&fleet).await;
        let error = client.call_tool_error("fleet_teleport", json!({})).await;
        assert_eq!(error["code"], json!(-32602), "{error}");
        assert!(
            error["message"].as_str().unwrap().contains("Unknown tool"),
            "{error}"
        );
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-brief-");
        let (mut client, server) = Client::connect(&fleet).await;
        // An empty string fails the min(1) schema: a protocol error.
        let error = client
            .call_tool_error("fleet_spawn", json!({"name": "x", "brief": ""}))
            .await;
        assert_eq!(error["code"], json!(-32602), "{error}");
        // A whitespace brief passes the schema and is refused by the core: a
        // tool error carrying exit 1.
        let refused = client
            .call_tool("fleet_spawn", json!({"name": "x", "brief": "  "}))
            .await;
        assert_eq!(refused["isError"], json!(true), "{refused}");
        assert!(
            text_of(&refused).contains("task brief required"),
            "{refused}"
        );
        assert!(text_of(&refused).ends_with("exit: 1"), "{refused}");
        // Nothing was created.
        assert!(
            run::list_runs(fleet.paths.root()).is_empty(),
            "the refused spawn created nothing"
        );
        shutdown(client, server).await;
    }
}

#[tokio::test]
async fn steering_tools() {
    {
        let fleet = Fleet::new("mcp-answer-");
        fleet.write_state_with(|state| {
            state.status = RunStatus::Running;
            state.pid = Some(1);
            state.pending_question = Some(PendingQuestion {
                id: "m_q1".into(),
                question: "which fixture?".into(),
                options: None,
                context: None,
                asked_at: pilotfish::util::now_iso(),
            });
        });
        let (mut client, server) = Client::connect(&fleet).await;

        let answered = client
            .call_tool("fleet_answer", json!({"name": "w", "answer": "argon2"}))
            .await;
        assert_eq!(answered["isError"], json!(false), "{answered}");
        assert_eq!(
            text_of(&answered),
            "answer queued for w (question m_q1)\nexit: 0"
        );
        let envelopes = fleet.inbox();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(
            envelopes[0].from,
            Party::Orchestrator(DEFAULT_ORCHESTRATOR_SESSION),
            "honest provenance"
        );
        assert_eq!(
            envelopes[0].decode(),
            Some(Decoded::Answer {
                message: Some("argon2"),
                question_id: Some("m_q1")
            })
        );
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-answer2-");
        fleet.write_state_with(|state| {
            state.status = RunStatus::Running;
            state.pid = Some(1);
        });
        let (mut client, server) = Client::connect(&fleet).await;
        let refused = client
            .call_tool("fleet_answer", json!({"name": "w", "answer": "x"}))
            .await;
        assert_eq!(refused["isError"], json!(true), "{refused}");
        assert!(
            text_of(&refused).contains("no pending question"),
            "{refused}"
        );
        assert!(text_of(&refused).ends_with("exit: 1"), "{refused}");
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-dialog-");
        fleet.write_state_with(|state| {
            state.status = RunStatus::Running;
            state.pid = Some(1);
            state.pending_dialog = Some(PendingDialog {
                id: "ui-9".into(),
                method: "confirm".into(),
                question: "overwrite?".into(),
                options: None,
                context: None,
                asked_at: pilotfish::util::now_iso(),
            });
        });
        let (mut client, server) = Client::connect(&fleet).await;
        let answered = client
            .call_tool("fleet_answer", json!({"name": "w", "answer": "yes"}))
            .await;
        assert_eq!(answered["isError"], json!(false), "{answered}");
        assert!(text_of(&answered).contains("(question ui-9)"), "{answered}");
        let envelopes = fleet.inbox();
        assert_eq!(
            envelopes[0].decode(),
            Some(Decoded::Answer {
                message: Some("yes"),
                question_id: Some("ui-9")
            })
        );
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-steer-");
        fleet.write_state(RunStatus::Settled);
        let (mut client, server) = Client::connect(&fleet).await;
        let send = client
            .call_tool("fleet_send", json!({"name": "w", "message": "again"}))
            .await;
        assert_eq!(send["isError"], json!(true), "{send}");
        let text = text_of(&send);
        assert!(text.contains("is settled — steering refused"), "{text}");
        assert!(text.contains("pilotfish spawn w-2 --session"), "{text}");
        assert!(text.ends_with("exit: 1"), "{text}");
        // A refused stop reads "nothing to stop" with the same exit code.
        let stop = client.call_tool("fleet_stop", json!({"name": "w"})).await;
        assert!(text_of(&stop).contains("nothing to stop"), "{stop}");
        assert_eq!(stop["isError"], json!(true));
        // Nothing reached the inbox.
        assert!(
            fleet.inbox().is_empty(),
            "the refusals left the inbox untouched"
        );
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-merge-");
        fleet.write_state_with(|state| {
            state.status = RunStatus::Running;
            state.pid = Some(1);
        });
        let (mut client, server) = Client::connect(&fleet).await;
        let merge = client.call_tool("fleet_merge", json!({"name": "w"})).await;
        assert_eq!(merge["isError"], json!(true), "{merge}");
        let text = text_of(&merge);
        assert!(
            text.contains("is running — only settled runs can be merged"),
            "{text}"
        );
        assert!(text.ends_with("exit: 1"), "{text}");
        shutdown(client, server).await;
    }
    {
        let fleet = Fleet::new("mcp-exits-");
        fleet.write_state(RunStatus::Settled);
        let (mut client, server) = Client::connect(&fleet).await;
        let stop = client.call_tool("fleet_stop", json!({"name": "w"})).await;
        assert!(text_of(&stop).ends_with("exit: 1"), "{stop}");
        let report = client.call_tool("fleet_report", json!({"name": "w"})).await;
        assert!(text_of(&report).ends_with("exit: 2"), "no report: {report}");
        shutdown(client, server).await;
        assert_eq!(ExitCode::MergeConflict as u8, 5, "the merge-conflict code");
    }
}
