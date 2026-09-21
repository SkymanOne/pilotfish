//! The stdio MCP server: text-first results (the CLI's stdout/stderr lines
//! plus a trailing `exit: N`), `isError` when the exit code is non-zero, and
//! structured content where a caller benefits. One tool per ops core, served
//! over stdio; the server name stays `fleet`, so the tools stay
//! `mcp__fleet__*`. (Ported from the TypeScript `src/mcp/server.ts`.)
//!
//! stdout is the MCP protocol — nothing else may write to it. The ops cores
//! return their lines instead of printing precisely so this module can render
//! them as tool output; `print_result` is never called here and diagnostics
//! belong on stderr (rmcp logs through `tracing`, which stays silent without
//! a subscriber). A tool call never depends on the parent's environment: the
//! cores resolve the fleet dir from `PARL_DIR` when set, else from the
//! `--cwd` target, else the process directory.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer, ServiceExt as _};
use serde::Serialize;
use serde_json::json;
use serde_json::{Map, Value};

use crate::cli::ExitCode;
use crate::fleet::envelope::Party;
use crate::ops::integrate::{cleanup_runs, diff_core_with_env, merge_core_with_env};
use crate::ops::query::{
    logs_core_with_env, output_core_with_env, report_core_with_env, status_core_with_env,
    wait_core_with_env,
};
use crate::ops::spawn::{SpawnRequest, spawn_core_with_env};
use crate::ops::steer::{
    answer_core_with_env, followup_core_with_env, send_core_with_env, stop_core_with_env,
};
use crate::ops::{CommandResult, resolve_fleet_dir_with_env};

/// The 13 fleet tools, in the order the orchestrator's prompt lists them.
pub const FLEET_TOOL_NAMES: [&str; 13] = [
    "fleet_spawn",
    "fleet_status",
    "fleet_wait",
    "fleet_output",
    "fleet_logs",
    "fleet_send",
    "fleet_followup",
    "fleet_answer",
    "fleet_stop",
    "fleet_report",
    "fleet_diff",
    "fleet_merge",
    "fleet_cleanup",
];

/// The MCP server name; claude prefixes every tool with `mcp__<name>__`.
pub const SERVER_NAME: &str = "fleet";
/// Serve the fleet tools over stdio until the client disconnects. A client
/// that closes stdin without initializing has disconnected, not failed.
///
/// # Errors
///
/// Fails when the stdio transport cannot start or the server errors after
/// initialization; a client that disconnects before `initialize` exits 0.
pub async fn serve_stdio(cwd: Option<&Path>) -> anyhow::Result<ExitCode> {
    let server = FleetServer::new(cwd.map(Path::to_path_buf));
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        // Closed before `initialize`: the orchestrator went away.
        Err(rmcp::service::ServerInitializeError::ConnectionClosed(_)) => {
            return Ok(ExitCode::Ok);
        }
        Err(err) => return Err(err.into()),
    };
    running.waiting().await?;
    Ok(ExitCode::Ok)
}

/// The fleet as an MCP server. Holds only the caller's target directory;
/// every tool re-resolves the fleet dir per call so a changed `PARL_DIR` or
/// a moved repo is picked up instead of cached.
#[derive(Debug, Clone, Default)]
pub struct FleetServer {
    /// Target directory (the repo being orchestrated); `None` means the
    /// process's working directory. Provenance for steering is always
    /// [`Party::Orchestrator`]: the agent driving these tools.
    cwd: Option<PathBuf>,
    /// A pinned `$PARL_DIR` value for the per-call resolution. Production
    /// leaves this unset so every call re-reads the environment; the
    /// in-process tests pin their own fleet dir instead.
    pinned_parl_dir: Option<String>,
}

impl FleetServer {
    /// A server for `cwd` (the repo being orchestrated).
    pub const fn new(cwd: Option<PathBuf>) -> Self {
        Self {
            cwd,
            pinned_parl_dir: None,
        }
    }

    /// [`FleetServer::new`] with the `$PARL_DIR` value pinned for every
    /// per-call resolution instead of re-read from the environment. Tests
    /// pin their own fleet dir so an ambient variable cannot redirect the
    /// tools to an unrelated fleet.
    #[doc(hidden)]
    pub const fn with_parl_dir(cwd: Option<PathBuf>, parl_dir: Option<String>) -> Self {
        Self {
            cwd,
            pinned_parl_dir: parl_dir,
        }
    }

    fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The `$PARL_DIR` value for one tool call: the pinned value when the
    /// server was built with one (tests), else the ambient environment,
    /// re-read per call so a changed `PARL_DIR` is picked up.
    fn parl_dir(&self) -> Option<String> {
        self.pinned_parl_dir
            .clone()
            .or_else(crate::ops::ambient_parl_dir)
    }

    /// The party steering runs under: the fleet's acting session — resolved
    /// from the same `cwd`/`$PARL_DIR` every tool call uses, so the
    /// envelope never parts company with the ownership bookkeeping — or the
    /// default orchestrator session when the fleet has no session rows yet.
    /// The default's on-wire spelling is the bare `"orchestrator"` the
    /// tools have always written.
    async fn tool_party(&self) -> Party {
        let fleet =
            crate::ops::resolve_fleet_dir_with_env(self.cwd(), self.parl_dir().as_deref()).await;
        let session = fleet
            .ok()
            .map(|f| crate::ops::acting_session(f.paths.root()));
        Party::Orchestrator(session.unwrap_or(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION))
    }

    /// Route one `tools/call` to its ops core. Argument problems and unknown
    /// tools are protocol errors; everything the core itself refused comes
    /// back as a tool result with its exit code.
    async fn dispatch(&self, name: &str, args: &JsonObject) -> Result<CallToolResult, McpError> {
        match name {
            "fleet_spawn" => self.fleet_spawn(args).await,
            "fleet_status" => self.fleet_status(args).await,
            "fleet_wait" => self.fleet_wait(args).await,
            "fleet_output" => self.fleet_output(args).await,
            "fleet_logs" => self.fleet_logs(args).await,
            "fleet_send" => self.fleet_send(args).await,
            "fleet_followup" => self.fleet_followup(args).await,
            "fleet_answer" => self.fleet_answer(args).await,
            "fleet_stop" => self.fleet_stop(args).await,
            "fleet_report" => self.fleet_report(args).await,
            "fleet_diff" => self.fleet_diff(args).await,
            "fleet_merge" => self.fleet_merge(args).await,
            "fleet_cleanup" => self.fleet_cleanup(args).await,
            other => Err(McpError::invalid_params(
                format!("Unknown tool: {other}"),
                None,
            )),
        }
    }

    async fn fleet_spawn(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let request = SpawnRequest {
            name: req_str(args, "name")?,
            brief: req_str(args, "brief")?,
            cwd: self.cwd.clone(),
            model: opt_str(args, "model")?,
            provider: opt_str(args, "provider")?,
            thinking: opt_str(args, "thinking")?,
            worktree: opt_bool(args, "worktree")?.unwrap_or(true),
            base: opt_str(args, "base")?,
            skill: opt_str(args, "skill")?,
            append_system_prompt: opt_str(args, "appendSystemPrompt")?,
            session: opt_str(args, "session")?,
            tools: opt_str(args, "tools")?,
            exclude_tools: opt_str(args, "excludeTools")?,
            route: opt_bool(args, "route")?,
        };
        match spawn_core_with_env(request, self.parl_dir().as_deref()).await {
            Ok(r) => {
                let structured = structured_data(&r);
                Ok(render_result(&r, structured))
            }
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_status(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = opt_str(args, "name")?;
        let all = opt_bool(args, "all")?.unwrap_or(false);
        match status_core_with_env(
            name.as_deref(),
            self.cwd(),
            false,
            all,
            self.parl_dir().as_deref(),
        )
        .await
        {
            Ok(r) => {
                let structured = structured_data(&r);
                Ok(render_result(&r, structured))
            }
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_wait(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let timeout = opt_u64(args, "timeoutSec", 1, Some(600))?.unwrap_or(120);
        match wait_core_with_env(&name, self.cwd(), timeout, self.parl_dir().as_deref()).await {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_output(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let tail = tail_arg(args, "tail")?;
        match output_core_with_env(&name, self.cwd(), tail, self.parl_dir().as_deref()).await {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_logs(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let tail = tail_arg(args, "tail")?;
        match logs_core_with_env(&name, self.cwd(), tail, self.parl_dir().as_deref()).await {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_send(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let message = req_str(args, "message")?;
        match send_core_with_env(
            &name,
            self.cwd(),
            &message,
            self.tool_party().await,
            self.parl_dir().as_deref(),
        )
        .await
        {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_followup(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let message = req_str(args, "message")?;
        match followup_core_with_env(
            &name,
            self.cwd(),
            &message,
            self.tool_party().await,
            self.parl_dir().as_deref(),
        )
        .await
        {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_answer(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let answer = req_str(args, "answer")?;
        let question_id = opt_str(args, "questionId")?;
        match answer_core_with_env(
            &name,
            self.cwd(),
            question_id.as_deref(),
            &answer,
            self.tool_party().await,
            self.parl_dir().as_deref(),
        )
        .await
        {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_stop(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        match stop_core_with_env(
            &name,
            self.cwd(),
            self.tool_party().await,
            self.parl_dir().as_deref(),
        )
        .await
        {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_report(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        match report_core_with_env(&name, self.cwd(), self.parl_dir().as_deref()).await {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_diff(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let name_only = opt_bool(args, "nameOnly")?.unwrap_or(false);
        match diff_core_with_env(&name, self.cwd(), name_only, self.parl_dir().as_deref()).await {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_merge(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let name = req_str(args, "name")?;
        let no_commit = opt_bool(args, "noCommit")?.unwrap_or(false);
        match merge_core_with_env(&name, self.cwd(), no_commit, self.parl_dir().as_deref()).await {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }

    async fn fleet_cleanup(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let target = req_str(args, "target")?;
        let force = opt_bool(args, "force")?.unwrap_or(false);
        let fleet = match resolve_fleet_dir_with_env(self.cwd(), self.parl_dir().as_deref()).await {
            Ok(fleet) => fleet,
            Err(err) => return Ok(render_error(&err)),
        };
        match cleanup_runs(fleet.paths.root(), &target, force).await {
            Ok(r) => Ok(render_result(&r, None)),
            Err(err) => Ok(render_error(&err)),
        }
    }
}

impl ServerHandler for FleetServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION"));
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: fleet_tools(),
            ..ListToolsResult::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        let result = self.dispatch(&request.name, &args).await?;
        Ok(CallToolResponse::Complete(result))
    }
}

// ---------------------------------------------------------------------------
// Result rendering — the port of `toToolResult`.
// ---------------------------------------------------------------------------

/// The ops core's structured data as JSON, when it serializes.
fn structured_data<T: Serialize>(r: &CommandResult<T>) -> Option<Value> {
    serde_json::to_value(&r.data).ok()
}

/// Render a core result for the model: `out` lines, then `err` lines, then
/// the trailing `exit: N` the orchestrator branches on. `isError` on a
/// non-zero exit; structured content attached where the tool declares an
/// output schema (spawn, status).
fn render_result<T: Serialize>(r: &CommandResult<T>, structured: Option<Value>) -> CallToolResult {
    let mut lines = Vec::with_capacity(r.out.len() + r.err.len() + 1);
    lines.extend(r.out.iter().cloned());
    lines.extend(r.err.iter().cloned());
    lines.push(format!("exit: {}", r.code as u8));
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(lines.join("\n"))];
    result.is_error = Some(r.code != ExitCode::Ok);
    result.structured_content = structured;
    result
}

/// A core that failed outright (unknown run, unreadable state): the error
/// message plus `exit: 1`. The TypeScript `errorResult`.
fn render_error(err: &anyhow::Error) -> CallToolResult {
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(format!("{err:#}\nexit: 1"))];
    result.is_error = Some(true);
    result
}

// ---------------------------------------------------------------------------
// Tool definitions — product copy the orchestrating model reads to decide
// what to call and when. Carried over from `src/mcp/server.ts` near
// verbatim, with two updates for this rewrite: the report now lives at
// `runs/<runId>/report.md`, and a worker can be blocked on a pi dialog as
// well as a `fleet_ask` question (`fleet_answer` resolves either).
// ---------------------------------------------------------------------------

/// Every tool definition, for `tools/list`.
fn fleet_tools() -> Vec<Tool> {
    let mut tools = Vec::with_capacity(FLEET_TOOL_NAMES.len());
    tools.extend(spawn_status_tools());
    tools.extend(query_tools());
    tools.extend(steering_tools());
    tools.extend(integration_tools());
    tools
}
/// The spawn and status tools; the structured-output pair.
fn spawn_status_tools() -> Vec<Tool> {
    vec![
        make_tool(
            "fleet_spawn",
            "Spawn a pi worker",
            "Start a headless pi worker on a task brief. By default it works on its own git worktree and branch; \
             set worktree=false for read-only steps (research, review). The brief must be self-contained: the worker \
             sees nothing else. Returns the run id; fleet events arrive as the run progresses. Each orchestrator \
             session may hold at most [limits] max_workers_per_session live workers (default 3); exit 1 with the \
             holders named when this session is at its cap. exit 0 on success.",
            properties([
                (
                    "name",
                    string_prop("Short kebab-case run name, e.g. add-auth"),
                ),
                (
                    "brief",
                    string_prop("The complete task brief for the worker"),
                ),
                ("model", string_prop("pi model pattern")),
                ("provider", string_prop("pi provider")),
                ("thinking", string_prop("pi thinking level")),
                (
                    "worktree",
                    bool_prop("Isolate in a git worktree (default true)"),
                ),
                (
                    "base",
                    string_prop("Base ref for the worker branch (default HEAD)"),
                ),
                ("skill", string_prop("Extra pi skill file or directory")),
                (
                    "appendSystemPrompt",
                    string_prop("Text appended to the pi system prompt"),
                ),
                (
                    "session",
                    string_prop(
                        "pi session file or id to resume (from a previous run's refusal/resume hint)",
                    ),
                ),
                ("tools", string_prop("pi tool allowlist (comma-separated)")),
                (
                    "excludeTools",
                    string_prop("pi tool denylist (comma-separated)"),
                ),
                (
                    "route",
                    bool_prop(
                        "Choose the model, thinking level and worktree from the brief (overrides the configured default)",
                    ),
                ),
            ]),
            &["name", "brief"],
            Some(object_schema([
                ("runId", string_schema()),
                ("runDir", string_schema()),
                ("fleetDir", string_schema()),
                ("worktree", nullable_string_schema()),
                ("branch", nullable_string_schema()),
            ])),
        ),
        make_tool(
            "fleet_status",
            "Fleet status",
            "The fleet table (name, state, last activity, last tool, steer count, age), or one run's full state with \
             name. States: starting, running, blocked (waiting on a fleet_answer question or a pi dialog), settled, \
             stopped, error, dead, archived. The table shows this session's runs; all=true is the whole-fleet \
             diagnostic view (every session's runs, archived included). Events are pushed to you as they happen; do \
             not poll this in a loop.",
            properties([
                (
                    "name",
                    string_prop("Run name for the full state of one run"),
                ),
                (
                    "all",
                    bool_prop("Include archived runs and other sessions' runs (whole-fleet view)"),
                ),
            ]),
            &[],
            Some(object_schema([
                (
                    "runs",
                    json_schema(json!({
                        "type": "array",
                        "items": {"type": "object"},
                        "description": "One derived run state per run, newest first",
                    })),
                ),
                (
                    "models",
                    json_schema(json!({
                        "type": "array",
                        "items": {"type": "object"},
                        "description":
                            "The models pi has configured, to choose from when spawning",
                    })),
                ),
            ])),
        ),
    ]
}

/// The query tools: wait, output, logs.
fn query_tools() -> Vec<Tool> {
    vec![
        make_tool(
            "fleet_wait",
            "Wait for a run",
            "Block until the run reaches a terminal state or the timeout passes. exit 0 settled, 3 timed out \
             (still running), 4 stopped/error/dead. Use only when you have nothing else to do; fleet events are \
             pushed to you anyway.",
            properties([
                (
                    "name",
                    string_prop(
                        "Run name (the newest non-archived run of that name) or a full run id",
                    ),
                ),
                (
                    "timeoutSec",
                    integer_prop("Seconds to wait (default 120, max 600)", 1, Some(600)),
                ),
            ]),
            &["name"],
            None,
        ),
        make_tool(
            "fleet_output",
            "Worker output",
            "The worker's last assistant text, or with tail=N the last N tool results (an activity trail).",
            properties([
                (
                    "name",
                    string_prop(
                        "Run name (the newest non-archived run of that name) or a full run id",
                    ),
                ),
                (
                    "tail",
                    integer_prop("Print the last N tool results instead", 1, None),
                ),
            ]),
            &["name"],
            None,
        ),
        make_tool(
            "fleet_logs",
            "Worker RPC log",
            "The tail of the worker's raw pi RPC log; use it to diagnose error or dead runs.",
            properties([
                (
                    "name",
                    string_prop(
                        "Run name (the newest non-archived run of that name) or a full run id",
                    ),
                ),
                ("tail", integer_prop("Lines to print (default 50)", 1, None)),
            ]),
            &["name"],
            None,
        ),
    ]
}

/// The steering tools: send, follow-up, answer, stop.
fn steering_tools() -> Vec<Tool> {
    let shared_name = "Run name (the newest non-archived run of that name) or a full run id";
    vec![
        make_tool(
            "fleet_send",
            "Steer a worker",
            "Send a steering message to a running worker; delivered after its current tool calls. exit 1 if the run \
             is finished (the message then includes the resume command).",
            properties([
                ("name", string_prop(shared_name)),
                ("message", string_prop("The steering message")),
            ]),
            &["name", "message"],
            None,
        ),
        make_tool(
            "fleet_followup",
            "Queue a follow-up",
            "Queue a message for after the worker finishes its current work. exit 1 if the run is finished.",
            properties([
                ("name", string_prop(shared_name)),
                ("message", string_prop("The follow-up message")),
            ]),
            &["name", "message"],
            None,
        ),
        make_tool(
            "fleet_answer",
            "Answer a worker question",
            "Answer a question the worker asked through fleet_ask, or resolve the pi dialog it is blocked on — \
             either way the worker stays blocked until you answer. Targets the run's pending question or dialog \
             unless questionId is given. exit 1 when nothing is pending.",
            properties([
                ("name", string_prop(shared_name)),
                ("answer", string_prop("The answer, or the dialog choice")),
                (
                    "questionId",
                    string_prop("Question id from the fleet event (default: the pending one)"),
                ),
            ]),
            &["name", "answer"],
            None,
        ),
        make_tool(
            "fleet_stop",
            "Stop a worker",
            "Abort a running worker; its state becomes stopped. exit 1 if it already finished.",
            properties([("name", string_prop(shared_name))]),
            &["name"],
            None,
        ),
    ]
}

/// The integration tools: report, diff, merge, cleanup.
fn integration_tools() -> Vec<Tool> {
    let shared_name = "Run name (the newest non-archived run of that name) or a full run id";
    vec![
        make_tool(
            "fleet_report",
            "Worker report",
            "The worker's final fleet report (Status, Summary, What I did, Files changed, Verification, Decisions, \
             Steering received, Open questions, Suggested next step) from runs/<runId>/report.md, plus the steering \
             log. Falls back to the last assistant text; exit 2 when there is neither.",
            properties([("name", string_prop(shared_name))]),
            &["name"],
            None,
        ),
        make_tool(
            "fleet_diff",
            "Worker diff",
            "What the worker committed on its branch versus the commit it started from (git diff --stat, or names \
             only).",
            properties([
                ("name", string_prop(shared_name)),
                ("nameOnly", bool_prop("List changed file names only")),
            ]),
            &["name"],
            None,
        ),
        make_tool(
            "fleet_merge",
            "Merge a worker branch",
            "Merge a settled worker's branch into the repository checkout. exit 5 on conflicts: the merge is aborted \
             and the checkout left clean; have the worker rebase in its worktree and merge again. Run the project's \
             integration checks after merging.",
            properties([
                ("name", string_prop(shared_name)),
                ("noCommit", bool_prop("Stage the merge without committing")),
            ]),
            &["name"],
            None,
        ),
        make_tool(
            "fleet_cleanup",
            "Clean up runs",
            "Remove a finished run's worktree and branch and archive it (reports and events are kept). target is a \
             run name or 'all'. force discards unmerged branches and uncommitted changes; running workers are \
             aborted only when named explicitly — 'all' always skips them. 'all' sweeps the whole fleet and says so \
             when it includes runs owned by other orchestrator sessions.",
            properties([
                ("target", string_prop("Run name or id, or 'all'")),
                (
                    "force",
                    bool_prop("Discard unmerged work (never aborts a run 'all' did not name)"),
                ),
            ]),
            &["target"],
            None,
        ),
    ]
}

/// One tool definition. rmcp's model types are `#[non_exhaustive]`, so this
/// goes through `Default` plus field assignment.
fn make_tool(
    name: &'static str,
    title: &str,
    description: &str,
    properties: JsonObject,
    required: &[&str],
    output: Option<JsonObject>,
) -> Tool {
    let mut tool = Tool::default();
    tool.name = Cow::Borrowed(name);
    tool.title = Some(title.to_owned());
    tool.description = Some(Cow::Owned(description.to_owned()));
    tool.input_schema = Arc::new(input_schema(properties, required));
    tool.output_schema = output.map(|schema| Arc::new(with_object_schema(schema)));
    tool
}

/// `{"type": "object", "properties": …, "required": …}`.
fn input_schema(properties: JsonObject, required: &[&str]) -> JsonObject {
    let mut schema = Map::new();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert(
            "required".into(),
            Value::Array(
                required
                    .iter()
                    .map(|r| Value::String((*r).to_owned()))
                    .collect(),
            ),
        );
    }
    schema.insert("additionalProperties".into(), Value::Bool(false));
    schema
}

/// An output schema wrapping the given properties: stricter than input —
/// `additionalProperties: false` so the caller can trust the shape.
fn with_object_schema(properties: JsonObject) -> JsonObject {
    let mut schema = Map::new();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("properties".into(), Value::Object(properties));
    schema.insert("additionalProperties".into(), Value::Bool(false));
    schema
}

/// Named properties from `(name, schema)` pairs.
fn properties<const N: usize>(props: [(&str, Value); N]) -> JsonObject {
    props
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect()
}

/// Named output-schema properties from `(name, schema)` pairs.
fn object_schema<const N: usize>(props: [(&str, Value); N]) -> JsonObject {
    properties(props)
}

/// `{"type": "string", "description": …}`.
fn string_prop(description: &str) -> Value {
    json_schema(json!({"type": "string", "description": description}))
}

/// `{"type": "boolean", "description": …}`.
fn bool_prop(description: &str) -> Value {
    json_schema(json!({"type": "boolean", "description": description}))
}

/// `{"type": "integer", "minimum": min, ["maximum": max], "description": …}`.
fn integer_prop(description: &str, min: u64, max: Option<u64>) -> Value {
    let mut schema = json!({"type": "integer", "minimum": min, "description": description});
    if let (Some(max), Some(object)) = (max, schema.as_object_mut()) {
        object.insert(String::from("maximum"), Value::from(max));
    }
    json_schema(schema)
}

fn string_schema() -> Value {
    json_schema(json!({"type": "string"}))
}

/// `string | null`, for spawn's optional worktree/branch.
fn nullable_string_schema() -> Value {
    json_schema(json!({"type": ["string", "null"]}))
}

/// The `tail` argument as a line count: validated 1.., clamped to the host's
/// `usize` so a 32-bit build cannot truncate a huge value.
fn tail_arg(args: &JsonObject, key: &str) -> Result<Option<usize>, McpError> {
    Ok(opt_u64(args, key, 1, None)?.map(|n| usize::try_from(n).unwrap_or(usize::MAX)))
}

/// Turn a `json!` value into a schema map; the shapes above are literals, so
/// this cannot actually fail.
const fn json_schema(schema: Value) -> Value {
    schema
}

// ---------------------------------------------------------------------------
// Argument parsing — the port of the zod schemas. Problems are protocol
// errors (`INVALID_PARAMS`), like the TypeScript SDK's validation failures.
// ---------------------------------------------------------------------------

fn invalid_param(message: String) -> McpError {
    McpError::invalid_params(message, None)
}

const fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn mistyped(key: &str, value: &Value, expected: &str) -> McpError {
    invalid_param(format!(
        "invalid argument {key}: expected {expected}, got {}",
        type_of(value)
    ))
}

/// A required non-empty string argument (`z.string().min(1)`).
fn req_str(args: &JsonObject, key: &str) -> Result<String, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Err(invalid_param(format!("missing required argument: {key}"))),
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(invalid_param(format!(
            "invalid argument {key}: must be a non-empty string"
        ))),
        Some(other) => Err(mistyped(key, other, "a string")),
    }
}

/// An optional string argument.
fn opt_str(args: &JsonObject, key: &str) -> Result<Option<String>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(mistyped(key, other, "a string")),
    }
}

/// An optional boolean argument.
fn opt_bool(args: &JsonObject, key: &str) -> Result<Option<bool>, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(mistyped(key, other, "a boolean")),
    }
}

/// An optional integer argument within `[min, max]` (max `None` = unbounded).
fn opt_u64(
    args: &JsonObject,
    key: &str,
    min: u64,
    max: Option<u64>,
) -> Result<Option<u64>, McpError> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let number = match value {
        Value::Null => return Ok(None),
        Value::Number(n) => n.as_u64().ok_or_else(|| {
            invalid_param(format!(
                "invalid argument {key}: expected an integer >= {min}"
            ))
        }),
        other => return Err(mistyped(key, other, "an integer")),
    }?;
    if number < min {
        return Err(invalid_param(format!(
            "invalid argument {key}: must be >= {min}"
        )));
    }
    if max.is_some_and(|max| number > max) {
        return Err(invalid_param(format!(
            "invalid argument {key}: must be <= {}",
            max.unwrap_or_default()
        )));
    }
    Ok(Some(number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The single text block of a tool result.
    fn text_of(result: &CallToolResult) -> String {
        match result.content.first() {
            Some(ContentBlock::Text(text)) => text.text.clone(),
            other => panic!("expected one text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_party_is_the_fleets_acting_session_or_the_default() {
        let cwd = std::env::temp_dir().join(format!(
            "parl-mcp-party-cwd-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        let fleet = std::env::temp_dir().join(format!(
            "parl-mcp-party-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        for dir in [&cwd, &fleet] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // No fleet.json: the default session, spelled "orchestrator".
        let server = FleetServer::with_parl_dir(
            Some(cwd.clone()),
            Some(fleet.to_string_lossy().into_owned()),
        );
        assert_eq!(
            server.tool_party().await,
            Party::Orchestrator(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION)
        );
        // With fleet.json, the acting session is its last-used row.
        let mut store = crate::orch::session::FleetSessions::new();
        store.upsert(crate::orch::session::OrchestratorSession::new("/repo"));
        let session = store.last_used().unwrap().uuid;
        crate::orch::session::save(&fleet, &mut store).unwrap();
        let server =
            FleetServer::with_parl_dir(Some(cwd), Some(fleet.to_string_lossy().into_owned()));
        assert_eq!(server.tool_party().await, Party::Orchestrator(session));
    }

    #[test]
    fn render_result_puts_lines_exit_code_and_structured_content_in_order() {
        let good = CommandResult {
            code: ExitCode::Ok,
            out: vec!["a".into()],
            err: vec!["warn".into()],
            data: json!({"x": 1}),
        };
        let rendered = render_result(&good, Some(json!({"x": 1})));
        assert_eq!(text_of(&rendered), "a\nwarn\nexit: 0");
        assert_eq!(rendered.is_error, Some(false));
        assert_eq!(rendered.structured_content, Some(json!({"x": 1})));

        let bad = CommandResult {
            code: ExitCode::MergeConflict,
            out: Vec::new(),
            err: vec!["boom".into()],
            data: Value::Null,
        };
        let rendered = render_result(&bad, None);
        assert_eq!(text_of(&rendered), "boom\nexit: 5");
        assert_eq!(rendered.is_error, Some(true));
        assert_eq!(rendered.structured_content, None);
    }

    #[test]
    fn render_error_carries_the_message_and_exit_1() {
        let err = anyhow::anyhow!("No run found matching \"nope\"");
        let rendered = render_error(&err);
        assert_eq!(
            text_of(&rendered),
            "No run found matching \"nope\"\nexit: 1"
        );
        assert_eq!(rendered.is_error, Some(true));
    }

    #[test]
    fn the_thirteen_tools_carry_their_schemas() {
        let tools = fleet_tools();
        assert_eq!(
            tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(),
            FLEET_TOOL_NAMES.to_vec()
        );
        let spawn = &tools[0];
        assert_eq!(spawn.title.as_deref(), Some("Spawn a pi worker"));
        let required = spawn.input_schema.get("required").unwrap();
        assert_eq!(required, &json!(["name", "brief"]));
        assert!(
            spawn.output_schema.is_some(),
            "spawn declares structured output"
        );
        // The spawn output names the fields the ops data actually emits.
        let output = spawn.output_schema.as_ref().unwrap();
        let properties = output.get("properties").unwrap();
        for key in ["runId", "runDir", "fleetDir", "worktree", "branch"] {
            assert!(properties.get(key).is_some(), "{key} missing: {properties}");
        }
        // Only spawn and status declare structured output.
        assert!(
            tools[1].output_schema.is_some(),
            "status declares structured output"
        );
        for tool in tools.iter().skip(2) {
            assert!(tool.output_schema.is_none(), "{}", tool.name);
        }
        // Every description is present: the orchestrating model reads them.
        for tool in &tools {
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "{} has no description",
                tool.name
            );
        }
        // The dialog update landed where the orchestrator needs it.
        let answer = tools.iter().find(|t| t.name == "fleet_answer").unwrap();
        assert!(
            answer
                .description
                .as_ref()
                .is_some_and(|d| d.contains("dialog"))
        );
        let status = tools.iter().find(|t| t.name == "fleet_status").unwrap();
        assert!(
            status
                .description
                .as_ref()
                .is_some_and(|d| d.contains("dialog"))
        );
        // The report names its new home.
        let report = tools.iter().find(|t| t.name == "fleet_report").unwrap();
        assert!(
            report
                .description
                .as_ref()
                .is_some_and(|d| d.contains("runs/<runId>/report.md"))
        );
    }

    #[test]
    fn arguments_are_validated_like_the_zod_schemas() {
        let mut args = Map::new();
        assert_eq!(
            req_str(&args, "name").unwrap_err().message,
            "missing required argument: name"
        );
        args.insert("name".into(), json!(""));
        assert!(
            req_str(&args, "name")
                .unwrap_err()
                .message
                .contains("must be a non-empty string")
        );
        args.insert("name".into(), json!(3));
        assert!(
            req_str(&args, "name")
                .unwrap_err()
                .message
                .contains("expected a string")
        );
        args.insert("name".into(), json!("hello"));

        args.insert("tail".into(), json!(0));
        assert!(
            opt_u64(&args, "tail", 1, None)
                .unwrap_err()
                .message
                .contains(">= 1")
        );
        args.insert("tail".into(), json!(1.5));
        assert!(opt_u64(&args, "tail", 1, None).is_err());
        args.insert("tail".into(), json!(4));
        assert_eq!(opt_u64(&args, "tail", 1, None).unwrap(), Some(4));

        args.insert("timeoutSec".into(), json!(601));
        assert!(
            opt_u64(&args, "timeoutSec", 1, Some(600))
                .unwrap_err()
                .message
                .contains("<= 600")
        );
        args.insert("timeoutSec".into(), json!(30));
        assert_eq!(
            opt_u64(&args, "timeoutSec", 1, Some(600)).unwrap(),
            Some(30)
        );

        args.insert("worktree".into(), json!("yes"));
        assert!(
            opt_bool(&args, "worktree")
                .unwrap_err()
                .message
                .contains("a boolean")
        );
        args.insert("worktree".into(), json!(true));
        assert_eq!(opt_bool(&args, "worktree").unwrap(), Some(true));
        assert_eq!(opt_bool(&args, "absent").unwrap(), None);
        // Explicit nulls read as absent, like clients that send them.
        args.insert("absent".into(), Value::Null);
        assert_eq!(opt_str(&args, "absent").unwrap(), None);
    }
}
