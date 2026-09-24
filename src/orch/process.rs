//! The orchestrator process: a `claude -p` child driven over stream-json.
//!
//! Ported from the TypeScript `src/orchestrator/process.ts` onto tokio: the
//! Node `EventEmitter` becomes a `tokio::sync::mpsc` channel of
//! [`ProcEvent`]s, and the synchronous handler state becomes a mutex-guarded
//! [`OrchState`]. This owns the process and the wire protocol; it knows
//! nothing about the TUI, the transcript files, or the monitor.
//!
//! The TypeScript comments that capture hard-won protocol facts are carried
//! over — they are load-bearing.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nix::sys::signal::Signal;
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, watch};

use crate::orch::args::{ClaudeArgsOptions, build_claude_args, claude_command_from_spec};
use crate::orch::protocol::{
    AgentCommand, ControlResponseMessage, ResultMessage, SystemInitMessage, allow_response,
    apply_flag_settings_request, ask_user_question_response, deny_response, initialize_request,
    interrupt_request, is_assistant, is_can_use_tool, is_control_cancel_request,
    is_control_response, is_result, is_stream_event, is_system_init, is_thinking_event, is_user,
    new_request_id, parse_claude_line, serialize, set_model_request, set_permission_mode_request,
    text_delta_of, thinking_delta_of, tool_uses_of, try_can_use_tool, try_control_response,
    try_result, try_system_init, user_message,
};
use crate::orch::records::{Activity, ActivityKind, sorted_pending};
use crate::util::now_iso;

/// Control requests are correlated by request id with this timeout; on timeout
/// or child death the caller gets `None`.
const CONTROL_TIMEOUT_MS: u64 = 5_000;

/// An outbound control request resolved: the CLI's receipt, or its verbatim
/// error text (worth surfacing — `set_model` validates the name itself).
#[derive(Debug, Clone, PartialEq)]
pub enum ControlOutcome {
    Success(Value),
    Error(String),
}

impl ControlOutcome {
    /// The success payload, or none for an error.
    #[must_use]
    pub const fn success(&self) -> Option<&Value> {
        match self {
            Self::Success(value) => Some(value),
            Self::Error(_) => None,
        }
    }

    /// The CLI's error text, or none on success.
    #[must_use]
    pub fn error_text(&self) -> Option<&str> {
        match self {
            Self::Success(_) => None,
            Self::Error(text) => Some(text),
        }
    }
}

/// How the child ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

/// A permission prompt (or AskUserQuestion) waiting for a human, as seen by
/// the process.
pub type PermissionRequest = crate::orch::protocol::PermissionRequest;

/// Everything the process emits, in order. The single consumer (the monitor)
/// forwards these to the transcript files; losing none of them is why this is
/// an unbounded channel.
#[derive(Debug, Clone)]
pub enum ProcEvent {
    /// The `system/init` handshake.
    Init(SystemInitMessage),
    Assistant(Value),
    User(Value),
    TextDelta(String),
    /// A `thinking_delta`, coalesced by the monitor like a text delta so
    /// reasoning reaches the console as it is written, not at turn end.
    ThinkingDelta(String),
    StreamEvent(Value),
    Result(ResultMessage),
    PermissionRequest(PermissionRequest),
    ControlResponse(ControlResponseMessage),
    /// Slash commands and skills claude offers, from the initialize response.
    Commands(Vec<AgentCommand>),
    /// Every parsed message, in order.
    Message(Value),
    /// A line we wrote to the child's stdin.
    Sent(Value),
    Stderr(String),
    Spawned(u32),
    Exit(ExitInfo),
    /// A child we could not even spawn (bad binary, missing cwd).
    Error(String),
}

enum WriteMsg {
    Line(String),
    Close,
}

/// What the process needs beyond the argv.
#[derive(Debug, Clone)]
pub struct OrchestratorOptions {
    /// Argv options (prompt file, mcp config, model, …).
    pub args: ClaudeArgsOptions,
    /// The repository the orchestrator works in (claude's cwd).
    pub cwd: PathBuf,
    /// Raw protocol log (both directions), e.g. `orchestrator/claude.log`.
    pub log_path: Option<PathBuf>,
    /// The exact environment for the child, or none to inherit ours.
    pub env: Option<HashMap<String, String>>,
    /// Escalation delays for [`OrchestratorProcess::stop`].
    pub stop_grace_ms: u64,
}

impl OrchestratorOptions {
    /// Options for a child in `cwd` with the given prompt and MCP documents.
    #[must_use]
    pub fn new(cwd: PathBuf, prompt_file: String, mcp_config_json: String) -> Self {
        Self {
            args: ClaudeArgsOptions {
                prompt_file,
                mcp_config_json,
                ..ClaudeArgsOptions::default()
            },
            cwd,
            log_path: None,
            env: None,
            stop_grace_ms: 3_000,
        }
    }
}

/// Mutable derived state, guarded by one mutex. Never hold the guard across
/// an await.
#[derive(Default)]
struct OrchState {
    session_id: Option<String>,
    model: Option<String>,
    claude_version: Option<String>,
    capabilities: Vec<String>,
    slash_commands: Vec<AgentCommand>,
    cost_usd: f64,
    num_turns: u32,
    turn_active: bool,
    activity: Option<Activity>,
    init_received: bool,
    pending_requests: HashMap<String, PermissionRequest>,
    stderr_tail: VecDeque<String>,
}

/// The `claude -p` child: spawn it, drive it, watch it die.
///
/// Methods that spawn tasks take `&Arc<Self>` — an explicit parameter, since
/// `self: &Arc<Self>` is not a valid receiver — because the tasks outlive the
/// call.
pub struct OrchestratorProcess {
    options: OrchestratorOptions,
    pid: Mutex<Option<u32>>,
    state: Mutex<OrchState>,
    control_waiters: Mutex<HashMap<String, oneshot::Sender<ControlOutcome>>>,
    write_tx: mpsc::UnboundedSender<WriteMsg>,
    write_rx: Mutex<Option<mpsc::UnboundedReceiver<WriteMsg>>>,
    event_tx: mpsc::UnboundedSender<ProcEvent>,
    exit_tx: watch::Sender<Option<ExitInfo>>,
    exit_rx: watch::Receiver<Option<ExitInfo>>,
    result_tx: watch::Sender<u64>,
    result_rx: watch::Receiver<u64>,
    log: Mutex<Option<std::fs::File>>,
    started: AtomicBool,
}

/// Lock a mutex even if a previous holder panicked; the state is still
/// consistent enough to keep going.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl OrchestratorProcess {
    /// Build the process and take the event stream. Call [`Self::start`] on
    /// the returned handle to spawn the child.
    #[must_use]
    pub fn new(options: OrchestratorOptions) -> (Arc<Self>, mpsc::UnboundedReceiver<ProcEvent>) {
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = watch::channel(None);
        let (result_tx, result_rx) = watch::channel(0);
        let session_id = options.args.resume_session_id.clone();
        let process = Arc::new(Self {
            options,
            pid: Mutex::new(None),
            state: Mutex::new(OrchState {
                session_id,
                ..OrchState::default()
            }),
            control_waiters: Mutex::new(HashMap::new()),
            write_tx,
            write_rx: Mutex::new(Some(write_rx)),
            event_tx,
            exit_tx,
            exit_rx,
            result_tx,
            result_rx,
            log: Mutex::new(None),
            started: AtomicBool::new(false),
        });
        (process, event_rx)
    }

    // -- introspection -----------------------------------------------------

    /// The options this process was built with.
    pub const fn options(&self) -> &OrchestratorOptions {
        &self.options
    }

    /// The child's pid, once spawned.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        *lock(&self.pid)
    }

    /// True while the child is spawned and has not exited.
    #[must_use]
    pub fn running(&self) -> bool {
        self.started.load(Ordering::Acquire) && self.exited().is_none()
    }

    /// The argv the child is started with (without the executable).
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        build_claude_args(&self.options.args)
    }

    /// The claude session id: the one we resume, or the one the child gave us.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        lock(&self.state).session_id.clone()
    }

    /// The model claude reports (from init or result).
    #[must_use]
    pub fn model(&self) -> Option<String> {
        lock(&self.state).model.clone()
    }

    /// The claude CLI version, from init.
    #[must_use]
    pub fn claude_version(&self) -> Option<String> {
        lock(&self.state).claude_version.clone()
    }

    /// Capabilities claude announced at init.
    #[must_use]
    pub fn capabilities(&self) -> Vec<String> {
        lock(&self.state).capabilities.clone()
    }

    /// Slash commands and skills claude offers, learned from the initialize
    /// response.
    #[must_use]
    pub fn slash_commands(&self) -> Vec<AgentCommand> {
        lock(&self.state).slash_commands.clone()
    }

    /// Running total cost, from the latest result.
    #[must_use]
    pub fn cost_usd(&self) -> f64 {
        lock(&self.state).cost_usd
    }

    /// Turn count, from the latest result.
    #[must_use]
    pub fn num_turns(&self) -> u32 {
        lock(&self.state).num_turns
    }

    /// True from the moment we send a user message until the next `result`.
    #[must_use]
    pub fn turn_active(&self) -> bool {
        lock(&self.state).turn_active
    }

    /// What the model is doing right now, or none between turns.
    #[must_use]
    pub fn activity(&self) -> Option<Activity> {
        lock(&self.state).activity.clone()
    }

    /// Whether the `system/init` handshake has been seen.
    #[must_use]
    pub fn init_received(&self) -> bool {
        lock(&self.state).init_received
    }

    /// How the child ended, once it has.
    #[must_use]
    pub fn exited(&self) -> Option<ExitInfo> {
        self.exit_rx.borrow().clone()
    }

    /// Permission prompts (and AskUserQuestions) awaiting an answer, oldest
    /// first.
    #[must_use]
    pub fn pending_requests(&self) -> Vec<PermissionRequest> {
        sorted_pending(&lock(&self.state).pending_requests)
    }

    /// Last stderr output, for error reporting.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        lock(&self.state)
            .stderr_tail
            .iter()
            .map(String::as_str)
            .collect()
    }

    // -- lifecycle ---------------------------------------------------------

    /// Spawn the child and start the reader/writer/watcher tasks.
    ///
    /// # Panics
    ///
    /// Panics when called twice — starting twice is a programming error, as
    /// in the TypeScript (`throw new Error("orchestrator already started")`).
    pub fn start(self: &Arc<Self>) {
        assert!(
            !self.started.swap(true, Ordering::AcqRel),
            "orchestrator already started"
        );
        if let Some(path) = &self.options.log_path {
            *lock(&self.log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok();
        }
        let env_spec = self
            .options
            .env
            .as_ref()
            .and_then(|env| {
                env.get(crate::paths::env_var("CLAUDE_BIN").as_str())
                    .cloned()
            })
            .or_else(|| std::env::var(crate::paths::env_var("CLAUDE_BIN")).ok());
        let (bin, prefix) = claude_command_from_spec(env_spec.as_deref());
        let argv = self.args();
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&prefix)
            .args(&argv)
            .current_dir(&self.options.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(env) = &self.options.env {
            cmd.env_clear();
            cmd.envs(env.iter());
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let message = format!("could not spawn {bin}: {err}");
                self.log_text(&format!("[error] {message}\n"));
                self.emit(ProcEvent::Error(message));
                // Mark the child gone so stop paths short-circuit instead of
                // waiting on a child that never was.
                let _ = self.exit_tx.send(Some(ExitInfo {
                    code: None,
                    signal: None,
                }));
                return;
            }
        };
        let pid = child.id();
        *lock(&self.pid) = pid;
        self.log_text(&format!(
            "[{}] spawn pid={} {bin} {} {}\n",
            now_iso(),
            pid.map_or_else(|| "?".to_string(), |p| p.to_string()),
            prefix.join(" "),
            argv.join(" ")
        ));
        if let Some(pid) = pid {
            self.emit(ProcEvent::Spawned(pid));
        }

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let writer_rx = lock(&self.write_rx).take();
        if let (Some(stdin), Some(rx)) = (stdin, writer_rx) {
            Self::spawn_writer(self, stdin, rx);
        }
        if let Some(stderr) = stderr {
            Self::spawn_stderr_task(self, stderr);
        }
        if let Some(stdout) = stdout {
            Self::spawn_reader(self, stdout);
        }
        Self::spawn_waiter(self, child);
        // The handshake is also how we learn which commands and skills this
        // claude offers, so it is always sent; the spike showed permission
        // prompts do not depend on it.
        Self::request_commands(self);
    }

    fn spawn_writer(
        process: &Arc<Self>,
        mut stdin: ChildStdin,
        mut rx: mpsc::UnboundedReceiver<WriteMsg>,
    ) {
        let owner = process.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    WriteMsg::Line(line) => {
                        owner.log_text(&format!("> {line}"));
                        // A write that races the child's death fails here
                        // instead of taking the process down; the close handler
                        // (the waiter task) is what actually reports the child
                        // going away.
                        if let Err(err) = stdin.write_all(line.as_bytes()).await {
                            owner.log_text(&format!("[stdin error] {err}\n"));
                        }
                    }
                    WriteMsg::Close => break,
                }
            }
            // Dropping stdin half-closes: the child sees EOF and can exit
            // cleanly on its own.
            drop(stdin);
        });
    }

    fn spawn_stderr_task(process: &Arc<Self>, stderr: tokio::process::ChildStderr) {
        let owner = process.clone();
        let mut reader = tokio::io::BufReader::new(stderr);
        tokio::spawn(async move {
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = String::from_utf8_lossy(&line).into_owned();
                {
                    let mut state = lock(&owner.state);
                    state.stderr_tail.push_back(text.clone());
                    while state.stderr_tail.len() > 40 {
                        state.stderr_tail.pop_front();
                    }
                }
                owner.log_text(&format!("[stderr] {text}"));
                owner.emit(ProcEvent::Stderr(text));
            }
        });
    }

    fn spawn_reader(process: &Arc<Self>, stdout: ChildStdout) {
        let owner = process.clone();
        let mut reader = tokio::io::BufReader::new(stdout);
        tokio::spawn(async move {
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let mut text = String::from_utf8_lossy(&line).into_owned();
                while text.ends_with('\n') || text.ends_with('\r') {
                    text.pop();
                }
                if text.trim().is_empty() {
                    continue;
                }
                owner.log_text(&format!("< {text}\n"));
                if let Some(msg) = parse_claude_line(&text) {
                    owner.handle_message(msg);
                }
            }
        });
    }

    fn spawn_waiter(process: &Arc<Self>, mut child: Child) {
        let owner = process.clone();
        tokio::spawn(async move {
            let info = match child.wait().await {
                Ok(status) => ExitInfo {
                    code: status.code(),
                    #[cfg(unix)]
                    signal: status.signal().and_then(signal_name),
                    #[cfg(not(unix))]
                    signal: None,
                },
                Err(err) => {
                    owner.log_text(&format!("[error] waiting on the child: {err}\n"));
                    ExitInfo {
                        code: None,
                        signal: None,
                    }
                }
            };
            {
                let mut state = lock(&owner.state);
                state.turn_active = false;
                state.activity = None;
            }
            // Resolve every outstanding control request with none — the
            // senders dropping is what wakes the waiters.
            lock(&owner.control_waiters).clear();
            owner.log_text(&format!(
                "[{}] exit code={} signal={}\n",
                now_iso(),
                info.code.map_or_else(|| "?".to_string(), |c| c.to_string()),
                info.signal.clone().unwrap_or_else(|| "none".into())
            ));
            let _ = owner.exit_tx.send(Some(info.clone()));
            owner.emit(ProcEvent::Exit(info));
        });
    }

    /// Ask claude what it offers. Sent once at startup and again whenever a
    /// console asks for a refresh — a session's command list grows when a
    /// skill is installed, so one answer at handshake time goes stale.
    pub fn request_commands(process: &Arc<Self>) {
        let owner = process.clone();
        tokio::spawn(async move {
            let id = new_request_id();
            let outcome = owner.control(&id, initialize_request(&id, json!({}))).await;
            let Some(ControlOutcome::Success(response)) = outcome else {
                return;
            };
            let Some(commands) = response.get("commands").and_then(Value::as_array) else {
                return;
            };
            let commands: Vec<AgentCommand> = commands
                .iter()
                .filter(|c| c.get("name").and_then(Value::as_str).is_some())
                .filter_map(|c| serde_json::from_value(c.clone()).ok())
                .collect();
            lock(&owner.state).slash_commands = commands.clone();
            owner.emit(ProcEvent::Commands(commands));
        });
    }

    // -- io ----------------------------------------------------------------

    fn log_text(&self, text: &str) {
        if let Some(file) = lock(&self.log).as_mut() {
            use std::io::Write as _;
            let _ = file.write_all(text.as_bytes());
        }
    }

    fn emit(&self, event: ProcEvent) {
        // A dropped receiver just means nobody is listening.
        let _ = self.event_tx.send(event);
    }

    /// Queue one message for the child. False when there is no child left to
    /// write to. The actual write happens on the writer task, so a failure
    /// never takes this process down.
    fn write(&self, msg: &Value) -> bool {
        if !self.running() {
            return false;
        }
        let line = serialize(msg);
        self.emit(ProcEvent::Sent(msg.clone()));
        self.write_tx.send(WriteMsg::Line(line)).is_ok()
    }

    // -- conversation ------------------------------------------------------

    /// A user turn, or an async message injected mid-turn (claude folds it
    /// into the running turn).
    pub fn send(&self, text: &str) -> bool {
        let wrote = self.write(&user_message(text));
        if wrote {
            let mut state = lock(&self.state);
            state.turn_active = true;
            state.activity = Some(Activity::starting(ActivityKind::Thinking, None));
        }
        wrote
    }

    /// Allow a pending tool call, optionally adopting the rules claude
    /// suggested. False when the request is unknown or already answered.
    pub fn allow(&self, request_id: &str, updated_permissions: Option<&[Value]>) -> bool {
        let input = {
            let mut state = lock(&self.state);
            match state.pending_requests.remove(request_id) {
                Some(pending) => pending.request.input,
                None => return false,
            }
        };
        self.write(&allow_response(request_id, input, updated_permissions))
    }

    /// Deny a pending tool call with a reason shown to the model.
    pub fn deny(&self, request_id: &str, message: &str) -> bool {
        lock(&self.state).pending_requests.remove(request_id);
        self.write(&deny_response(request_id, message))
    }

    /// Answer an AskUserQuestion request (answers keyed by question text).
    pub fn answer_question(&self, request_id: &str, answers: Value) -> bool {
        let input = {
            let mut state = lock(&self.state);
            match state.pending_requests.remove(request_id) {
                Some(pending) => pending.request.input,
                None => return false,
            }
        };
        self.write(&ask_user_question_response(request_id, input, answers))
    }

    // -- control requests --------------------------------------------------

    /// Send a control request and wait for its correlated response: the CLI's
    /// receipt, its verbatim error text, or `None` on the 5 s timeout or child
    /// death. The waiter is registered *before* the write so a fast response
    /// cannot race its own registration — single-threaded JavaScript never had
    /// this race; tokio does.
    pub async fn control(&self, request_id: &str, msg: Value) -> Option<ControlOutcome> {
        let (tx, rx) = oneshot::channel();
        lock(&self.control_waiters).insert(request_id.to_string(), tx);
        if !self.write(&msg) {
            lock(&self.control_waiters).remove(request_id);
            return None;
        }
        match tokio::time::timeout(Duration::from_millis(CONTROL_TIMEOUT_MS), rx).await {
            Ok(Ok(outcome)) => Some(outcome),
            _ => None,
        }
    }

    /// Stop the running turn; resolves with the CLI's receipt (or none on
    /// timeout). With `cancel_queued`, also drops messages queued behind it.
    pub async fn interrupt(&self, cancel_queued: bool) -> Option<ControlOutcome> {
        let id = new_request_id();
        self.control(&id, interrupt_request(&id, cancel_queued))
            .await
    }

    /// Change how prompts are handled mid-session.
    pub async fn set_permission_mode(&self, mode: &str) -> Option<ControlOutcome> {
        let id = new_request_id();
        self.control(&id, set_permission_mode_request(&id, mode))
            .await
    }

    /// Change the orchestrator's model live: claude switches the running
    /// session with no child restart and no conversation turn, and validates
    /// the name itself — an unknown model comes back as the CLI's verbatim
    /// error text. Prefer this over [`Self::apply_flag_settings`], which
    /// succeeds without validating.
    pub async fn set_model(&self, model: &str) -> Option<ControlOutcome> {
        let id = new_request_id();
        self.control(&id, set_model_request(&id, model)).await
    }

    /// Change session settings (effort, thinking) without saying anything to
    /// the model.
    pub async fn apply_flag_settings(&self, settings: Value) -> Option<ControlOutcome> {
        let id = new_request_id();
        self.control(&id, apply_flag_settings_request(&id, &settings))
            .await
    }

    /// The SDK's session handshake; its response is how we learn the slash
    /// commands and skills this claude offers.
    pub async fn initialize(&self) -> Option<ControlOutcome> {
        let id = new_request_id();
        self.control(&id, initialize_request(&id, json!({}))).await
    }

    /// Wait until the next `result` (a turn ended), or `dur` elapses.
    async fn wait_turn_end(&self, dur: Duration) {
        let mut rx = self.result_rx.clone();
        let baseline = *rx.borrow();
        let _ = tokio::time::timeout(dur, async {
            while rx.changed().await.is_ok() {
                if *rx.borrow_and_update() != baseline {
                    break;
                }
            }
        })
        .await;
    }

    // -- shutdown ----------------------------------------------------------

    /// End the running turn, then stdin, then escalate to SIGTERM and SIGKILL.
    ///
    /// The interrupt matters: a `claude -p` killed mid-turn leaves that turn
    /// unfinished, and resuming the session *continues* it — which looks like
    /// the session restarting itself the next time the console opens.
    pub async fn stop(self: &Arc<Self>) -> ExitInfo {
        if self.exited().is_none() && self.turn_active() {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.interrupt(true)).await;
            if self.turn_active() {
                self.wait_turn_end(Duration::from_millis(1_500)).await;
            }
        }
        self.stop_now().await
    }

    /// Close the child down without ending the turn first: stdin end, then
    /// SIGTERM after `stop_grace_ms`, then SIGKILL after twice that.
    pub async fn stop_now(self: &Arc<Self>) -> ExitInfo {
        if let Some(info) = self.exited() {
            return info;
        }
        if !self.started.load(Ordering::Acquire) {
            return ExitInfo {
                code: None,
                signal: None,
            };
        }
        let grace = self.options.stop_grace_ms;
        let _ = self.write_tx.send(WriteMsg::Close);
        let escalator = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(grace)).await;
            if escalator.exited().is_none() {
                escalator.signal_process(Signal::SIGTERM);
            }
            tokio::time::sleep(Duration::from_millis(grace)).await;
            if escalator.exited().is_none() {
                escalator.signal_process(Signal::SIGKILL);
            }
        });
        let mut rx = self.exit_rx.clone();
        while rx.borrow().is_none() {
            if rx.changed().await.is_err() {
                break;
            }
        }
        self.exited().unwrap_or(ExitInfo {
            code: None,
            signal: None,
        })
    }

    fn signal_process(&self, sig: Signal) {
        let Some(pid) = self.pid() else { return };
        // ESRCH: it died between the check and the signal — fine. EPERM: not
        // ours to signal — nothing to do either. Unix pids fit in i32
        // (pid_max ≪ 2^31); the fallback only ever fails harmlessly.
        let _ = nix::sys::signal::kill(Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX)), sig);
    }

    // -- stream handling ---------------------------------------------------

    fn handle_message(&self, msg: Value) {
        self.emit(ProcEvent::Message(msg.clone()));
        if is_system_init(&msg) {
            if let Some(init) = try_system_init(&msg) {
                let mut state = lock(&self.state);
                state.init_received = true;
                state.session_id = Some(init.session_id.clone());
                if init.model.is_some() {
                    state.model = init.model.clone();
                }
                if init.claude_code_version.is_some() {
                    state.claude_version = init.claude_code_version.clone();
                }
                state.capabilities = init.capabilities.clone();
                drop(state);
                self.emit(ProcEvent::Init(init));
            }
            return;
        }
        if is_can_use_tool(&msg) {
            if let Some(request) = try_can_use_tool(&msg) {
                let request_id = msg
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let pending = PermissionRequest {
                    request_id: request_id.clone(),
                    request,
                    received_at: now_iso(),
                };
                lock(&self.state)
                    .pending_requests
                    .insert(request_id, pending.clone());
                self.emit(ProcEvent::PermissionRequest(pending));
            }
            return;
        }
        if is_control_cancel_request(&msg) {
            if let Some(id) = msg.get("request_id").and_then(Value::as_str) {
                lock(&self.state).pending_requests.remove(id);
            }
            return;
        }
        if is_control_response(&msg) {
            if let Some(response) = try_control_response(&msg) {
                let waiter = lock(&self.control_waiters).remove(&response.response.request_id);
                if let Some(tx) = waiter {
                    let outcome = if response.response.subtype == "success" {
                        ControlOutcome::Success(
                            response
                                .response
                                .response
                                .clone()
                                .unwrap_or_else(|| json!({})),
                        )
                    } else {
                        ControlOutcome::Error(response.response.error.clone().unwrap_or_default())
                    };
                    let _ = tx.send(outcome);
                }
                self.emit(ProcEvent::ControlResponse(response));
            }
            return;
        }
        if is_stream_event(&msg) {
            lock(&self.state).turn_active = true;
            if let Some(delta) = text_delta_of(&msg) {
                self.bump_activity(ActivityKind::Responding, None);
                self.emit(ProcEvent::TextDelta(delta));
            } else if is_thinking_event(&msg) {
                self.bump_activity(ActivityKind::Thinking, None);
                if let Some(delta) = thinking_delta_of(&msg) {
                    self.emit(ProcEvent::ThinkingDelta(delta));
                }
            }
            self.emit(ProcEvent::StreamEvent(msg));
            return;
        }
        if is_assistant(&msg) {
            lock(&self.state).turn_active = true;
            let uses = tool_uses_of(&msg);
            if let Some(last) = uses.last() {
                self.bump_activity(ActivityKind::Tool, Some(last.name.clone()));
            }
            self.emit(ProcEvent::Assistant(msg));
            return;
        }
        if is_user(&msg) {
            self.emit(ProcEvent::User(msg));
            return;
        }
        if is_result(&msg) {
            let view = try_result(&msg);
            {
                let mut state = lock(&self.state);
                state.turn_active = false;
                state.activity = None;
                if let Some(result) = &view {
                    if let Some(cost) = result.total_cost_usd {
                        state.cost_usd = cost;
                    }
                    if let Some(turns) = result.num_turns {
                        state.num_turns = turns;
                    }
                    if result.session_id.is_some() {
                        state.session_id = result.session_id.clone();
                    }
                }
            }
            self.result_tx.send_modify(|count| *count += 1);
            if let Some(result) = view {
                self.emit(ProcEvent::Result(result));
            }
        }
    }

    /// Move to `kind` unless the model is already there — the elapsed counter
    /// should not restart on every delta. A labelled (tool) activity always
    /// refreshes, since the tool changed.
    fn bump_activity(&self, kind: ActivityKind, label: Option<String>) {
        let mut state = lock(&self.state);
        if label.is_none() && state.activity.as_ref().map(|a| a.kind) == Some(kind) {
            return;
        }
        state.activity = Some(Activity::starting(kind, label));
    }
}

#[cfg(unix)]
fn signal_name(signal: i32) -> Option<String> {
    Signal::try_from(signal).ok().map(|sig| sig.to_string())
}

#[cfg(not(unix))]
fn signal_name(_signal: i32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make() -> (Arc<OrchestratorProcess>, mpsc::UnboundedReceiver<ProcEvent>) {
        OrchestratorProcess::new(OrchestratorOptions::new(
            PathBuf::from("/tmp"),
            "/p.md".into(),
            "{}".into(),
        ))
    }

    #[tokio::test]
    async fn before_start_fails() {
        {
            let (process, _rx) = make();
            assert!(!process.send("hello"));
            assert!(!process.turn_active());
            assert!(!process.allow("req_x", None));
            assert!(!process.deny("req_x", "no"));
            assert!(!process.answer_question("req_x", json!({})));
        }
        {
            let (process, _rx) = make();
            let started = std::time::Instant::now();
            // No child to write to: none, immediately, and the waiter is gone.
            let outcome = process
                .control("req_1", json!({"subtype":"interrupt"}))
                .await;
            assert_eq!(outcome, None);
            assert!(started.elapsed() < Duration::from_millis(100));
            assert!(lock(&process.control_waiters).is_empty());
        }
    }
}
