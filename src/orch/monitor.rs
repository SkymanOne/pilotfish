//! The detached owner of the `claude` child. It keeps the orchestrator alive
//! while consoles come and go: everything claude says lands in
//! `events.jsonl` and `state.json`, and anything a console wants done arrives
//! through `inbox.jsonl` envelopes. Quitting a console leaves this process
//! running; a `stop` command line is what ends it.
//!
//! Ported from the TypeScript `src/orchestrator/monitor.ts` onto tokio: the
//! Node EventEmitter handlers become one event-loop task over
//! [`ProcEvent`]s, and the timers become tasks sharing a mutex-guarded
//! [`Shared`]. The same timings are kept: 200 ms state flush, 200 ms inbox
//! poll, 150 ms stream-text flush. State writes are serialised under one
//! mutex — the Rust equivalent of the TypeScript `writeChain` promise, for
//! the same reason: two concurrent atomic writes can land out of order, and
//! an older one winning would drop things a console needs to see — a
//! permission request waiting for an answer, say.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::cli::ExitCode;
use crate::fleet::envelope::Envelope;
use crate::orch::process::{
    ControlOutcome, ExitInfo, OrchestratorOptions, OrchestratorProcess, ProcEvent,
};
use crate::orch::protocol::{PermissionRequest, SystemInitMessage};
use crate::orch::records::{
    self, Activity, ActivityKind, OrchestratorCommand, OrchestratorState, PermissionDecisionRecord,
    Transcript, decode_command, new_orchestrator_state, request_value,
};
use crate::orch::session;
use crate::paths::{FleetPaths, SessionKey};
use crate::util::{atomic_write_json, now_iso, read_new_lines};

const FLUSH_MS: u64 = 200;
const CONTROL_POLL_MS: u64 = 200;
/// Token deltas are coalesced into one record per tick, so the file stays
/// small. Short, because this interval is the first of the three that stand
/// between a token and the screen (the other two are the console's tail poll
/// and its draw).
const STREAM_FLUSH_MS: u64 = 50;
/// How many lines `events.jsonl` may hold while the monitor runs. The file
/// is the console's replay source, so it has to stay readable in one gulp;
/// past this the head is dropped and the console replays what is left.
const MAX_EVENT_LINES: usize = 5_000;
/// What `/trim` cuts the transcript down to: enough to restore the recent
/// conversation, nothing older.
const TRIM_TO_LINES: usize = 2_000;
/// Consecutive polls with the fleet directory missing before the monitor
/// accepts that it was deleted or moved out from under it: a single
/// transient `NotFound` under load is not a deletion.
const MISSING_DIR_POLLS: u32 = 2;
/// How often the monitor stamps its session's heartbeat, well inside
/// [`session::HEARTBEAT_GRACE_MS`] so two missed stamps mark a wedge.
const HEARTBEAT_WRITE_MS: i64 = 5_000;

/// Run the orchestrator monitor for the fleet rooted at `fleet_dir`,
/// serving the session `session` (by uuid) or — when none is named — the
/// most recently used one.
///
/// # Errors
///
/// Returns an error when the monitor cannot boot (unreadable session record,
/// unwritable prompt or MCP config), or when the run loop itself fails.
pub async fn run_orchestrator_monitor(
    fleet_dir: &Path,
    session: Option<Uuid>,
) -> anyhow::Result<ExitCode> {
    let monitor = Monitor::boot(fleet_dir, session)?;
    monitor.run().await?;
    Ok(ExitCode::Ok)
}

/// Read `<fleet>/orchestrators/<key>/state.json`, or none when there is none
/// yet.
#[must_use]
pub fn load_orchestrator_state(fleet_dir: &Path, key: &SessionKey) -> Option<OrchestratorState> {
    let path = FleetPaths::new(fleet_dir).orchestrator_state(key);
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Append one command to the session's inbox (the console's side of the
/// mailbox); the monitor picks it up within its poll interval.
///
/// # Errors
///
/// Returns an I/O error when the inbox cannot be created or written.
pub fn append_command(
    fleet_dir: &Path,
    key: &SessionKey,
    command: &OrchestratorCommand,
) -> std::io::Result<()> {
    append_envelope(
        fleet_dir,
        key,
        &command.to_envelope(crate::fleet::envelope::Party::Console),
    )
}

/// Append a pre-built envelope to the session's inbox.
///
/// # Errors
///
/// Returns an I/O error when the inbox cannot be created or written.
pub fn append_envelope(
    fleet_dir: &Path,
    key: &SessionKey,
    envelope: &Envelope,
) -> std::io::Result<()> {
    let path = FleetPaths::new(fleet_dir).orchestrator_inbox(key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::fleet::envelope::append_envelope(&path, envelope)
}

/// The claude child being replaced (Remote Control needs a new flag), noted
/// so the monitor does not mistake a restart for an exit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Restart {
    /// The Remote Control name for the replacement child; none turns it off.
    remote_control: Option<String>,
}

/// Everything the monitor's tasks mutate, behind one std mutex. Never hold
/// the guard across an await: every mutation and write here is synchronous.
struct Shared {
    state: OrchestratorState,
    /// What the agent currently offers. Kept beside the state but written to
    /// its own file, because it is asked for rather than accumulated.
    caps: records::Capabilities,
    /// The working copy of this monitor's session record; [`Monitor::save_record`]
    /// merges it into a fresh read of `fleet.json` and persists the store,
    /// so the two only ever differ between a mutation and the next flush.
    record: session::OrchestratorSession,
    transcript: Transcript,
    /// Permission requests still waiting, by request id; `state.json` holds
    /// the sorted view.
    pending: HashMap<String, PermissionRequest>,
    dirty: bool,
    finished: bool,
    /// Set while the child is being replaced rather than shut down.
    restart: Option<Restart>,
    process: Option<Arc<OrchestratorProcess>>,
    /// Byte offset into `inbox.jsonl` already consumed.
    control_offset: u64,
    /// Consecutive polls that found the fleet directory missing.
    dir_missing_polls: u32,
    /// Epoch ms of the last heartbeat write; the first poll writes at once.
    last_heartbeat_written: Option<i64>,
    /// Turns since the last compaction, counted here because claude's own
    /// count restarts with every query.
    turns_since_compact: u32,
    /// A `/compact` was sent and its result has not come back yet.
    compacting: bool,
    /// Set once the fleet directory loss (or an answering stop) ends the
    /// session; blocks a flag-change restart from respawning a child into a
    /// directory that no longer exists.
    shutting_down: bool,
}

/// The monitor for one fleet's orchestrator.
pub struct Monitor {
    fleet_dir: PathBuf,
    cwd: PathBuf,
    /// The session this monitor serves and its layout key; every
    /// `orchestrators/` path derives from it.
    key: SessionKey,
    /// The prompt document claude reads via --append-system-prompt-file.
    prompt_file: String,
    /// The `--mcp-config` document pointing claude at this binary's MCP server.
    mcp_config_json: String,
    pid: i32,
    /// The model asked for at launch; claude's own default when none.
    launch_model: Option<String>,
    /// Turns between automatic `/compact`s, or none when it is switched off.
    auto_compact_turns: Option<u32>,
    budget_usd: Option<f64>,
    paths: FleetPaths,
    shared: Mutex<Shared>,
}

impl Monitor {
    /// Load the session record and prepare the state, without a child yet.
    /// `session` pins the monitor to one session by uuid; without it, the
    /// most recently used row is served (a console that created a fresh row
    /// just before spawning makes that row the one).
    fn boot(fleet_dir: &Path, session: Option<Uuid>) -> anyhow::Result<Arc<Self>> {
        let paths = FleetPaths::new(fleet_dir);
        // The session this monitor serves: the one the console named, or
        // the most recently used one. A console always writes the row
        // before spawning the monitor, so a named session missing here is
        // a broken spawn, never a silent fallback.
        let store = session::load(fleet_dir).unwrap_or_default();
        let stored = match session {
            Some(uuid) => Some(store.sessions.get(&uuid).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "no session {uuid} in {} — the console must create a session \
                             before spawning its monitor",
                    paths.fleet_json().display()
                )
            })?),
            None => store.last_used().cloned(),
        };
        // The console records the repo it opened; without a session record
        // that is the directory holding the fleet.
        let cwd_string = stored
            .as_ref()
            .map(|s| s.cwd.clone())
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or_else(|| {
                fleet_dir
                    .parent()
                    .unwrap_or(fleet_dir)
                    .to_string_lossy()
                    .into_owned()
            });
        let cwd = PathBuf::from(&cwd_string);
        let adopted = stored.is_some();
        let mut record = stored.unwrap_or_else(|| session::OrchestratorSession::new(&cwd_string));
        let key = record.key();
        std::fs::create_dir_all(paths.orchestrator_dir(&key))?;
        if record.launch.fresh.unwrap_or(false) {
            // only --fresh throws the conversation away
            record.session_id = None;
        }
        record.pid = Some(i32::try_from(std::process::id()).unwrap_or(1));
        // The pid's own start time, for the orphan reaper: a pid recycled
        // onto a later process must never be reaped as this monitor.
        record.pid_started_at = crate::orch::health::process_started_at(record.pid.unwrap_or(0));
        if !adopted {
            // a monitor started on no session writes its row once, here;
            // later saves only update it, so a removed row stays removed
            session::with_store_mutation(fleet_dir, |store| store.upsert(record.clone()))?;
        }

        // The prompt is read by the claude child, so it lives beside it;
        // render the current override (or the embedded template) fresh.
        let prompt_file = crate::orch::prompt::write_prompt(fleet_dir, &cwd, &key)
            .map_err(|e| anyhow::anyhow!("cannot write the orchestrator prompt: {e:#}"))?;
        let mcp_config_json = crate::orch::mcp_config::fleet_mcp_config(
            &crate::orch::mcp_config::pilotfish_binary()?,
            fleet_dir,
        )?
        .to_string();

        let launch = record.launch.clone();
        let mut state = new_orchestrator_state(&cwd_string);
        state.pid = Some(i32::try_from(std::process::id()).unwrap_or(1));
        state.permission_mode = launch
            .permission_mode
            .clone()
            .unwrap_or_else(|| "default".to_string());
        state.remote_control = launch.remote_control.clone();
        let events_path = paths.orchestrator_events(&key);
        let caps_path = paths.orchestrator_capabilities(&key);

        let monitor = Arc::new(Self {
            fleet_dir: fleet_dir.to_path_buf(),
            cwd,
            key,
            prompt_file: prompt_file.to_string_lossy().into_owned(),
            mcp_config_json,
            pid: i32::try_from(std::process::id()).unwrap_or(1),
            launch_model: launch.model.clone(),
            budget_usd: launch.budget_usd,
            auto_compact_turns: launch.auto_compact_turns.filter(|t| *t > 0),
            paths,
            shared: Mutex::new(Shared {
                state,
                caps: records::read_capabilities(&caps_path),
                record,
                transcript: records::Transcript::new(events_path),
                pending: HashMap::new(),
                dirty: false,
                finished: false,
                restart: None,
                process: None,
                control_offset: 0,
                dir_missing_polls: 0,
                last_heartbeat_written: None,
                turns_since_compact: 0,
                compacting: false,
                shutting_down: false,
            }),
        });
        // The pid is discoverable the moment the monitor boots, like the
        // TypeScript monitor's writeState() before the child starts. The
        // session row itself is not saved until there is something to save
        // (init): a console that wrote the row keeps it, and a monitor booted
        // alone leaves no half-written record behind.
        monitor.flush_state();
        Ok(monitor)
    }

    /// Lock the shared state even if a previous holder panicked.
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Build a claude child for the current session. After a restart the
    /// resume id is the session the previous child was running; `fresh` (the
    /// one-shot launch flag) drops it instead.
    fn new_process(
        &self,
        remote_control: Option<String>,
        fresh: bool,
    ) -> (
        Arc<OrchestratorProcess>,
        tokio::sync::mpsc::UnboundedReceiver<ProcEvent>,
    ) {
        let (resume_id, permission_mode) = {
            let sh = self.shared();
            (
                if fresh {
                    None
                } else {
                    sh.record.session_id.clone()
                },
                sh.state.permission_mode.clone(),
            )
        };
        let mut options = OrchestratorOptions::new(
            self.cwd.clone(),
            self.prompt_file.clone(),
            self.mcp_config_json.clone(),
        );
        options.log_path = Some(self.paths.claude_log(&self.key));
        options.args.model = self.launch_model.clone();
        // after a restart this is the session the previous child was running
        options.args.resume_session_id = resume_id;
        options.args.max_budget_usd = self.budget_usd;
        options.args.permission_mode = Some(permission_mode);
        options.args.remote_control = remote_control;
        OrchestratorProcess::new(options)
    }

    /// Build and start the first child, write the boot state and notice.
    fn start_child(
        self: &Arc<Self>,
        fresh: bool,
    ) -> tokio::sync::mpsc::UnboundedReceiver<ProcEvent> {
        let remote = self.shared().record.launch.remote_control.clone();
        let (proc, rx) = self.new_process(remote, fresh);
        {
            let mut sh = self.shared();
            sh.process = Some(proc.clone());
            sh.state.pid = Some(self.pid);
            sh.dirty = true;
        }
        proc.start();
        self.flush_state();
        let text = {
            let sh = self.shared();
            match (fresh, sh.record.session_id.as_deref()) {
                (true, _) => "· new orchestrator session".to_string(),
                (false, Some(id)) => {
                    let end = id.char_indices().nth(8).map_or(id.len(), |(i, _)| i);
                    format!("· resumed the orchestrator session {}", &id[..end])
                }
                _ => "· orchestrator started".to_string(),
            }
        };
        self.write_notice(text, None);
        rx
    }

    /// Run the monitor until the claude child ends for good.
    ///
    /// # Errors
    ///
    /// Returns an error when the first child cannot be started or the run
    /// loop's own bookkeeping fails.
    pub async fn run(self: &Arc<Self>) -> anyhow::Result<()> {
        // `fresh` is a one-shot launch instruction: consume it so a restarted
        // monitor resumes instead of starting over.
        let fresh = { self.shared().record.launch.fresh.take().unwrap_or(false) };
        // the pid goes on record now, so the session reads as running before
        // claude's first init (which only comes with the first message)
        self.save_record();
        let mut rx = self.start_child(fresh);
        self.spawn_timers();

        while let Some(event) = rx.recv().await {
            match event {
                ProcEvent::Message(msg) => self.on_message(&msg),
                ProcEvent::TextDelta(delta) => self.on_text_delta(&delta),
                ProcEvent::ThinkingDelta(delta) => self.on_thinking_delta(&delta),
                ProcEvent::Init(init) => self.on_init(&init),
                ProcEvent::Commands(commands) => {
                    {
                        let mut sh = self.shared();
                        sh.caps.commands = commands;
                        sh.caps.fetched_at = now_iso();
                    }
                    self.flush_capabilities();
                }
                ProcEvent::PermissionRequest(request) => self.on_permission_request(&request),
                ProcEvent::Result(_) => self.on_result(),
                ProcEvent::Stderr(text) => {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        self.write_notice(trimmed.to_string(), Some(true));
                    }
                }
                ProcEvent::Exit(info) => {
                    {
                        let mut sh = self.shared();
                        let _ = sh.transcript.flush_text();
                    }
                    // A flag change, not the end of the session: the same
                    // conversation is resumed in a new child and the console
                    // sees no exit at all. Once the fleet directory loss (or
                    // an answering stop) has ended the session, there is
                    // nothing to resume into.
                    let restart = { self.shared().restart.take() };
                    if let Some(restart) = restart
                        && !self.shared().shutting_down
                    {
                        rx = self.restart_child(&restart);
                        continue;
                    }
                    self.record_exit(&info);
                    break;
                }
                // A child we could not even spawn (bad binary, missing cwd):
                // process.rs has already marked the child gone, so record the
                // end instead of waiting on a child that never was.
                ProcEvent::Error(message) => {
                    self.write_notice(message, Some(true));
                    self.record_exit(&ExitInfo {
                        code: None,
                        signal: None,
                    });
                    break;
                }
                // Stream events, control responses, our own writes and spawn
                // receipts carry nothing the transcript needs beyond the
                // typed events above.
                _ => {}
            }
        }

        // The child is gone for good: clear the pid and save the session.
        {
            let mut sh = self.shared();
            sh.record.pid = None;
        }
        self.save_record();
        self.flush_state();
        Ok(())
    }

    /// Record the final exit in the state and the transcript, and clear the pid.
    fn record_exit(&self, info: &ExitInfo) {
        {
            let mut sh = self.shared();
            let _ = sh.transcript.flush_text();
            sh.finished = true;
            sh.state.exited = Some(records::ExitedRecord {
                code: info.code,
                signal: info.signal.clone(),
                at: now_iso(),
            });
            sh.state.turn_active = false;
            sh.state.activity = None;
            sh.state.pid = None;
            let _ = sh.transcript.write(&records::OrchestratorEvent::Exit {
                code: info.code,
                signal: info.signal.clone(),
            });
            sh.dirty = true;
        }
        self.flush_state();
    }

    fn on_message(&self, msg: &serde_json::Value) {
        // Deltas are coalesced by the text_delta handler instead.
        if crate::orch::protocol::text_delta_of(msg).is_some() {
            return;
        }
        let interesting = crate::orch::protocol::is_system_init(msg)
            || crate::orch::protocol::is_assistant(msg)
            || crate::orch::protocol::is_user(msg)
            || crate::orch::protocol::is_result(msg)
            || (msg.get("type").and_then(serde_json::Value::as_str) == Some("system")
                && msg.get("subtype").and_then(serde_json::Value::as_str) == Some("api_retry"));
        if !interesting {
            return;
        }
        let mut sh = self.shared();
        // Ordering stays sane: pending deltas land before the message.
        let _ = sh
            .transcript
            .write(&records::OrchestratorEvent::Passthrough(msg.clone()));
    }

    fn on_text_delta(&self, delta: &str) {
        let mut sh = self.shared();
        sh.transcript.stream_text(delta);
        sh.state.last_activity = Some(now_iso());
        sh.dirty = true;
    }

    fn on_thinking_delta(&self, delta: &str) {
        let mut sh = self.shared();
        sh.transcript.stream_thinking(delta);
        sh.state.last_activity = Some(now_iso());
        sh.dirty = true;
    }

    fn on_init(&self, init: &SystemInitMessage) {
        {
            let mut sh = self.shared();
            sh.state.session_id = Some(init.session_id.clone());
            if init.model.is_some() {
                sh.state.model = init.model.clone();
            }
            if init.claude_code_version.is_some() {
                sh.state.claude_version = init.claude_code_version.clone();
            }
            sh.caps.capabilities = init.capabilities.clone();
            sh.caps.mcp_servers = init.mcp_servers.clone();
            sh.caps.tools = init.tools.clone();
            if let Some(model) = sh.state.model.clone()
                && !sh.caps.models.contains(&model)
            {
                sh.caps.models.push(model);
            }
            sh.caps.fetched_at = now_iso();
            sh.record.session_id = Some(init.session_id.clone());
            sh.record.pid = Some(self.pid);
            sh.record.model = sh.state.model.clone();
            sh.record.claude_version = sh.state.claude_version.clone();
            sh.dirty = true;
        }
        self.save_record();
        self.flush_state();
        self.flush_capabilities();
    }

    fn on_permission_request(&self, request: &PermissionRequest) {
        let mut sh = self.shared();
        sh.pending
            .insert(request.request_id.clone(), request.clone());
        let event = records::OrchestratorEvent::PermissionRequest {
            request_id: request.request_id.clone(),
            request: request_value(&request.request),
        };
        let _ = sh.transcript.write(&event);
        sh.dirty = true;
        drop(sh);
        self.flush_state();
    }

    fn on_result(&self) {
        let proc = { self.shared().process.clone() };
        let Some(proc) = proc else { return };
        {
            let mut sh = self.shared();
            let _ = sh.transcript.flush_text();
            sh.state.cost_usd = proc.cost_usd();
            // claude's own `num_turns` is per query — every result of a
            // stream-json session says 1 (verified fact 6) — so the session's
            // count is kept here, one per result
            sh.state.num_turns = sh.state.num_turns.saturating_add(1);
            // the `/compact` turn itself starts the new window at zero: were it
            // counted, a threshold of one would compact after every compaction,
            // forever, spending a turn each time
            if std::mem::take(&mut sh.compacting) {
                sh.turns_since_compact = 0;
            } else {
                sh.turns_since_compact = sh.turns_since_compact.saturating_add(1);
            }
            sh.state.turn_active = false;
            sh.state.activity = None;
            sh.dirty = true;
        }
        self.flush_state();
        // a turn is the only moment either of these is safe: the child is
        // idle, and the transcript has just been flushed
        self.trim_transcript(MAX_EVENT_LINES, MAX_EVENT_LINES / 2);
        self.maybe_compact(&proc);
    }

    /// Cut `events.jsonl` down to its last `keep` lines once it passes
    /// `over`. The gap between the two is what keeps this rare: trimming to
    /// the cap itself would rewrite the file after every turn from then on,
    /// and every rewrite makes the console replay the transcript.
    ///
    /// Held under the shared lock, so none of this monitor's own appends can
    /// land between the read and the rename — and the monitor is the only
    /// writer of this file, `/trim` included, which asks it rather than
    /// rewriting the file from the console.
    fn trim_transcript(&self, over: usize, keep: usize) -> bool {
        let sh = self.shared();
        let trimmed =
            records::trim_events_file(&self.paths.orchestrator_events(&self.key), over, keep);
        drop(sh);
        trimmed
    }

    /// Compact the session's context once it has run far enough since the
    /// last compaction. `/compact` is claude's own command, so this is an
    /// ordinary user turn — which is also why it is only worth doing rarely.
    fn maybe_compact(&self, proc: &Arc<OrchestratorProcess>) {
        let Some(threshold) = self.auto_compact_turns else {
            return;
        };
        let (since, stopping) = {
            let sh = self.shared();
            (
                sh.turns_since_compact,
                sh.shutting_down || sh.restart.is_some(),
            )
        };
        if since < threshold || stopping {
            return;
        }
        // marked before sending: `/compact` is itself a turn, and its result
        // must open the next window rather than count toward it
        {
            let mut sh = self.shared();
            sh.turns_since_compact = 0;
            sh.compacting = true;
        }
        self.write_notice(
            format!("· compacting the session's context after {since} turns"),
            None,
        );
        proc.send("/compact");
    }

    fn write_notice(&self, text: String, error: Option<bool>) {
        let mut sh = self.shared();
        let _ = sh
            .transcript
            .write(&records::OrchestratorEvent::Notice { text, error });
    }

    // -- timers -------------------------------------------------------------

    fn spawn_timers(self: &Arc<Self>) {
        self.spawn_flusher();
        self.spawn_stream_flusher();
        self.spawn_inbox_poller();
        self.spawn_signal_handlers();
    }

    /// The periodic flusher: writes state when dirty, so a burst of changes
    /// lands in one atomic write instead of one per delta.
    fn spawn_flusher(self: &Arc<Self>) {
        let owner = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(FLUSH_MS)).await;
                if owner.shared().finished {
                    break;
                }
                if owner.shared().dirty {
                    owner.flush_state();
                }
            }
        });
    }

    /// Coalesce token deltas into one record per tick, and derive activity
    /// here so it survives a reattach.
    fn spawn_stream_flusher(self: &Arc<Self>) {
        let owner = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(STREAM_FLUSH_MS)).await;
                if owner.shared().finished {
                    break;
                }
                let Some(proc) = owner.shared().process.clone() else {
                    continue;
                };
                // What the console shows as "thinking…" is derived here, so it
                // survives a reattach.
                let next = if proc.turn_active() {
                    proc.activity()
                } else {
                    None
                };
                let changed = {
                    let mut sh = owner.shared();
                    let changed = match (&next, &sh.state.activity) {
                        (Some(a), Some(b)) => a.kind != b.kind || a.label != b.label,
                        (None, None) => false,
                        _ => true,
                    };
                    if changed {
                        sh.state.activity = next.clone();
                        let _ = sh
                            .transcript
                            .write(&records::OrchestratorEvent::Activity { activity: next });
                        sh.dirty = true;
                    }
                    sh.state.turn_active = proc.turn_active();
                    sh.dirty
                };
                if changed {
                    owner.flush_state();
                }
                {
                    let mut sh = owner.shared();
                    let _ = sh.transcript.flush_text();
                }
            }
        });
    }

    fn spawn_inbox_poller(self: &Arc<Self>) {
        let owner = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(CONTROL_POLL_MS)).await;
                if owner.shared().finished {
                    break;
                }
                // The inbox poll doubles as the watch on the fleet directory:
                // once it has been gone for consecutive polls, no console can
                // ever reach this orchestrator again, and polling on would
                // just add another orphaned monitor holding a claude child.
                if owner.fleet_dir_gone() {
                    owner.shutdown_for_missing_dir().await;
                    break;
                }
                // The same tick keeps the session's heartbeat alive, so a
                // wedged monitor shows up as a stale timestamp instead of
                // being invisible.
                owner.heartbeat_if_due();
                owner.poll_inbox().await;
            }
        });
    }

    /// Stamp the session's heartbeat at most once per [`HEARTBEAT_WRITE_MS`].
    /// Best effort: fleet.json is the console's store, and nothing here may
    /// fail the poll loop over it.
    fn heartbeat_if_due(&self) {
        let now = crate::util::now_ms();
        {
            let mut sh = self.shared();
            if now - sh.last_heartbeat_written.unwrap_or(0) < HEARTBEAT_WRITE_MS {
                return;
            }
            sh.last_heartbeat_written = Some(now);
        }
        let _ = session::touch_heartbeat(&self.fleet_dir, self.key.uuid);
    }

    /// The poll tick's liveness check on the fleet directory: true only once
    /// it (or the monitor's own `orchestrator/` inside it) has been missing
    /// for [`MISSING_DIR_POLLS`] consecutive polls. A present directory
    /// resets the count, so a transient `NotFound` under load never trips it.
    fn fleet_dir_gone(&self) -> bool {
        let missing = dir_is_missing(&self.fleet_dir)
            || dir_is_missing(&self.paths.orchestrator_dir(&self.key));
        let mut sh = self.shared();
        if missing {
            sh.dir_missing_polls += 1;
        } else {
            sh.dir_missing_polls = 0;
        }
        missing && sh.dir_missing_polls >= MISSING_DIR_POLLS
    }

    /// The fleet directory vanished: end the claude child exactly as a `stop`
    /// command would, leave the reason behind, and let the run loop wind down
    /// through the same exit path a stop takes.
    async fn shutdown_for_missing_dir(&self) {
        {
            let mut sh = self.shared();
            sh.shutting_down = true;
            sh.restart = None;
        }
        let proc = { self.shared().process.clone() };
        self.write_notice(
            "· the fleet directory is gone; shutting down".into(),
            Some(true),
        );
        // The spawners point this monitor's stderr at `orchestrator/
        // claude.log`, and that open handle survives the directory's removal
        // — so this line is the reason in that log, where the transcript and
        // the state file cannot go: they live under what was deleted.
        let mut stderr = std::io::stderr();
        let _ = writeln!(
            stderr,
            "[{}] [monitor] the fleet directory {} is gone; stopping claude and exiting",
            now_iso(),
            self.fleet_dir.display()
        );
        if let Some(proc) = proc {
            proc.stop().await;
        }
    }

    fn spawn_signal_handlers(self: &Arc<Self>) {
        use tokio::signal::unix::{SignalKind, signal};
        for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
            let owner = self.clone();
            tokio::spawn(async move {
                let Ok(mut signals) = signal(kind) else {
                    return;
                };
                while signals.recv().await.is_some() {
                    if owner.shared().finished {
                        break;
                    }
                    // never hold the guard across the await
                    let proc = owner.shared().process.clone();
                    if let Some(proc) = proc {
                        proc.stop_now().await;
                    }
                }
            });
        }
    }

    // -- inbox --------------------------------------------------------------

    async fn poll_inbox(&self) {
        let lines = {
            let mut sh = self.shared();
            let (lines, offset) =
                read_new_lines(&self.paths.orchestrator_inbox(&self.key), sh.control_offset);
            sh.control_offset = offset;
            lines
        };
        for line in lines {
            let Some(envelope) = Envelope::parse_line(&line) else {
                continue;
            };
            let Some(command) = decode_command(&envelope) else {
                continue;
            };
            self.handle_command(&command).await;
        }
    }

    async fn handle_command(&self, command: &OrchestratorCommand) {
        let Some(proc) = ({ self.shared().process.clone() }) else {
            return;
        };
        match command {
            OrchestratorCommand::User { text } => {
                {
                    let mut sh = self.shared();
                    sh.state.turn_active = true;
                    sh.state.last_activity = Some(now_iso());
                    sh.state.activity = Some(Activity::starting(ActivityKind::Thinking, None));
                    let activity = sh.state.activity.clone();
                    let _ = sh
                        .transcript
                        .write(&records::OrchestratorEvent::Activity { activity });
                    sh.dirty = true;
                }
                self.flush_state();
                proc.send(text);
            }
            OrchestratorCommand::RefreshCapabilities => {
                OrchestratorProcess::request_commands(&proc);
            }
            OrchestratorCommand::TrimTranscript => {
                let trimmed = self.trim_transcript(TRIM_TO_LINES, TRIM_TO_LINES);
                if !trimmed {
                    self.write_notice("· the transcript is already short".to_string(), None);
                }
            }
            OrchestratorCommand::Permission {
                request_id,
                decision,
            } => {
                // The pending map is the source of truth; the state file's
                // list is derived from it on every flush, so removing from
                // anything else would be clobbered on the next flush.
                let Some(pending) = ({
                    let mut sh = self.shared();
                    sh.pending.remove(request_id.as_str())
                }) else {
                    return;
                };
                match &decision {
                    PermissionDecisionRecord::Allow {
                        updated_permissions,
                    } => {
                        proc.allow(request_id, updated_permissions.as_deref());
                    }
                    PermissionDecisionRecord::Deny { message } => {
                        proc.deny(request_id, message);
                    }
                    PermissionDecisionRecord::Answer { answers } => {
                        proc.answer_question(request_id, answers.clone());
                    }
                }
                {
                    let mut sh = self.shared();
                    let _ = sh
                        .transcript
                        .write(&records::OrchestratorEvent::PermissionResolved {
                            request_id: request_id.clone(),
                            how: decision_how(decision).to_string(),
                        });
                    sh.dirty = true;
                }
                let _ = pending;
                self.flush_state();
            }
            OrchestratorCommand::Interrupt => {
                let _ = proc.interrupt(true).await;
                self.write_notice("· interrupt requested".into(), None);
            }
            OrchestratorCommand::PermissionMode { mode } => {
                let outcome = proc.set_permission_mode(mode).await;
                if outcome.as_ref().and_then(ControlOutcome::success).is_none() {
                    self.write_notice(
                        format!("! claude refused the permission mode {mode}"),
                        Some(true),
                    );
                    return;
                }
                {
                    let mut sh = self.shared();
                    sh.state.permission_mode = mode.clone();
                    sh.record.launch.permission_mode = Some(mode.clone());
                    sh.dirty = true;
                }
                self.save_record();
                self.write_notice(format!("· permission mode → {mode}"), None);
                self.flush_state();
            }
            OrchestratorCommand::Effort { level } => {
                // a settings merge, not a message: the conversation is left alone
                let outcome = proc
                    .apply_flag_settings(serde_json::json!({ "effort": level }))
                    .await;
                if outcome.as_ref().and_then(ControlOutcome::success).is_none() {
                    // older CLIs may not know the setting; fall back to the slash command
                    self.write_notice(
                        "· effort set through /effort (this claude has no settings merge)".into(),
                        None,
                    );
                    proc.send(&format!("/effort {level}"));
                }
                {
                    let mut sh = self.shared();
                    sh.state.effort = Some(level.clone());
                    sh.dirty = true;
                }
                self.flush_state();
            }
            OrchestratorCommand::Model { name } => {
                let outcome = proc.set_model(name).await;
                match outcome {
                    Some(ControlOutcome::Success(_)) => {
                        {
                            let mut sh = self.shared();
                            sh.state.model = Some(name.clone());
                            sh.record.model = Some(name.clone());
                            sh.dirty = true;
                        }
                        self.save_record();
                        self.write_notice(format!("· model → {name}"), None);
                        self.flush_state();
                    }
                    Some(ControlOutcome::Error(text)) => {
                        // claude validates model names itself; its message is
                        // better than anything we would invent. The model is
                        // left unchanged.
                        self.write_notice(text, Some(true));
                    }
                    None => {
                        self.write_notice(
                            "! could not change the model: claude did not answer".into(),
                            Some(true),
                        );
                    }
                }
            }
            OrchestratorCommand::RemoteControl { name } => {
                let current = self.shared().state.remote_control.clone();
                if current == *name {
                    self.write_notice("· Remote Control is already on".into(), None);
                    return;
                }
                self.write_notice("· reconnecting claude with Remote Control…".into(), None);
                {
                    let mut sh = self.shared();
                    sh.restart = Some(Restart {
                        remote_control: name.clone(),
                    });
                }
                // the exit handler brings the session straight back up
                proc.stop().await;
            }
            OrchestratorCommand::Stop => {
                self.write_notice("· shutting down".into(), None);
                {
                    let mut sh = self.shared();
                    sh.restart = None;
                    // nothing may start a turn after this — a compaction
                    // triggered by the result of the turn being stopped included
                    sh.shutting_down = true;
                }
                proc.stop().await;
            }
        }
    }

    /// Replace the claude child for a flag change: the session is resumed in
    /// place and no console ever sees an exit.
    fn restart_child(&self, restart: &Restart) -> mpsc::UnboundedReceiver<ProcEvent> {
        {
            let mut sh = self.shared();
            sh.state.remote_control = restart.remote_control.clone();
            sh.record.launch.remote_control = restart.remote_control.clone();
            sh.state.turn_active = false;
            sh.state.activity = None;
            sh.dirty = true;
        }
        self.save_record();
        let (proc, rx) = self.new_process(restart.remote_control.clone(), false);
        {
            let mut sh = self.shared();
            sh.process = Some(proc.clone());
        }
        proc.start();
        let text = match restart.remote_control.as_deref() {
            None => "· Remote Control is off; the session was resumed without it".to_string(),
            Some("") => "· Remote Control is on; the session was resumed under it".to_string(),
            Some(name) => {
                format!("· Remote Control is on as \"{name}\"; the session was resumed under it")
            }
        };
        self.write_notice(text, None);
        self.flush_state();
        rx
    }

    // -- persistence ----------------------------------------------------------

    /// Write the state file. Flushes are serialised under the one mutex (the
    /// Rust equivalent of the TypeScript `writeChain`), and failures mark the
    /// state dirty again so the periodic flush retries.
    fn flush_state(&self) {
        let mut sh = self.shared();
        sh.dirty = false;
        sh.state.pending_requests = records::sorted_pending(&sh.pending);
        if atomic_write_json(&self.paths.orchestrator_state(&self.key), &sh.state).is_err() {
            sh.dirty = true; // the periodic flush will try again
        }
    }

    /// Write `capabilities.json`. Best effort: a lost write only means the
    /// console offers a slightly older command list until the next refresh.
    fn flush_capabilities(&self) {
        let sh = self.shared();
        let _ =
            records::write_capabilities(&self.paths.orchestrator_capabilities(&self.key), &sh.caps);
    }

    /// Persist what the monitor owns of its session row (claude's session
    /// id, the pids, the model, the launch flags) into a fresh, locked read
    /// of `fleet.json`, so N monitors sharing the store never clobber each
    /// other's rows. Everything else is the console's and stays as it is on
    /// disk: the name (a rename), recency, the watcher's cursors, and the
    /// heartbeat, which is stamped straight into the store. A row that is
    /// gone was removed, and stays gone: the monitor never recreates it.
    fn save_record(&self) {
        let record = self.shared().record.clone();
        let _ = session::with_store_mutation(&self.fleet_dir, |store| {
            let Some(row) = store.sessions.get_mut(&record.uuid) else {
                return;
            };
            row.session_id = record.session_id;
            row.pid = record.pid;
            row.pid_started_at = record.pid_started_at;
            row.model = record.model;
            row.claude_version = record.claude_version;
            row.launch = record.launch;
        });
    }
}

/// A metadata miss with `NotFound` is the only thing that counts as the
/// directory being gone; any other error (a permission blip, say) reads as
/// present, so an unrelated IO hiccup never kills a live session.
fn dir_is_missing(path: &Path) -> bool {
    matches!(
        std::fs::metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

/// How a console answered, as the transcript's `how` field spells it.
const fn decision_how(decision: &PermissionDecisionRecord) -> &'static str {
    match decision {
        PermissionDecisionRecord::Allow { .. } => "allow",
        PermissionDecisionRecord::Deny { .. } => "deny",
        PermissionDecisionRecord::Answer { .. } => "answer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_spell_how() {
        assert_eq!(
            decision_how(&PermissionDecisionRecord::Allow {
                updated_permissions: None,
            }),
            "allow"
        );
        assert_eq!(
            decision_how(&PermissionDecisionRecord::Deny {
                message: "no".into(),
            }),
            "deny"
        );
        assert_eq!(
            decision_how(&PermissionDecisionRecord::Answer {
                answers: serde_json::json!({"q": "a"}),
            }),
            "answer"
        );
    }

    #[test]
    fn boot_session_resolution() {
        {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = tmp.path().join(".pilotfish");
            std::fs::create_dir_all(&fleet).unwrap();
            let mut store = session::FleetSessions::new();
            let mut record = session::OrchestratorSession::new("/repo");
            record.session_id = Some("sess-boot123".into());
            record.launch = session::LaunchOptions {
                model: Some("fable".into()),
                budget_usd: Some(5.0),
                permission_mode: Some("acceptEdits".into()),
                remote_control: Some(String::new()),
                fresh: Some(true),
                auto_compact_turns: None,
            };
            let key = record.key();
            store.upsert(record);
            session::save(&fleet, &mut store).unwrap();

            let monitor = Monitor::boot(&fleet, None).unwrap();
            let sh = monitor.shared();
            assert_eq!(sh.record.session_id, None, "--fresh drops the session id");
            assert_eq!(sh.record.uuid, key.uuid, "the monitor serves the session");
            assert_eq!(sh.state.permission_mode, "acceptEdits");
            assert_eq!(sh.state.remote_control, Some(String::new()));
            assert_eq!(monitor.launch_model.as_deref(), Some("fable"));
            assert_eq!(monitor.budget_usd, Some(5.0));
            assert_eq!(sh.state.pid, Some(monitor.pid));
            assert_eq!(sh.state.pending_requests, Vec::new());
            drop(sh);
            // The boot wrote the durable files a console reads back, in the
            // session's own directory.
            assert!(monitor.paths.orchestrator_prompt(&monitor.key).is_file());
            assert!(load_orchestrator_state(&fleet, &key).is_some());
        }
        {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = tmp.path().join(".pilotfish");
            std::fs::create_dir_all(&fleet).unwrap();
            let monitor = Monitor::boot(&fleet, None).unwrap();
            let sh = monitor.shared();
            assert_eq!(sh.state.cwd, tmp.path().to_string_lossy());
            assert_eq!(sh.state.permission_mode, "default");
            assert_eq!(sh.state.remote_control, None);
        }
        {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = tmp.path().join(".pilotfish");
            std::fs::create_dir_all(&fleet).unwrap();
            let mut store = session::FleetSessions::new();
            let mut wanted = session::OrchestratorSession::new("/repo-a");
            wanted.alias = Some("wanted".into());
            let wanted_uuid = wanted.uuid;
            let mut other = session::OrchestratorSession::new("/repo-b");
            other.alias = Some("other".into());
            other.last_used_at = "2099-01-01T00:00:00.000Z".into(); // most recent
            store.upsert(wanted);
            store.upsert(other);
            session::save(&fleet, &mut store).unwrap();

            let monitor = Monitor::boot(&fleet, Some(wanted_uuid)).unwrap();
            let sh = monitor.shared();
            assert_eq!(sh.record.uuid, wanted_uuid, "serves the named session");
            assert_eq!(sh.record.alias.as_deref(), Some("wanted"));
            assert_eq!(sh.record.cwd, "/repo-a");
            assert_eq!(sh.record.pid, Some(monitor.pid));
            assert!(
                sh.record.pid_started_at.is_some(),
                "the boot records its own start time for the reaper"
            );
            drop(sh);
            assert!(
                monitor.key.dir_name().starts_with("wanted-")
                    && monitor
                        .key
                        .dir_name()
                        .ends_with(&crate::util::short_uuid(&wanted_uuid)),
                "{}",
                monitor.key.dir_name()
            );
            // The monitor's state lands in the named session's directory, not
            // the most recent one's.
            assert!(monitor.paths.orchestrator_state(&monitor.key).is_file());
            assert!(
                !monitor
                    .paths
                    .orchestrator_state(&session::OrchestratorSession::new("/x").key())
                    .is_file()
            );
            // The monitor saves only its own fields: a rename made while it
            // runs survives its next save, and a removed row stays removed.
            session::rename_session(&fleet, wanted_uuid, "renamed").unwrap();
            monitor.save_record();
            let row = session::session_by_key(&fleet, &wanted_uuid.to_string()).unwrap();
            assert_eq!(row.alias.as_deref(), Some("renamed"));
            assert_eq!(row.pid, Some(monitor.pid));
            session::with_store_mutation(&fleet, |store| store.sessions.remove(&wanted_uuid))
                .unwrap();
            monitor.save_record();
            assert!(session::session_by_key(&fleet, &wanted_uuid.to_string()).is_none());
        }
        {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = tmp.path().join(".pilotfish");
            std::fs::create_dir_all(&fleet).unwrap();
            let err = match Monitor::boot(&fleet, Some(uuid::Uuid::new_v4())) {
                Ok(_) => panic!("an unknown session must not boot"),
                Err(err) => err,
            };
            assert!(err.to_string().contains("no session"), "{err}");
        }
    }

    #[test]
    fn missing_dir_trips() {
        {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = tmp.path().join(".pilotfish");
            std::fs::create_dir_all(&fleet).unwrap();
            let monitor = Monitor::boot(&fleet, None).unwrap();
            assert!(!monitor.fleet_dir_gone(), "a present fleet is not gone");

            std::fs::remove_dir_all(&fleet).unwrap();
            assert!(!monitor.fleet_dir_gone(), "one missing poll is tolerated");
            assert!(monitor.fleet_dir_gone(), "the second consecutive one trips");

            // restored (or moved back): the count starts over
            std::fs::create_dir_all(monitor.paths.orchestrator_dir(&monitor.key)).unwrap();
            assert!(!monitor.fleet_dir_gone());
        }
        {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = tmp.path().join(".pilotfish");
            std::fs::create_dir_all(&fleet).unwrap();
            let monitor = Monitor::boot(&fleet, None).unwrap();
            std::fs::remove_dir_all(monitor.paths.orchestrator_dir(&monitor.key)).unwrap();
            assert!(!monitor.fleet_dir_gone(), "one missing poll is tolerated");
            assert!(monitor.fleet_dir_gone());
        }
    }
}
