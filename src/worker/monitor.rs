//! The detached worker monitor: owns one `pi --mode rpc` child, keeps
//! `runs/<id>/run.json` current, tails the RPC stream into `events.jsonl` and
//! `pi.log`, acts on `inbox.jsonl` envelopes, and mirrors the worker's
//! outbox. Ported from the TypeScript `src/monitor.ts`, with two additions
//! the TS monitor lacked: a `model` inbox envelope (switch the worker's
//! model mid-run) and answers for pi extension dialogs, so a skill or
//! extension command that opens a dialog never stalls the worker.
//!
//! Same lifecycle and timings as the TypeScript: 300 ms state flush, 150 ms
//! prompt delay, 300 ms mailbox poll, 2 s last-text timeout, and a 5 s
//! shutdown grace before SIGTERM then SIGKILL.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::Context as _;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use crate::cli::ExitCode;
use crate::fleet::envelope::{Decoded, Envelope};
use crate::fleet::run::{
    PendingDialog, PendingQuestion, PiCache, RunState, RunStatus, WorkerActivity, load_state,
    read_pi_cache, record_steering, record_tool_activity, save_state, write_pi_cache,
};
use crate::paths::{FleetPaths, env_var};
use crate::util::{append_json_line, append_text, now_iso, now_ms, read_new_lines};
use crate::worker::models::{worker_commands, worker_models};
use crate::worker::rpc::{
    ExtensionUiRequest, ExtensionUiResponse, ModelRef, RpcCommand, RpcMessage, RpcResponse,
    StreamPhase, StreamingBehavior, is_selected_event, parse_line,
};

const FLUSH_INTERVAL_MS: u64 = 300;
const PROMPT_DELAY_MS: u64 = 150;
const MAILBOX_POLL_MS: u64 = 300;
/// After settling: wait this long for `get_last_assistant_text`, then end stdin.
const LAST_TEXT_TIMEOUT_MS: u64 = 2000;
/// After ending stdin: escalate to SIGTERM, then SIGKILL.
const SHUTDOWN_GRACE_MS: u64 = 5000;
/// A dialog is cancelled this long before pi's own timeout would lapse, so the
/// worker never hangs waiting for an answer nobody will send.
const DIALOG_CANCEL_MARGIN_MS: u64 = 500;
/// When a dialog request carries no `timeout`, assume this long before
/// auto-cancelling (matching the extension's `fleet_ask` default).
const DEFAULT_DIALOG_TIMEOUT_MS: u64 = 10 * 60_000;
/// How many raw stderr chunks to keep for the error reason.
const STDERR_TAIL_CHUNKS: usize = 20;

/// Tools the fleet-worker extension registers; appended to any `--tools`
/// allowlist so a user allowlist cannot hide the worker protocol.
pub const FLEET_WORKER_TOOLS: [&str; 2] = ["fleet_ask", "fleet_progress"];

/// The worker extension, embedded so a single binary needs no package tree.
pub const FLEET_EXTENSION_TS: &str = include_str!("../../pi/extensions/fleet-worker.ts");
/// The report skill, embedded alongside the extension.
pub const FLEET_SKILL_MD: &str = include_str!("../../pi/skills/fleet-worker-report/SKILL.md");

/// `PILOTFISH_PI_BIN` is an executable spec split on spaces ("node /path/fake-pi.mjs")
/// so tests can point at a script; the default is the real `pi` on `PATH`.
pub fn pi_command() -> (String, Vec<String>) {
    pi_command_from(&std::env::var(env_var("PI_BIN")).unwrap_or_else(|_| "pi".to_string()))
}

/// [`pi_command`] with the spec injected (tests; env mutation is `unsafe` in
/// edition 2024).
fn pi_command_from(spec: &str) -> (String, Vec<String>) {
    let mut parts = spec
        .split(' ')
        .map(str::to_string)
        .filter(|part| !part.is_empty());
    let bin = parts.next().unwrap_or_else(|| "pi".to_string());
    (bin, parts.collect())
}

/// The argv for `pi --mode rpc`: the worker protocol travels with the CLI, so
/// `pi install` is optional — the extension and skill are passed explicitly.
#[must_use]
pub fn build_pi_args(
    state: &RunState,
    run_dir: &Path,
    extension: &Path,
    skill: &Path,
) -> Vec<String> {
    let mut args = vec![
        "--mode".to_string(),
        "rpc".to_string(),
        "--session-dir".to_string(),
        run_dir.join("session").to_string_lossy().into_owned(),
        "--extension".to_string(),
        extension.to_string_lossy().into_owned(),
        "--skill".to_string(),
        skill.to_string_lossy().into_owned(),
    ];
    let mut push = |flag: &str, value: &str| {
        args.push(flag.to_string());
        args.push(value.to_string());
    };
    if let Some(provider) = &state.provider {
        push("--provider", provider);
    }
    if let Some(model) = &state.model {
        push("--model", model);
    }
    if let Some(thinking) = &state.thinking {
        push("--thinking", thinking);
    }
    if let Some(skill) = &state.skill {
        push("--skill", skill);
    }
    if let Some(prompt) = &state.append_system_prompt {
        push("--append-system-prompt", prompt);
    }
    // A user allowlist must not hide the worker protocol tools.
    if let Some(tools) = &state.tools {
        push(
            "--tools",
            &format!("{tools},{}", FLEET_WORKER_TOOLS.join(",")),
        );
    }
    if let Some(exclude) = &state.exclude_tools {
        push("--exclude-tools", exclude);
    }
    if let Some(session) = &state.session_arg {
        push("--session", session);
    }
    args
}

/// The reminder appended to the task brief: the report path in the new
/// layout (`runs/<id>/report.md`, not the old `reports/<id>.md`).
#[must_use]
pub fn report_reminder(fleet_dir: &Path, run_id: &str) -> String {
    format!(
        "When you finish this task, write your fleet report to {}/runs/{run_id}/report.md \
         using the fleet-worker-report template before ending your final turn. \
         Include a \"Steering received\" section (\"none\" if you received no steering).",
        fleet_dir.display()
    )
}

/// Write the embedded extension and skill into the fleet dir, only when the
/// file is missing or its contents differ, and return their paths.
///
/// # Errors
///
/// Returns an I/O error when either file cannot be written.
pub fn materialize_worker_files(paths: &FleetPaths) -> std::io::Result<(PathBuf, PathBuf)> {
    let extension = paths.pi_extension();
    let skill = paths.pi_skill();
    write_if_changed(&extension, FLEET_EXTENSION_TS)?;
    write_if_changed(&skill, FLEET_SKILL_MD)?;
    Ok((extension, skill))
}

fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Yes/no mapping for `confirm` dialog answers arriving as console text.
fn is_affirmative(message: &str) -> bool {
    matches!(
        message.trim().to_lowercase().as_str(),
        "y" | "yes" | "true" | "1" | "ok" | "confirm" | "allow" | "always"
    )
}

/// The `extension_ui_response` for an `answer` envelope. Empty text dismisses
/// the dialog; `confirm` maps yes-ish words to `confirmed`.
fn dialog_reply(method: &str, id: &str, message: &str) -> ExtensionUiResponse {
    if message.trim().is_empty() {
        return ExtensionUiResponse::cancelled(id);
    }
    match method {
        "confirm" => ExtensionUiResponse::confirmed(id, is_affirmative(message)),
        _ => ExtensionUiResponse::value(id, message),
    }
}

/// Resolve the provider for a model id against pi's configured models.
/// `Err` names why it could not be resolved — unknown id or ambiguous across
/// providers; we never guess.
fn resolve_provider(models: &[ModelRef], model_id: &str) -> Result<String, String> {
    let mut providers: Vec<String> = models
        .iter()
        .filter(|m| m.id.as_deref() == Some(model_id))
        .filter_map(|m| m.provider.clone())
        .collect();
    providers.sort();
    providers.dedup();
    match providers.len() {
        1 => Ok(providers.remove(0)),
        0 => Err(format!(
            "model \"{model_id}\" is not in pi's available models"
        )),
        n => Err(format!(
            "model \"{model_id}\" is ambiguous across {n} providers: {}",
            providers.join(", ")
        )),
    }
}

/// Monitor-private facts about one open dialog: the answer escalation needs
/// the method, and the auto-cancel needs the deadline.
struct DialogRecord {
    id: String,
    method: String,
    /// Wall-clock ms after which the dialog is cancelled to pi.
    deadline_ms: i64,
}

/// Everything the monitor's tasks mutate, behind one std mutex (never held
/// across an await: the flush and every mutation are synchronous).
struct Shared {
    state: RunState,
    dirty: bool,
    /// Set once a `cleanup` archive is seen on disk; our writes stop there.
    archived_on_disk: bool,
    settled_handled: bool,
    pending_abort: bool,
    /// The last `turn_end`'s error: pi retries on its own, so an errored
    /// turn followed by a successful one settles normally — only the last
    /// turn before `agent_settled` decides. Set on every `turn_end`, cleared
    /// by a clean one.
    last_turn_error: Option<String>,
    finished: bool,
    prompt_sent: bool,
    shutdown_started: bool,
    abort_requests: u32,
    /// pi's configured models, cached for provider resolution.
    available_models: Vec<ModelRef>,
    /// The fleet-level pi catalogue, persisted to `pi-cache.json` instead of
    /// `run.json`: models and commands describe the pi installation, not the
    /// run, and every run used to carry a byte-identical copy.
    dialogs: Vec<DialogRecord>,
    stderr_tail: VecDeque<String>,
    /// Resolved when the post-settle `get_last_assistant_text` arrives, so
    /// the 2 s watcher can stop early.
    last_text_done: Option<oneshot::Sender<()>>,
    /// The thinking level we last asked pi for, held until the `get_state`
    /// that follows tells us what pi actually did with it.
    requested_thinking: Option<String>,
}

impl Shared {
    const fn finished(&self) -> bool {
        self.finished
    }
}

/// The monitor for one run. All long-lived handles are behind locks so the
/// stdout, mailbox, flusher and signal tasks can share them.
pub struct Monitor {
    fleet_dir: PathBuf,
    run_id: String,
    run_dir: PathBuf,
    events_path: PathBuf,
    pi_log_path: PathBuf,
    inbox_path: PathBuf,
    outbox_path: PathBuf,
    extension_path: PathBuf,
    skill_path: PathBuf,
    child_pid: AtomicI32,
    /// pi's stdin; `None` once shutdown has ended it.
    stdin: AsyncMutex<Option<tokio::process::ChildStdin>>,
    /// Serialises mailbox polling between the poller task and the settle drain.
    mailboxes: AsyncMutex<(u64, u64)>,
    shared: Mutex<Shared>,
}

/// Run the monitor for one run until the worker process exits. The CLI-level
/// exit code is always ok — how the run ended lives in `run.json`.
///
/// # Errors
///
/// Returns an error when the monitor cannot boot (unreadable or uncreatable
/// fleet files), or the run loop's own bookkeeping fails.
pub async fn run_monitor(fleet_dir: &Path, run_id: &str) -> anyhow::Result<ExitCode> {
    let monitor = Monitor::boot(fleet_dir, run_id)?;
    monitor.run().await?;
    Ok(ExitCode::Ok)
}

impl Monitor {
    fn boot(fleet_dir: &Path, run_id: &str) -> anyhow::Result<Arc<Self>> {
        let run_dir = fleet_dir.join("runs").join(run_id);
        let mut state = load_state(&run_dir)
            .with_context(|| format!("worker monitor cannot start for run {run_id}"))?;
        let paths = FleetPaths::new(fleet_dir);
        let (extension_path, skill_path) = materialize_worker_files(&paths)
            .with_context(|| "cannot materialize the worker extension and skill")?;
        state.pid = Some(i32::try_from(std::process::id()).unwrap_or(1));
        state.status = RunStatus::Running;
        let monitor = Arc::new(Self {
            fleet_dir: fleet_dir.to_path_buf(),
            run_id: run_id.to_string(),
            run_dir,
            events_path: paths.run_events(run_id),
            pi_log_path: paths.pi_log(run_id),
            inbox_path: paths.run_inbox(run_id),
            outbox_path: paths.run_outbox(run_id),
            extension_path,
            skill_path,
            child_pid: AtomicI32::new(0),
            stdin: AsyncMutex::new(None),
            mailboxes: AsyncMutex::new((0, 0)),
            shared: Mutex::new(Shared {
                state,
                dirty: false,
                archived_on_disk: false,
                settled_handled: false,
                pending_abort: false,
                last_turn_error: None,
                finished: false,
                prompt_sent: false,
                shutdown_started: false,
                abort_requests: 0,
                available_models: Vec::new(),
                dialogs: Vec::new(),
                stderr_tail: VecDeque::new(),
                last_text_done: None,
                requested_thinking: None,
            }),
        });
        // A missing pi.log is fine, but say we are here.
        let _ = append_text(
            &monitor.pi_log_path,
            &format!("[monitor] supervising run {run_id}\n"),
        );
        monitor.flush_now();
        Ok(monitor)
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        // A poisoned lock means a task panicked mid-mutation; the state is
        // still ours to fix up, so recover the guard instead of panicking.
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn run(self: &Arc<Self>) -> anyhow::Result<()> {
        let (cwd, args) = {
            let sh = self.shared();
            let cwd = sh
                .state
                .worktree
                .clone()
                .unwrap_or_else(|| sh.state.cwd.clone());
            let args = build_pi_args(
                &sh.state,
                &self.run_dir,
                &self.extension_path,
                &self.skill_path,
            );
            (cwd, args)
        };
        let (bin, prefix) = pi_command();
        let mut command = Command::new(&bin);
        command
            .args(prefix)
            .args(&args)
            .current_dir(&cwd)
            .env(env_var("RUN"), &self.run_id)
            .env(env_var("DIR"), &self.fleet_dir)
            // As in the TypeScript spawn: all three streams are ours to drive.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = append_text(
                    &self.pi_log_path,
                    &format!("[monitor] failed to start pi: {err}\n"),
                );
                self.finish(None, Some(err.to_string())).await;
                return Ok(());
            }
        };
        if let Some(pid) = child.id() {
            self.child_pid
                .store(i32::try_from(pid).unwrap_or(0), Ordering::SeqCst);
        }
        *self.stdin.lock().await = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let Some(stdout) = stdout {
            let monitor = self.clone();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let mut line = String::from_utf8_lossy(&buf).into_owned();
                    if line.ends_with('\n') {
                        line.pop();
                        if line.ends_with('\r') {
                            line.pop();
                        }
                    }
                    monitor.on_rpc_line(&line).await;
                }
            });
        }
        if let Some(stderr) = stderr {
            let monitor = self.clone();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let mut chunk = String::from_utf8_lossy(&buf).into_owned();
                    chunk.push('\n');
                    let mut sh = monitor.shared();
                    sh.stderr_tail.push_back(chunk);
                    while sh.stderr_tail.len() > STDERR_TAIL_CHUNKS {
                        sh.stderr_tail.pop_front();
                    }
                }
            });
        }

        let flusher = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(FLUSH_INTERVAL_MS)).await;
                if flusher.shared().finished() {
                    break;
                }
                if flusher.shared().dirty {
                    flusher.flush_now();
                }
            }
        });
        let poller = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(MAILBOX_POLL_MS)).await;
                if poller.shared().finished() {
                    break;
                }
                if !poller.shared().prompt_sent {
                    continue;
                }
                poller.poll_mailboxes().await;
                poller.cancel_expired_dialogs().await;
            }
        });
        let signal = self.clone();
        tokio::spawn(async move {
            let Ok(mut sigterm) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                return;
            };
            while sigterm.recv().await.is_some() {
                if signal.shared().finished() {
                    break;
                }
                signal.request_abort().await;
            }
        });

        // Ask pi what it actually resolved, what it offers, and then send the
        // brief — so the console can show the worker's real model and commands.
        tokio::time::sleep(Duration::from_millis(PROMPT_DELAY_MS)).await;
        if !self.shared().finished() {
            self.ask_capabilities().await;
            let brief = self.shared().state.task_brief.clone();
            self.write_event(json!({ "type": "task_prompt", "brief": brief }));
            self.send(&RpcCommand::Prompt {
                id: Some("fleet-init".to_string()),
                message: format!(
                    "{}\n\n{}",
                    brief,
                    report_reminder(&self.fleet_dir, &self.run_id)
                ),
                streaming_behavior: None,
            })
            .await;
            self.shared().prompt_sent = true;
        }

        let status = child.wait().await;
        self.finish(status.ok().and_then(|s| s.code()), None).await;
        Ok(())
    }

    /// One parsed stdout line: raw into `pi.log`, then dispatched.
    async fn on_rpc_line(self: &Arc<Self>, line: &str) {
        let _ = append_text(&self.pi_log_path, &format!("{line}\n"));
        match parse_line(line) {
            Some(RpcMessage::Response(response)) => self.handle_response(response).await,
            Some(RpcMessage::Ui(request)) => self.handle_ui_request(request),
            Some(RpcMessage::Event(event)) => self.handle_event(&event).await,
            None => {}
        }
    }

    async fn handle_response(self: &Arc<Self>, response: RpcResponse) {
        let command = response.command.as_deref().unwrap_or("").to_string();
        let success = response.success == Some(true);
        match (command.as_str(), success) {
            ("get_state", true) => {
                let levels = response.available_thinking_levels();
                let mut sh = self.shared();
                if let Some(model) = response.model() {
                    sh.state.active_model = model.id.or(model.name);
                    sh.state.active_provider = model.provider;
                }
                if !levels.is_empty() {
                    sh.state.available_thinking_levels = levels;
                }
                if let Some(level) = response.thinking_level() {
                    sh.state.thinking_level = Some(level);
                }
                // pi answers `success: true` to a level the model does not
                // have and then goes on running at the old one, so the only
                // honest report is the level it comes back with.
                let asked = sh.requested_thinking.take();
                let landed = sh.state.thinking_level.clone();
                let model = sh.state.active_model.clone();
                let available = sh.state.available_thinking_levels.clone();
                sh.dirty = true;
                drop(sh);
                if let Some(asked) = asked
                    && landed.as_deref() != Some(asked.as_str())
                {
                    self.write_event(json!({
                        "type": "thinking_unavailable",
                        "level": asked,
                        "level_now": landed,
                        "model": model,
                        "available": available,
                    }));
                }
            }
            ("set_thinking_level", _) => {
                // The level we asked for is only real once pi confirms it; a
                // refresh tells us what pi is actually running at.
                if success {
                    self.send(&RpcCommand::GetState {
                        id: Some("fleet-state".to_string()),
                    })
                    .await;
                } else {
                    self.write_event(json!({
                        "type": "thinking_rejected",
                        "error": response.error.clone().unwrap_or_else(|| "unknown error".to_string()),
                    }));
                }
            }
            ("set_model", _) => {
                if success {
                    self.send(&RpcCommand::GetState {
                        id: Some("fleet-state".to_string()),
                    })
                    .await;
                } else {
                    self.write_event(json!({
                        "type": "model_rejected",
                        "error": response.error.clone().unwrap_or_else(|| "unknown error".to_string()),
                    }));
                }
            }
            ("get_commands", true) => {
                let commands = worker_commands(&response.commands());
                // Commands describe the pi installation, not the run: they go
                // to the fleet-level cache, never into run.json.
                self.persist_pi_cache(|cache| cache.commands = commands);
            }
            ("get_available_models", true) => {
                let models = response.available_models();
                let worker_models = worker_models(&models);
                // Same fleet-level treatment as commands; the `ModelRef` list
                // still stays on the monitor for provider resolution. An empty
                // answer is pi being briefly unaskable (the open list_models
                // issue), not pi having no models, so it replaces nothing.
                self.shared().available_models = models;
                if !worker_models.is_empty() {
                    self.persist_pi_cache(|cache| cache.available_models = worker_models);
                }
            }
            ("get_last_assistant_text", true) => {
                if let Some(text) = response.text() {
                    self.shared().state.last_assistant_text = Some(text);
                }
                self.flush_now();
                let done = self.shared().last_text_done.take();
                if let Some(done) = done {
                    let _ = done.send(());
                }
                self.begin_shutdown().await;
            }
            (_, false) => {
                if let Some(raw) = response.raw.clone() {
                    self.write_event(raw);
                }
                if response.id.as_deref() == Some("fleet-init") {
                    self.shared().state.error = Some(format!(
                        "prompt rejected: {}",
                        response
                            .error
                            .clone()
                            .unwrap_or_else(|| "unknown error".to_string())
                    ));
                    self.flush_now();
                    self.begin_shutdown().await;
                }
            }
            _ => {}
        }
    }

    async fn handle_event(self: &Arc<Self>, event: &crate::worker::rpc::RpcEvent) {
        if event.kind == "message_update" {
            match event.stream_phase() {
                Some(StreamPhase::Thinking) => {
                    // The console shows "thinking…" for as long as this lasts.
                    let mut sh = self.shared();
                    if sh.state.activity != Some(WorkerActivity::Thinking) {
                        sh.state.activity = Some(WorkerActivity::Thinking);
                        sh.state.last_activity = Some(now_iso());
                        sh.dirty = true;
                    }
                }
                Some(StreamPhase::Text) => {
                    if let Some(mirrored) = event.mirrored_message_update() {
                        self.write_event(mirrored);
                    }
                    let mut sh = self.shared();
                    sh.state.activity = Some(WorkerActivity::Text);
                    sh.state.last_activity = Some(now_iso());
                    sh.dirty = true;
                }
                _ => {}
            }
            return;
        }
        if !is_selected_event(&event.kind) {
            return;
        }
        self.write_event(event.raw.clone());
        match event.kind.as_str() {
            "tool_execution_start" | "tool_execution_end" => {
                let mut sh = self.shared();
                let tool = event
                    .tool_name()
                    .map(str::to_string)
                    .or_else(|| sh.state.last_tool.clone());
                record_tool_activity(&mut sh.state, tool.as_deref());
                sh.state.activity = Some(WorkerActivity::Tool);
                sh.dirty = true;
            }
            "turn_end" => {
                // Remember the last turn's outcome so `agent_settled` knows
                // whether pi failed (a 402 or OAuth refresh error) or retried
                // its way to a clean turn before settling.
                let mut sh = self.shared();
                sh.last_turn_error = last_turn_error(&event.raw);
            }
            "agent_settled" => {
                let already = self.shared().settled_handled;
                if already {
                    return;
                }
                // Everything the worker wrote before settling must be mirrored
                // before observers can see the terminal status.
                if self.shared().prompt_sent {
                    self.poll_mailboxes().await;
                }
                {
                    let mut sh = self.shared();
                    sh.settled_handled = true;
                    if sh.pending_abort {
                        sh.state.status = RunStatus::Stopped;
                    } else if let Some(error) = sh.last_turn_error.clone() {
                        sh.state.status = RunStatus::Error;
                        sh.state.error = Some(error);
                    } else {
                        sh.state.status = RunStatus::Settled;
                    }
                    sh.state.settled_at = Some(now_iso());
                    sh.state.pending_question = None;
                    sh.state.pending_dialog = None;
                    sh.dialogs.clear();
                    sh.state.activity = None;
                }
                self.flush_now();
                self.send(&RpcCommand::GetLastAssistantText {
                    id: Some("fleet-last".to_string()),
                })
                .await;
                let (done, watcher) = oneshot::channel();
                self.shared().last_text_done = Some(done);
                let monitor = self.clone();
                tokio::spawn(async move {
                    // Whether the text arrives or the 2 s lapses, pi's work is
                    // done: end stdin either way.
                    let _ =
                        tokio::time::timeout(Duration::from_millis(LAST_TEXT_TIMEOUT_MS), watcher)
                            .await;
                    monitor.begin_shutdown().await;
                });
            }
            _ => {}
        }
    }

    fn handle_ui_request(self: &Arc<Self>, request: ExtensionUiRequest) {
        if !request.is_dialog() {
            // notify/setStatus/setWidget/setTitle/set_editor_text need no
            // reply: record and move on.
            if let Some(raw) = request.raw {
                self.write_event(raw);
            }
            return;
        }
        let question = request.display_question();
        let asked_at = now_iso();
        let timeout = request.timeout.unwrap_or(DEFAULT_DIALOG_TIMEOUT_MS);
        {
            let mut sh = self.shared();
            sh.state.pending_dialog = Some(PendingDialog {
                id: request.id.clone(),
                method: request.method.clone(),
                question: question.clone(),
                options: request.options.clone(),
                context: None,
                asked_at,
            });
            sh.state.last_activity = Some(now_iso());
            sh.dialogs.push(DialogRecord {
                id: request.id.clone(),
                method: request.method.clone(),
                deadline_ms: now_ms()
                    + i64::try_from(timeout.saturating_sub(DIALOG_CANCEL_MARGIN_MS))
                        .unwrap_or(i64::MAX),
            });
            sh.dirty = true;
        }
        self.write_event(json!({
            "type": "worker_dialog",
            "questionId": request.id,
            "method": request.method,
            "question": question,
            "options": request.options,
            "context": Value::Null,
        }));
        self.flush_now();
    }

    async fn handle_inbox(self: &Arc<Self>, envelope: &Envelope, decoded: Decoded<'_>) {
        match decoded {
            Decoded::Steer(message) => self.deliver_steering("steer", envelope, message).await,
            Decoded::FollowUp(message) => {
                self.deliver_steering("follow_up", envelope, message).await;
            }
            Decoded::Command(message) => {
                let source = envelope.from.to_string();
                if self.shared().settled_handled {
                    self.write_event(json!({
                        "type": "control_dropped",
                        "control": "command",
                        "source": source,
                        "reason": "run already settled",
                    }));
                    return;
                }
                // `prompt` (not `steer`) is the only delivery that runs
                // extension commands; with streamingBehavior it lands like a
                // steer for everything else.
                let delivered = self
                    .send(&RpcCommand::Prompt {
                        id: Some(format!("fleet-cmd-{}", now_ms())),
                        message: message.to_string(),
                        streaming_behavior: Some(StreamingBehavior::Steer),
                    })
                    .await;
                if !delivered {
                    return;
                }
                self.write_event(json!({
                    "type": "command_delivered",
                    "source": source,
                    "message": message,
                }));
                let mut sh = self.shared();
                record_steering(
                    &mut sh.state,
                    &source,
                    &now_iso(),
                    &format!("command: {message}"),
                );
                sh.dirty = true;
                drop(sh);
                self.flush_now();
            }
            Decoded::Thinking(level) => {
                let delivered = self
                    .send(&RpcCommand::SetThinkingLevel {
                        id: Some("fleet-thinking".to_string()),
                        level: level.to_string(),
                    })
                    .await;
                if delivered {
                    self.shared().requested_thinking = Some(level.to_string());
                    self.write_event(json!({
                        "type": "thinking_requested",
                        "source": envelope.from.to_string(),
                        "level": level,
                    }));
                }
            }
            Decoded::RefreshCapabilities => {
                self.ask_capabilities().await;
            }
            Decoded::Abort => {
                self.write_event(json!({
                    "type": "abort_requested",
                    "source": envelope.from.to_string(),
                }));
                self.request_abort().await;
            }
            Decoded::Answer {
                message,
                question_id,
            } => {
                // Never drop the answer: the worker may consume it and settle
                // before this poll runs, and the record must still say who
                // answered. A matching dialog id also goes to pi's stdin.
                let Some(message) = message else {
                    return;
                };
                let dialog = {
                    let sh = self.shared();
                    sh.dialogs
                        .iter()
                        .find(|d| Some(d.id.as_str()) == question_id)
                        .map(|d| (d.id.clone(), d.method.clone()))
                };
                if let Some((id, method)) = dialog {
                    let reply = dialog_reply(&method, &id, message);
                    self.send_line(&reply.to_line()).await;
                    let mut sh = self.shared();
                    sh.dialogs.retain(|d| d.id != id);
                    if sh.state.pending_dialog.as_ref().is_some_and(|d| d.id == id) {
                        sh.state.pending_dialog = None;
                    }
                }
                let source = envelope.from.to_string();
                self.write_event(json!({
                    "type": "answer_delivered",
                    "questionId": question_id,
                    "source": source,
                    "message": message,
                }));
                let mut sh = self.shared();
                record_steering(
                    &mut sh.state,
                    &source,
                    &now_iso(),
                    &format!("answer({}): {message}", question_id.unwrap_or("?")),
                );
                if sh
                    .state
                    .pending_question
                    .as_ref()
                    .is_some_and(|q| question_id.is_none_or(|id| q.id == id))
                {
                    sh.state.pending_question = None;
                }
                sh.dirty = true;
                drop(sh);
                self.flush_now();
            }
            Decoded::Model { model_id, provider } => {
                self.write_event(json!({
                    "type": "model_requested",
                    "source": envelope.from.to_string(),
                    "model": model_id,
                    "provider": provider,
                }));
                let resolved = provider.map_or_else(
                    || {
                        let models = self.shared().available_models.clone();
                        resolve_provider(&models, model_id)
                    },
                    |provider| Ok(provider.to_string()),
                );
                match resolved {
                    Ok(provider) => {
                        self.send(&RpcCommand::SetModel {
                            id: Some("fleet-model".to_string()),
                            provider: Some(provider),
                            model_id: model_id.to_string(),
                        })
                        .await;
                    }
                    // Say why instead of guessing a provider.
                    Err(reason) => {
                        self.write_event(json!({
                            "type": "model_unresolved",
                            "model": model_id,
                            "reason": reason,
                        }));
                    }
                }
            }
            // Outbox-only types never arrive on the inbox.
            _ => {}
        }
    }

    async fn deliver_steering(self: &Arc<Self>, kind: &str, envelope: &Envelope, message: &str) {
        let source = envelope.from.to_string();
        if self.shared().settled_handled {
            self.write_event(json!({
                "type": "control_dropped",
                "control": kind,
                "source": source,
                "reason": "run already settled",
            }));
            return;
        }
        let delivered = if kind == "steer" {
            self.send(&RpcCommand::Steer {
                message: message.to_string(),
            })
            .await
        } else {
            self.send(&RpcCommand::FollowUp {
                message: message.to_string(),
            })
            .await
        };
        if !delivered {
            return;
        }
        self.write_event(json!({
            "type": "steering_delivered",
            "source": source,
            "message": message,
        }));
        let mut sh = self.shared();
        record_steering(&mut sh.state, &source, &now_iso(), message);
        sh.dirty = true;
        drop(sh);
        self.flush_now();
    }

    fn handle_outbox(self: &Arc<Self>, envelope: &Envelope) {
        let Some(decoded) = envelope.decode() else {
            return;
        };
        match decoded {
            Decoded::Question(payload) => {
                self.write_event(json!({
                    "type": "worker_question",
                    "questionId": envelope.id,
                    "question": payload.question,
                    "options": payload.options,
                    "context": payload.context,
                }));
                let mut sh = self.shared();
                sh.state.pending_question = Some(PendingQuestion {
                    id: envelope.id.clone(),
                    question: payload.question,
                    options: payload.options,
                    context: payload.context,
                    asked_at: envelope.ts.clone(),
                });
                sh.state.last_activity = Some(now_iso());
                drop(sh);
                self.flush_now();
            }
            Decoded::Progress(message) => {
                self.write_event(json!({ "type": "worker_progress", "message": message }));
                let mut sh = self.shared();
                sh.state.last_progress = Some(message);
                sh.state.last_activity = Some(now_iso());
                sh.dirty = true;
            }
            Decoded::QuestionResolved { question_id, how } => {
                let how = serde_json::to_value(how)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                self.write_event(json!({
                    "type": "worker_question_resolved",
                    "questionId": question_id,
                    "how": how,
                }));
                let mut sh = self.shared();
                if sh
                    .state
                    .pending_question
                    .as_ref()
                    .is_some_and(|q| q.id == question_id)
                {
                    sh.state.pending_question = None;
                }
                drop(sh);
                self.flush_now();
            }
            _ => {}
        }
    }

    /// Drain both mailboxes once. Guarded by a mutex because the poller task
    /// and the settle drain can race. The lock covers only the offset reads
    /// and writes — never the per-envelope handling, which awaits.
    async fn poll_mailboxes(self: &Arc<Self>) {
        let lines = {
            let mut offsets = self.mailboxes.lock().await;
            let (lines, offset) = read_new_lines(&self.inbox_path, offsets.0);
            offsets.0 = offset;
            lines
        };
        for line in lines {
            let Some(envelope) = Envelope::parse_line(&line) else {
                continue;
            };
            if let Some(decoded) = envelope.decode() {
                self.handle_inbox(&envelope, decoded).await;
            }
        }
        let lines = {
            let mut offsets = self.mailboxes.lock().await;
            let (lines, offset) = read_new_lines(&self.outbox_path, offsets.1);
            offsets.1 = offset;
            lines
        };
        for line in lines {
            let Some(envelope) = Envelope::parse_line(&line) else {
                continue;
            };
            self.handle_outbox(&envelope);
        }
    }

    /// Send `cancelled` for dialogs nobody answered, shortly before pi's own
    /// timeout would lapse.
    async fn cancel_expired_dialogs(self: &Arc<Self>) {
        let now = now_ms();
        let expired: Vec<String> = {
            let sh = self.shared();
            sh.dialogs
                .iter()
                .filter(|d| d.deadline_ms <= now)
                .map(|d| d.id.clone())
                .collect()
        };
        for id in expired {
            let cancelled = ExtensionUiResponse::cancelled(&id).to_line();
            self.send_line(&cancelled).await;
            {
                let mut sh = self.shared();
                sh.dialogs.retain(|d| d.id != id);
                if sh.state.pending_dialog.as_ref().is_some_and(|d| d.id == id) {
                    sh.state.pending_dialog = None;
                }
            }
            self.write_event(json!({
                "type": "dialog_cancelled",
                "questionId": id,
                "reason": "no answer arrived",
            }));
        }
    }

    /// First abort request: RPC abort. Second: SIGTERM pi. Third: SIGKILL pi.
    /// Ask pi what it resolved, what it offers and what models it has. Sent
    /// at boot and again whenever a console asks: pi's command list grows
    /// when a skill or extension is added, so one answer at boot goes stale.
    async fn ask_capabilities(self: &Arc<Self>) {
        self.send(&RpcCommand::GetState {
            id: Some("fleet-state".to_string()),
        })
        .await;
        self.send(&RpcCommand::GetCommands {
            id: Some("fleet-commands".to_string()),
        })
        .await;
        self.send(&RpcCommand::GetAvailableModels {
            id: Some("fleet-models".to_string()),
        })
        .await;
    }

    async fn request_abort(self: &Arc<Self>) {
        let request = {
            let mut sh = self.shared();
            sh.pending_abort = true;
            sh.abort_requests += 1;
            sh.abort_requests
        };
        match request {
            1 => {
                self.send(&RpcCommand::Abort).await;
            }
            2 => self.signal_child(nix::sys::signal::Signal::SIGTERM),
            _ => self.signal_child(nix::sys::signal::Signal::SIGKILL),
        }
    }

    fn signal_child(&self, signal: nix::sys::signal::Signal) {
        let pid = self.child_pid.load(Ordering::SeqCst);
        if pid > 0 {
            // A pid that vanished between the store and here is fine.
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal);
        }
    }

    /// Ask pi to exit: end stdin, then escalate if it lingers.
    async fn begin_shutdown(self: &Arc<Self>) {
        {
            let mut sh = self.shared();
            if sh.shutdown_started {
                return;
            }
            sh.shutdown_started = true;
        }
        *self.stdin.lock().await = None;
        let term = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(SHUTDOWN_GRACE_MS)).await;
            if !term.shared().finished() {
                term.signal_child(nix::sys::signal::Signal::SIGTERM);
            }
        });
        let kill = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(SHUTDOWN_GRACE_MS * 2)).await;
            if !kill.shared().finished() {
                kill.signal_child(nix::sys::signal::Signal::SIGKILL);
            }
        });
    }

    /// Update one field of the fleet-level pi catalogue in `pi-cache.json`.
    ///
    /// Read, change the one field that just arrived, write — never a
    /// whole-file write from this monitor's memory. Several monitors boot
    /// side by side, and each learns commands and models in two separate
    /// replies; writing a whole cache after the first reply once wiped the
    /// models list another spawn was about to route from.
    ///
    /// Best-effort: a failed write degrades to an older catalogue, never an
    /// error path for the run, and is logged to `pi.log` for diagnosis.
    fn persist_pi_cache(&self, update: impl FnOnce(&mut PiCache)) {
        let mut cache = read_pi_cache(&self.fleet_dir).unwrap_or_default();
        update(&mut cache);
        if let Err(err) = write_pi_cache(&self.fleet_dir, &cache) {
            let _ = append_text(
                &self.pi_log_path,
                &format!("[monitor] failed to write pi-cache.json: {err}\n"),
            );
        }
    }

    /// Write one line to pi's stdin. `false` when pi is gone or stdin ended —
    /// an EPIPE racing pi's death must not kill the monitor before it records
    /// why the run ended.
    async fn send_line(&self, line: &str) -> bool {
        let mut stdin = self.stdin.lock().await;
        match stdin.as_mut() {
            Some(stdin) => stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_ok(),
            None => false,
        }
    }

    async fn send(&self, command: &RpcCommand) -> bool {
        self.send_line(&command.to_line()).await
    }

    /// Append one event to `events.jsonl` with a fresh timestamp. Best-effort:
    /// events are advisory, `run.json` is the source of truth.
    fn write_event(&self, mut event: Value) {
        if let Value::Object(map) = &mut event {
            map.insert("ts".into(), Value::String(now_iso()));
        }
        let _ = append_json_line(&self.events_path, &event);
    }

    /// Flush the run state. Flushes are serialized (one std mutex, no awaits),
    /// and never overwrite an `archived` state written by `cleanup` — that
    /// write can land between our settle flush and the child's close.
    fn flush_now(&self) {
        let mut sh = self.shared();
        sh.dirty = false;
        if sh.archived_on_disk {
            return;
        }
        if load_state(&self.run_dir).is_ok_and(|disk| disk.status == RunStatus::Archived) {
            sh.archived_on_disk = true;
            return;
        }
        // A missing or mid-rename run.json on disk: write ours.
        if save_state(&self.run_dir, &sh.state).is_err() {
            sh.dirty = true; // retried by the periodic flusher
        }
    }

    /// Final bookkeeping once the child is gone (or failed to spawn). The
    /// monitor's exit code is always ok; how the run ended is in `run.json`.
    async fn finish(self: &Arc<Self>, code: Option<i32>, spawn_error: Option<String>) {
        {
            let mut sh = self.shared();
            if sh.finished {
                return;
            }
            sh.finished = true;
            sh.last_text_done = None;
        }
        // Lines written in the last poll interval (e.g. a question_resolved
        // right before settling) must still land in events.jsonl.
        if self.shared().prompt_sent {
            self.poll_mailboxes().await;
        }
        let failure = {
            let mut sh = self.shared();
            if sh.state.status == RunStatus::Settled || sh.state.status == RunStatus::Stopped {
                None
            } else {
                sh.state.status = RunStatus::Error;
                let stderr = sh
                    .stderr_tail
                    .iter()
                    .map(String::as_str)
                    .collect::<String>();
                let lines: Vec<&str> = stderr.split('\n').filter(|line| !line.is_empty()).collect();
                let start = lines.len().saturating_sub(8);
                let tail = lines[start..].join("\n");
                let reason = spawn_error.map_or_else(
                    || {
                        format!(
                            "pi exited with code {} before settling",
                            code.map_or_else(|| "unknown".to_string(), |c| c.to_string())
                        )
                    },
                    |err| format!("failed to start pi: {err}"),
                );
                let error = sh.state.error.clone().unwrap_or_else(|| {
                    if tail.is_empty() {
                        reason
                    } else {
                        format!("{reason}\n{tail}")
                    }
                });
                sh.state.error = Some(error.clone());
                Some(error)
            }
        };
        // The rail can only show a clipped label, so the whole reason belongs
        // in the transcript where it can be read.
        if let Some(error) = failure {
            self.write_event(json!({ "type": "run_failed", "error": error }));
        }
        {
            let mut sh = self.shared();
            if sh.state.settled_at.is_none() {
                sh.state.settled_at = Some(now_iso());
            }
            sh.state.pending_question = None;
            sh.state.pending_dialog = None;
        }
        self.flush_now();
    }
}

/// The error of a `turn_end` whose `message.stopReason` is `"error"`; `None`
/// for a clean turn, which clears an earlier error — pi retries on its own,
/// and only the last turn before `agent_settled` decides the run's fate.
fn last_turn_error(event: &Value) -> Option<String> {
    let message = event.get("message")?;
    if message.get("stopReason").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let text = match message.get("errorMessage") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(value) if !value.is_null() => value.to_string(),
        _ => "turn ended with an error".to_string(),
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::RunState;

    fn base_state() -> RunState {
        RunState::new(
            "/f/.pilotfish",
            "r-20260828141530",
            "r",
            "/w",
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
        )
    }

    #[test]
    fn pi_launch() {
        {
            let run_dir = Path::new("/f/.pilotfish/runs/r-1");
            let ext = Path::new("/f/.pilotfish/pi/extensions/fleet-worker.ts");
            let skill = Path::new("/f/.pilotfish/pi/skills/fleet-worker-report/SKILL.md");
            let args = build_pi_args(&base_state(), run_dir, ext, skill);
            assert_eq!(
                args,
                vec![
                    "--mode",
                    "rpc",
                    "--session-dir",
                    "/f/.pilotfish/runs/r-1/session",
                    "--extension",
                    "/f/.pilotfish/pi/extensions/fleet-worker.ts",
                    "--skill",
                    "/f/.pilotfish/pi/skills/fleet-worker-report/SKILL.md",
                ]
            );
            let mut state = base_state();
            state.provider = Some("anthropic".into());
            state.model = Some("fable".into());
            state.thinking = Some("high".into());
            state.skill = Some("/extra/skill".into());
            state.append_system_prompt = Some("be terse".into());
            state.tools = Some("read,bash".into());
            state.exclude_tools = Some("web".into());
            state.session_arg = Some("abc123".into());
            let args = build_pi_args(&state, run_dir, ext, skill);
            // The last occurrence: `--skill` appears twice (worker protocol + user).
            let pair = |flag: &str| {
                let at = args.iter().rposition(|a| a == flag).unwrap();
                args[at + 1].clone()
            };
            assert_eq!(pair("--provider"), "anthropic");
            assert_eq!(pair("--model"), "fable");
            assert_eq!(pair("--thinking"), "high");
            assert_eq!(pair("--skill"), "/extra/skill");
            assert_eq!(pair("--append-system-prompt"), "be terse");
            // A user allowlist must still admit the worker protocol tools.
            assert_eq!(pair("--tools"), "read,bash,fleet_ask,fleet_progress");
            assert_eq!(pair("--exclude-tools"), "web");
            assert_eq!(pair("--session"), "abc123");
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let paths = FleetPaths::new(dir.path());
            let (ext, skill) = materialize_worker_files(&paths).unwrap();
            assert_eq!(std::fs::read_to_string(&ext).unwrap(), FLEET_EXTENSION_TS);
            assert_eq!(std::fs::read_to_string(&skill).unwrap(), FLEET_SKILL_MD);
            assert!(FLEET_SKILL_MD.starts_with("---\nname: fleet-worker-report\n"));
            // A stale file is refreshed.
            std::fs::write(&ext, "stale").unwrap();
            materialize_worker_files(&paths).unwrap();
            assert_eq!(std::fs::read_to_string(&ext).unwrap(), FLEET_EXTENSION_TS);
        }
    }

    #[test]
    fn provider_and_dialogs() {
        {
            let models = vec![
                ModelRef {
                    id: Some("m-1".into()),
                    name: None,
                    provider: Some("vendorco".into()),
                    ..ModelRef::default()
                },
                ModelRef {
                    id: Some("m-1".into()),
                    name: None,
                    provider: Some("vendorco".into()),
                    ..ModelRef::default()
                },
                ModelRef {
                    id: Some("m-2".into()),
                    name: None,
                    provider: Some("other".into()),
                    ..ModelRef::default()
                },
            ];
            assert_eq!(
                resolve_provider(&models, "m-1").unwrap(),
                "vendorco".to_string()
            );
            assert!(resolve_provider(&models, "ghost").is_err());
            let ambiguous = vec![
                ModelRef {
                    id: Some("m".into()),
                    name: None,
                    provider: Some("a".into()),
                    ..ModelRef::default()
                },
                ModelRef {
                    id: Some("m".into()),
                    name: None,
                    provider: Some("b".into()),
                    ..ModelRef::default()
                },
            ];
            let err = resolve_provider(&ambiguous, "m").unwrap_err();
            assert!(err.contains("ambiguous"), "{err}");
        }
        {
            assert_eq!(
                dialog_reply("confirm", "u1", "yes"),
                ExtensionUiResponse::confirmed("u1", true)
            );
            assert_eq!(
                dialog_reply("confirm", "u1", "no"),
                ExtensionUiResponse::confirmed("u1", false)
            );
            assert_eq!(
                dialog_reply("select", "u2", "b"),
                ExtensionUiResponse::value("u2", "b")
            );
            assert_eq!(
                dialog_reply("input", "u3", "  "),
                ExtensionUiResponse::cancelled("u3")
            );
            assert!(is_affirmative(" OK "));
            assert!(!is_affirmative("nope"));
        }
    }
}
