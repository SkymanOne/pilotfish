#![allow(clippy::unwrap_used)]

//! Port of `tests/mcp-server.test.ts` and `tests/mcp-stdio.test.ts` against
//! the real `pilotfish mcp` binary over stdio, with the scripted fake pi
//! (`tests/fixtures/fake-pi-pilotfish.mjs`): the spawn → wait → report → status →
//! refusal → cleanup flow, a merge conflict ending in exit 5, the
//! `PILOTFISH_DIR`-derived fleet directory, malformed input not killing the
//! server, and proof that nothing but protocol reaches stdout. Hermetic: no
//! network, no tokens.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use pilotfish::fleet::run::{self, RunState};
use pilotfish::paths::FleetPaths;
use serde_json::{Value, json};

/// The fake pi settles this long after its work turn.
const FAKE_PI_DELAY_MS: &str = "200";

/// Each test here boots the real binary plus a node fake pi and a detached
/// monitor. Running them concurrently spikes the CPU enough to trip the
/// timing-sensitive monitor suite that shares the `cargo test` run, so they
/// go one at a time; nothing else in these tests is order-dependent.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn fake_pi() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-pi-pilotfish.mjs")
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo() -> PathBuf {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    // Leak the TempDir guard: the fleet outlives the test while its detached
    // monitor settles, and the OS reclaims /tmp anyway.
    std::mem::forget(dir);
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "seed"]);
    root
}

/// A plain (non-git) directory for runs without a worktree.
fn plain_dir() -> PathBuf {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    root
}

// ---------------------------------------------------------------------------
// A minimal synchronous MCP client over the real binary's stdio.
// ---------------------------------------------------------------------------

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// Every raw stdout line ever observed, for the purity assertion.
    lines: Vec<String>,
}

impl McpClient {
    /// Spawn `pilotfish mcp [--cwd root]` with the given extra environment.
    fn spawn(root: Option<&Path>, extra_env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(assert_cmd::cargo_bin!("pilotfish"));
        // never the developer's own ~/.pilotfish, which can switch routing on
        // and hold a real key
        command.args(["mcp"]).env(
            "PILOTFISH_HOME",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/no-user-home"),
        );
        if let Some(root) = root {
            // The child must never inherit an ambient PILOTFISH_DIR: its fleet is
            // the one this test created, resolved from `--cwd`.
            command
                .arg("--cwd")
                .arg(root)
                .env("PILOTFISH_DIR", root.join(pilotfish::paths::STATE_DIR_NAME));
        }
        command
            .env("PILOTFISH_PI_BIN", format!("node {}", fake_pi().display()))
            .env("FAKE_PI_DELAY_MS", FAKE_PI_DELAY_MS)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            lines: Vec::new(),
        };
        client.initialize();
        client
    }

    fn send_raw(&mut self, raw: &str) {
        self.stdin.write_all(raw.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    fn send(&mut self, value: &Value) {
        self.send_raw(&value.to_string());
    }

    /// Read stdout until the response with `id` arrives, skipping
    /// notifications and stray responses. Every line must be JSON.
    fn read_response(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).unwrap();
            assert!(
                read > 0,
                "the server closed stdout while waiting for id {id}"
            );
            let trimmed = line.trim_end();
            self.lines.push(trimmed.to_owned());
            let message: Value = serde_json::from_str(trimmed).unwrap_or_else(|err| {
                panic!("stdout carried non-protocol bytes: {trimmed:?}: {err}")
            });
            let is_response = message.get("method").is_none();
            if is_response && message["id"] == json!(id) {
                return message;
            }
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.read_response(id)["result"].clone()
    }

    fn initialize(&mut self) -> Value {
        let result = self.request(
            "initialize",
            &json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "pilotfish-test", "version": "0"},
            }),
        );
        assert_eq!(result["protocolVersion"], "2025-06-18", "{result}");
        assert_eq!(result["serverInfo"]["name"], "fleet", "{result}");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        result
    }

    fn list_tools(&mut self) -> Vec<Value> {
        self.request("tools/list", &json!({}))
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap()
    }

    /// The `tools/call` result object; an ops refusal is a result here, only
    /// a protocol-level failure would error.
    fn call_tool(&mut self, name: &str, arguments: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }));
        let response = self.read_response(id);
        assert!(
            response["error"].is_null(),
            "tools/call {name} was a protocol error: {response}"
        );
        response["result"].clone()
    }

    /// Every line the server ever wrote parsed as protocol JSON.
    fn assert_pure_stdout(&self) {
        assert!(!self.lines.is_empty(), "no protocol was observed on stdout");
        for line in &self.lines {
            let parsed: Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("non-protocol stdout line {line:?}: {err}"));
            assert!(
                parsed.get("jsonrpc").is_some(),
                "stdout line without a jsonrpc field: {line}"
            );
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Closing stdin is how a client disconnects; the server exits.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// tests — the stdio halves of tests/mcp-stdio.test.ts
// ---------------------------------------------------------------------------

#[test]
fn stdio_transport() {
    {
        let _serial = serial();
        let root = plain_dir();
        let mut client = McpClient::spawn(Some(&root), &[]);
        let tools = client.list_tools();
        let mut names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        names.sort_unstable();
        let mut expected = pilotfish::mcp::server::FLEET_TOOL_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected, "{names:?}");

        let status = client.call_tool("fleet_status", &json!({}));
        assert_eq!(status["isError"], json!(false), "{status}");
        assert_eq!(text_of(&status), "(no runs)\nexit: 0");
        client.assert_pure_stdout();
    }
    {
        let _serial = serial();
        let root = plain_dir();
        let paths = FleetPaths::new(root.join(pilotfish::paths::STATE_DIR_NAME));
        let run_id = "w-20260828141530";
        std::fs::create_dir_all(paths.run_dir(run_id)).unwrap();
        let mut state = RunState::new(
            paths.root().to_str().unwrap(),
            run_id,
            "w",
            &root.to_string_lossy(),
            "b",
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
        // A live pid past the starting grace reads as running.
        state.status = pilotfish::fleet::run::RunStatus::Running;
        state.pid = Some(1);
        run::save_state(&paths.run_dir(run_id), &state).unwrap();

        let mut client =
            McpClient::spawn(None, &[("PILOTFISH_DIR", paths.root().to_str().unwrap())]);
        let status = client.call_tool("fleet_status", &json!({}));
        assert_eq!(status["isError"], json!(false), "{status}");
        let text = text_of(&status);
        assert!(text.contains('w') && text.contains("running"), "{text}");
        let runs = status["structuredContent"]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["name"], "w");
        client.assert_pure_stdout();
    }
    {
        let _serial = serial();
        let root = plain_dir();
        let mut client = McpClient::spawn(Some(&root), &[]);
        // Garbage before and between well-formed requests.
        client.send_raw("this is not json");
        client.send_raw("[1, 2, 3]");
        let tools = client.list_tools();
        assert_eq!(tools.len(), 13);
        client.send_raw("}{ garbage }{");
        let status = client.call_tool("fleet_status", &json!({}));
        assert_eq!(text_of(&status), "(no runs)\nexit: 0");
        client.assert_pure_stdout();
    }
}

// ---------------------------------------------------------------------------
// tests — the flow half of tests/mcp-server.test.ts
// ---------------------------------------------------------------------------

#[test]
fn tool_cycle() {
    {
        let _serial = serial();
        let root = plain_dir();
        let mut client = McpClient::spawn(Some(&root), &[]);

        let spawned = client.call_tool(
            "fleet_spawn",
            &json!({"name": "hello", "brief": "write hello.txt", "worktree": false}),
        );
        assert_eq!(spawned["isError"], json!(false), "{}", text_of(&spawned));
        let run_id = spawned["structuredContent"]["runId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            regex::Regex::new(r"^hello-[0-9a-f]{7}$")
                .unwrap()
                .is_match(&run_id),
            "{run_id}"
        );
        assert_eq!(spawned["structuredContent"]["worktree"], json!(null));
        let text = text_of(&spawned);
        assert!(text.contains("Spawned hello-"), "{text}");
        assert!(text.ends_with("exit: 0"), "{text}");

        let waited = client.call_tool("fleet_wait", &json!({"name": "hello", "timeoutSec": 30}));
        assert_eq!(waited["isError"], json!(false), "{}", text_of(&waited));
        assert_eq!(text_of(&waited), "hello settled\nexit: 0");

        let report = client.call_tool("fleet_report", &json!({"name": "hello"}));
        assert_eq!(report["isError"], json!(false), "{}", text_of(&report));
        assert!(
            text_of(&report).contains("## Status\ndone"),
            "{}",
            text_of(&report)
        );

        let status = client.call_tool("fleet_status", &json!({}));
        let runs = status["structuredContent"]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["name"], "hello");
        assert_eq!(runs[0]["status"], "settled");
        assert!(
            text_of(&status).contains("hello") && text_of(&status).contains("settled"),
            "{status}"
        );
        let one = client.call_tool("fleet_status", &json!({"name": "hello"}));
        assert_eq!(
            one["structuredContent"]["runs"][0]["taskBrief"],
            "write hello.txt"
        );

        let output = client.call_tool("fleet_output", &json!({"name": "hello", "tail": 3}));
        assert!(
            text_of(&output).starts_with("bash: hi"),
            "{}",
            text_of(&output)
        );
        let logs = client.call_tool("fleet_logs", &json!({"name": "hello", "tail": 2}));
        assert!(
            text_of(&logs).contains("agent_settled"),
            "{}",
            text_of(&logs)
        );

        let answer = client.call_tool("fleet_answer", &json!({"name": "hello", "answer": "x"}));
        assert_eq!(answer["isError"], json!(true), "{}", text_of(&answer));
        let text = text_of(&answer);
        assert!(text.contains("nothing is waiting for an answer"), "{text}");
        assert!(text.ends_with("exit: 1"), "{text}");
        let send = client.call_tool("fleet_send", &json!({"name": "hello", "message": "again"}));
        assert_eq!(send["isError"], json!(true));
        assert!(
            text_of(&send).contains("steering refused"),
            "{}",
            text_of(&send)
        );
        let merge = client.call_tool("fleet_merge", &json!({"name": "hello"}));
        assert_eq!(merge["isError"], json!(true));
        assert!(
            text_of(&merge).contains("has no branch"),
            "{}",
            text_of(&merge)
        );
        let missing = client.call_tool("fleet_report", &json!({"name": "nope"}));
        assert_eq!(missing["isError"], json!(true));
        assert!(
            text_of(&missing).contains("No run found matching \"nope\""),
            "{}",
            text_of(&missing)
        );

        // An unresolvable model is refused before anything is created: exit 2.
        let bad_model = client.call_tool(
            "fleet_spawn",
            &json!({"name": "badmodel", "brief": "b", "model": "no-such-model", "worktree": false}),
        );
        assert_eq!(bad_model["isError"], json!(true), "{}", text_of(&bad_model));
        let text = text_of(&bad_model);
        assert!(text.contains("spawn: unknown model"), "{text}");
        assert!(text.ends_with("exit: 2"), "{text}");

        let cleanup = client.call_tool("fleet_cleanup", &json!({"target": "hello"}));
        assert_eq!(cleanup["isError"], json!(false), "{}", text_of(&cleanup));
        assert!(
            text_of(&cleanup).starts_with("archived hello-"),
            "{}",
            text_of(&cleanup)
        );
        client.assert_pure_stdout();
    }
    {
        let _serial = serial();
        let root = init_repo();
        let mut client = McpClient::spawn(Some(&root), &[("FAKE_PI_WRITE_HELLO", "1")]);

        let spawned = client.call_tool(
            "fleet_spawn",
            &json!({"name": "hello", "brief": "write hello.txt"}),
        );
        assert_eq!(spawned["isError"], json!(false), "{}", text_of(&spawned));
        let worktree = PathBuf::from(spawned["structuredContent"]["worktree"].as_str().unwrap());
        assert!(
            spawned["structuredContent"]["branch"].as_str().is_some(),
            "a worktree run has a branch"
        );
        let waited = client.call_tool("fleet_wait", &json!({"name": "hello", "timeoutSec": 30}));
        assert_eq!(waited["isError"], json!(false), "{}", text_of(&waited));

        // The fake pi wrote hello.txt but did not commit; diff sees nothing
        // committed and warns about the uncommitted file.
        let dirty = client.call_tool("fleet_diff", &json!({"name": "hello"}));
        assert_eq!(dirty["isError"], json!(false), "{}", text_of(&dirty));
        let text = text_of(&dirty);
        assert!(text.contains("(no changes)"), "{text}");
        assert!(
            text.contains("uncommitted change(s)") && text.contains("hello.txt"),
            "{text}"
        );
        assert!(text.ends_with("exit: 0"), "{text}");

        // The worker commits; the diff then shows the committed work.
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-qm", "worker hello"]);
        let committed = client.call_tool("fleet_diff", &json!({"name": "hello"}));
        assert!(
            text_of(&committed).contains("hello.txt"),
            "{}",
            text_of(&committed)
        );

        // A conflicting change lands on the main checkout.
        std::fs::write(root.join("hello.txt"), "different\n").unwrap();
        git(&root, &["add", "hello.txt"]);
        git(&root, &["commit", "-qm", "conflict"]);

        let merge = client.call_tool("fleet_merge", &json!({"name": "hello"}));
        assert_eq!(merge["isError"], json!(true), "{}", text_of(&merge));
        let text = text_of(&merge);
        assert!(text.contains("conflicts in:\nhello.txt"), "{text}");
        assert!(
            text.contains("merge was aborted; the checkout is clean"),
            "{text}"
        );
        assert!(
            text.contains("rebase its branch pilotfish/hello-"),
            "{text}"
        );
        assert!(text.ends_with("exit: 5"), "{text}");

        // The checkout is clean apart from the gitignore spawn introduced.
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output()
            .unwrap();
        let porcelain_text = String::from_utf8_lossy(&status.stdout).into_owned();
        let porcelain: Vec<&str> = porcelain_text
            .lines()
            .filter(|line| !line.is_empty() && *line != "?? .gitignore")
            .collect();
        assert_eq!(porcelain, Vec::<&str>::new(), "{porcelain:?}");
        assert!(
            !root.join(".git").join("MERGE_HEAD").exists(),
            "the merge abort removed MERGE_HEAD"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "different\n"
        );

        let cleanup = client.call_tool("fleet_cleanup", &json!({"target": "hello", "force": true}));
        assert_eq!(cleanup["isError"], json!(false), "{}", text_of(&cleanup));
        client.assert_pure_stdout();
    }
}
