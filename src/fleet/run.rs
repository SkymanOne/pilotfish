//! Run state: the durable facts about one worker, plus the status that is
//! *derived* from them (never stored) and name/id resolution.
//!
//! Ported from the TypeScript `src/state.ts`, renamed to live in
//! `runs/<id>/run.json`. Serde is deliberately tolerant: unknown fields are
//! ignored and missing fields fall back to defaults, so a state file written
//! by a newer version cannot crash an older reader.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fleet::envelope::{Party, legacy_worker_uuid};
use crate::paths::FleetPaths;
use crate::util::{atomic_write_json, now_iso, parse_ts_ms, sanitize_name};

/// The durable status stored in `run.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Spawned, its monitor has not reported a live pid yet.
    #[default]
    Starting,
    /// The worker process is alive and working.
    Running,
    /// The worker finished its brief and reported back.
    Settled,
    /// Aborted (by `stop` or a failure to start).
    Stopped,
    /// The worker ended with an error.
    Error,
    /// The monitor is gone without a terminal report.
    Dead,
    /// Cleaned up: worktree and branch removed, row kept for the record.
    Archived,
}

impl RunStatus {
    /// Terminal states: `wait` stops polling once one is reached.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Settled | Self::Stopped | Self::Error | Self::Dead | Self::Archived
        )
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Settled => "settled",
            Self::Stopped => "stopped",
            Self::Error => "error",
            Self::Dead => "dead",
            Self::Archived => "archived",
        };
        f.write_str(name)
    }
}

/// What observers show: the durable status, plus `blocked` for a running
/// worker waiting on a `fleet_ask` answer. Never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedView {
    Starting,
    Running,
    Blocked,
    Settled,
    Stopped,
    Error,
    Dead,
    Archived,
}

impl From<RunStatus> for DerivedView {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Starting => Self::Starting,
            RunStatus::Running => Self::Running,
            RunStatus::Settled => Self::Settled,
            RunStatus::Stopped => Self::Stopped,
            RunStatus::Error => Self::Error,
            RunStatus::Dead => Self::Dead,
            RunStatus::Archived => Self::Archived,
        }
    }
}

impl std::fmt::Display for DerivedView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Settled => "settled",
            Self::Stopped => "stopped",
            Self::Error => "error",
            Self::Dead => "dead",
            Self::Archived => "archived",
        };
        f.write_str(name)
    }
}

/// One steering note: who sent it, when, and what it said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringEntry {
    pub source: String,
    pub ts: String,
    pub message: String,
}

/// What the worker is doing right now: reasoning, writing, or in a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerActivity {
    Thinking,
    Text,
    Tool,
}

/// One entry from pi's `get_commands`: an extension command, prompt template
/// or skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCommand {
    pub name: String,
    pub description: String,
    pub source: String,
}

/// A `fleet_ask` the worker is blocked on until an `answer` lands in its inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    #[serde(default)]
    pub context: Option<String>,
    pub asked_at: String,
}

/// A pi extension dialog (`extension_ui_request`) awaiting an answer, shaped
/// like [`PendingQuestion`] so the console renders both the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDialog {
    pub id: String,
    /// The dialog method: `select`, `confirm`, `input` or `editor`.
    pub method: String,
    pub question: String,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    #[serde(default)]
    pub context: Option<String>,
    pub asked_at: String,
}

/// A model pi has configured (from `get_available_models`), slimmed to what
/// the console needs to offer and switch models and what routing needs to
/// choose between them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerModel {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// The thinking levels this model has. Empty means it has none pi can
    /// set, or pi did not say — either way there is no level to ask for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

impl WorkerModel {
    /// A model known only by provider and id.
    #[must_use]
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            id: id.into(),
            name: None,
            thinking_levels: Vec::new(),
            context_window: None,
            cost: None,
        }
    }

    /// `provider:id`, the form that names one model unambiguously — most ids
    /// are served by more than one provider.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, self.id)
    }
}

/// What a model costs, in dollars per million tokens, as pi reports it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
}

/// The fleet-level pi catalogue: the models and commands pi offers. These
/// describe the pi installation, not the run — every run used to carry a
/// byte-identical copy in `run.json`, which is what let a single `run.json`
/// reach 128 KB across 40 runs. They now live once in `<fleet>/pi-cache.json`,
/// written by a worker monitor at boot and merged back into state by
/// [`load_state`], so the console's model switcher and command lists see the
/// same data they always did. Serde-tolerant like everything on disk: unknown
/// fields are ignored and missing ones default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PiCache {
    /// When pi last answered, RFC3339. The console shows the age and asks a
    /// live worker to refresh a stale catalogue rather than trusting a copy
    /// taken at some earlier worker's boot.
    pub fetched_at: String,
    /// The models pi has configured, for the console's model switcher.
    pub available_models: Vec<WorkerModel>,
    /// Commands, skills and prompt templates this worker offers
    /// (pi's `get_commands`).
    pub commands: Vec<WorkerCommand>,
}

/// The run's durable facts, stored as `runs/<id>/run.json`.
///
/// Field names stay camelCase on disk to match the JSON the fleet tooling and
/// docs already speak (`activeModel`, `pendingQuestion`, `commands`, …).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunState {
    pub id: String,
    pub name: String,
    /// The run's UUID identity — what `Party::Worker` addresses and what
    /// `orchestratorId` ownership references. `Uuid::nil()` only in state
    /// files written before the field existed (legacy runs; they keep
    /// resolving through their id and alias). Explicitly nil, never
    /// `Uuid::default()`: under the v4 feature that is a *random* uuid, so
    /// a legacy file would read as a fresh identity every load.
    #[serde(default = "crate::util::nil_uuid")]
    pub uuid: Uuid,
    /// The owning orchestrator session; `None` in state files written before
    /// the field existed (legacy/unowned runs).
    #[serde(default)]
    pub orchestrator_id: Option<Uuid>,
    pub status: RunStatus,
    pub cwd: String,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    /// Commit the worker branch was cut from (resolved at spawn); `None`
    /// without a worktree.
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub session_arg: Option<String>,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub append_system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Option<String>,
    #[serde(default)]
    pub exclude_tools: Option<String>,
    #[serde(default)]
    pub task_brief: String,
    /// What the brief was routed to, when it was routed at all. Recorded so
    /// the choice of model, thinking level and worktree can be read back
    /// rather than guessed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<crate::route::Routing>,
    pub fleet_dir: String,
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub is_git: bool,
    #[serde(default)]
    pub pid: Option<i32>,
    pub created_at: String,
    #[serde(default)]
    pub settled_at: Option<String>,
    #[serde(default)]
    pub last_tool: Option<String>,
    #[serde(default)]
    pub last_activity: Option<String>,
    #[serde(default)]
    pub last_assistant_text: Option<String>,
    #[serde(default)]
    pub steer_count: u32,
    #[serde(default)]
    pub steering_log: Vec<SteeringEntry>,
    #[serde(default)]
    pub error: Option<String>,
    /// Set while the worker waits in `fleet_ask`; absent in state files
    /// written before this field existed.
    #[serde(default)]
    pub pending_question: Option<PendingQuestion>,
    /// Set while the worker is blocked on a pi extension dialog; rendered
    /// like [`Self::pending_question`].
    #[serde(default)]
    pub pending_dialog: Option<PendingDialog>,
    /// The models pi has configured — a load-time view drawn from the
    /// fleet-level pi cache. Never serialized: the on-disk `run.json` does
    /// not carry the catalogue (legacy files that still do load fine and
    /// are simply never written again).
    #[serde(default, skip_serializing)]
    pub available_models: Vec<WorkerModel>,
    /// The model pi actually resolved (from its `get_state`), as opposed to
    /// the `--model` pattern asked for.
    #[serde(default)]
    pub active_model: Option<String>,
    #[serde(default)]
    pub active_provider: Option<String>,
    /// Commands, skills and prompt templates this worker offers — the same
    /// load-time fleet-cache view as [`Self::available_models`], equally
    /// never serialized.
    #[serde(default, skip_serializing)]
    pub commands: Vec<WorkerCommand>,
    /// The reasoning level pi is running at, as it reports it.
    #[serde(default)]
    pub thinking_level: Option<String>,
    /// The levels the worker's model actually has, from pi's
    /// `thinkingLevelMap`. Empty means pi has not told us yet, which reads
    /// as "every level" rather than "none" — a model's map is per-model, so
    /// this changes with `/model`.
    #[serde(default)]
    pub available_thinking_levels: Vec<String>,
    /// What the worker is doing right now.
    #[serde(default)]
    pub activity: Option<WorkerActivity>,
    /// Last `fleet_progress` message.
    #[serde(default)]
    pub last_progress: Option<String>,
}

impl RunState {
    /// A fresh state for a run that has not booted its monitor yet.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fleet_dir: &str,
        run_id: &str,
        name: &str,
        cwd: &str,
        task_brief: &str,
        worktree: Option<String>,
        branch: Option<String>,
        base: Option<String>,
        model: Option<String>,
        provider: Option<String>,
        thinking: Option<String>,
        session_arg: Option<String>,
        skill: Option<String>,
        append_system_prompt: Option<String>,
        tools: Option<String>,
        exclude_tools: Option<String>,
    ) -> Self {
        Self {
            id: run_id.to_string(),
            name: name.to_string(),
            // A fresh identity; the caller deriving the directory name from
            // a specific uuid overwrites it right after construction.
            uuid: Uuid::new_v4(),
            orchestrator_id: None,
            routing: None,
            status: RunStatus::Starting,
            cwd: cwd.to_string(),
            worktree,
            branch,
            base,
            base_commit: None,
            model,
            provider,
            thinking,
            session_arg,
            skill,
            append_system_prompt,
            tools,
            exclude_tools,
            task_brief: task_brief.to_string(),
            fleet_dir: fleet_dir.to_string(),
            repo_root: None,
            is_git: false,
            pid: None,
            created_at: now_iso(),
            settled_at: None,
            last_tool: None,
            last_activity: None,
            last_assistant_text: None,
            steer_count: 0,
            steering_log: Vec::new(),
            error: None,
            pending_question: None,
            pending_dialog: None,
            available_models: Vec::new(),
            active_model: None,
            active_provider: None,
            commands: Vec::new(),
            thinking_level: None,
            available_thinking_levels: Vec::new(),
            activity: None,
            last_progress: None,
        }
    }

    /// What the worker's changes are measured against: the commit its
    /// worktree was cut from, else the ref it was cut from, else `HEAD`.
    #[must_use]
    pub fn diff_base(&self) -> &str {
        self.base_commit
            .as_deref()
            .or(self.base.as_deref())
            .unwrap_or("HEAD")
    }

    /// What to show as the run's model: what pi resolved, else the requested
    /// pattern.
    #[must_use]
    pub fn model_label(&self) -> Option<&str> {
        self.active_model.as_deref().or(self.model.as_deref())
    }
}

/// `runs/<id>/run.json` for one run.
pub fn run_json_path(run_dir: &Path) -> PathBuf {
    run_dir.join("run.json")
}

/// `<fleet>/pi-cache.json` — the fleet-level pi catalogue ([`PiCache`]).
#[must_use]
pub fn pi_cache_json_path(fleet_dir: &Path) -> PathBuf {
    fleet_dir.join(crate::paths::PI_CACHE_FILE)
}

/// Read the fleet-level pi catalogue. Tolerant on purpose: a missing,
/// unreadable or unparsable cache reads as `None`, so callers degrade to an
/// empty model/command list instead of erroring. A write is atomic, so a
/// half-written file is never observed.
#[must_use]
pub fn read_pi_cache(fleet_dir: &Path) -> Option<PiCache> {
    let raw = std::fs::read_to_string(pi_cache_json_path(fleet_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write the fleet-level pi catalogue atomically (tmp + fsync + rename, like
/// [`save_state`]). Best-effort at the call sites: the cache is derived data
/// a monitor rewrites at every boot, so a lost write just means the console
/// shows an empty catalogue until the next boot.
///
/// # Errors
///
/// Returns `std::io::Error` when serialization or the atomic rename fails.
pub fn write_pi_cache(fleet_dir: &Path, cache: &PiCache) -> std::io::Result<()> {
    let stamped = PiCache {
        fetched_at: crate::util::now_iso(),
        ..cache.clone()
    };
    atomic_write_json(&pi_cache_json_path(fleet_dir), &stamped)
}

/// Change the fleet's pi catalogue under its lock: read, change, write.
/// Monitors and one-shot fetches learn different fields at different times;
/// an unlocked read-modify-write let one writer's stale copy wipe the
/// models another had just written. The lock is a sidecar, because the
/// cache itself is replaced by rename on every write.
///
/// # Errors
///
/// When the lock or the cache cannot be written.
pub fn update_pi_cache(fleet_dir: &Path, update: impl FnOnce(&mut PiCache)) -> std::io::Result<()> {
    std::fs::create_dir_all(fleet_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fleet_dir.join(format!("{}.lock", crate::paths::PI_CACHE_FILE)))?;
    lock.lock()?;
    let mut cache = read_pi_cache(fleet_dir).unwrap_or_default();
    update(&mut cache);
    let written = write_pi_cache(fleet_dir, &cache);
    lock.unlock()?;
    written
}

/// The read path for the pi catalogue: the fleet cache when it reads and
/// parses, else empty. Legacy `run.json` values are never trusted and never
/// re-persisted ([`save_state`] strips them); a missing or stale cache
/// degrades to an empty catalogue, never to an error.
fn state_pi_catalogue(run_dir: &Path) -> (Vec<WorkerModel>, Vec<WorkerCommand>) {
    let Some(cache) = run_dir
        .parent()
        .and_then(Path::parent)
        .and_then(read_pi_cache)
    else {
        return (Vec::new(), Vec::new());
    };
    (cache.available_models, cache.commands)
}

/// Read a run's `run.json`, tolerating nothing: a missing or corrupted file
/// is an error with a message naming the directory. The pi catalogue fields
/// come from the fleet-level cache, never from the file itself.
///
/// # Errors
///
/// Returns `anyhow::Error` when `run.json` is missing or does not parse,
/// with the run directory named in the message.
pub fn load_state(run_dir: &Path) -> anyhow::Result<RunState> {
    let raw = std::fs::read_to_string(run_json_path(run_dir))
        .map_err(|_| anyhow::anyhow!("No readable run.json in {}", run_dir.display()))?;
    let mut state: RunState = serde_json::from_str(&raw)
        .map_err(|_| anyhow::anyhow!("Corrupted run.json in {}", run_dir.display()))?;
    (state.available_models, state.commands) = state_pi_catalogue(run_dir);
    Ok(state)
}

/// Write a run's `run.json` atomically. The pi catalogue fields carry
/// `skip_serializing`, so they can never reappear in `run.json` — a
/// loaded-then-saved state cannot grow a file back to 100+ KB of duplicated
/// catalogue.
///
/// # Errors
///
/// Returns `std::io::Error` when serialization or the atomic rename fails.
pub fn save_state(run_dir: &Path, state: &RunState) -> std::io::Result<()> {
    atomic_write_json(&run_json_path(run_dir), state)
}

/// Is a process with this pid alive? `kill(pid, 0)` semantics: a pid we may
/// signal is alive, and `EPERM` (owned by someone else) counts as alive too.
#[must_use]
pub fn is_alive(pid: Option<i32>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// A freshly spawned run has no pid until its monitor boots; don't call it
/// dead yet.
pub const STARTING_GRACE_MS: i64 = 30_000;

/// The stored status, corrected for reality: a run whose monitor is gone is
/// `dead`, and a run still waiting to boot stays `starting` for
/// [`STARTING_GRACE_MS`] before being called `dead`.
#[must_use]
pub fn derive_status(
    state: &RunState,
    liveness: impl Fn(Option<i32>) -> bool,
    now_ms: i64,
) -> RunStatus {
    if state.status == RunStatus::Starting && state.pid.is_none() {
        let created = parse_ts_ms(&state.created_at).unwrap_or(now_ms);
        return if now_ms - created > STARTING_GRACE_MS {
            RunStatus::Dead
        } else {
            RunStatus::Starting
        };
    }
    if matches!(state.status, RunStatus::Starting | RunStatus::Running) && !liveness(state.pid) {
        return RunStatus::Dead;
    }
    state.status
}

/// [`derive_status`] plus the `blocked` view for a running worker waiting on
/// a `fleet_ask` answer or an extension dialog.
#[must_use]
pub fn derive_view(
    state: &RunState,
    liveness: impl Fn(Option<i32>) -> bool,
    now_ms: i64,
) -> DerivedView {
    let status = derive_status(state, liveness, now_ms);
    if status == RunStatus::Running
        && (state.pending_question.is_some() || state.pending_dialog.is_some())
    {
        return DerivedView::Blocked;
    }
    DerivedView::from(status)
}

/// Note what tool the worker is in and when it last did anything.
pub fn record_tool_activity(state: &mut RunState, tool_name: Option<&str>) {
    if let Some(tool) = tool_name {
        state.last_tool = Some(tool.to_string());
    }
    state.last_activity = Some(now_iso());
}

/// Append a steering note; the log is capped so a chatty console cannot grow
/// `run.json` without bound.
pub fn record_steering(state: &mut RunState, source: &str, ts: &str, message: &str) {
    state.steer_count += 1;
    state.steering_log.push(SteeringEntry {
        source: source.to_string(),
        ts: ts.to_string(),
        message: message.to_string(),
    });
    if state.steering_log.len() > STEERING_LOG_CAP {
        let excess = state.steering_log.len() - STEERING_LOG_CAP;
        state.steering_log.drain(..excess);
    }
}

/// Steering entries kept in `run.json`; older ones are dropped, the count stays.
pub const STEERING_LOG_CAP: usize = 20;

/// pi's reasoning levels, lowest to highest; the last two need a model that
/// supports them.
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A run located by [`find_run`].
#[derive(Debug, Clone)]
pub struct RunRef {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub state: RunState,
}

impl RunRef {
    /// The run's worker party: its uuid, or the stable derived uuid of its
    /// (legacy) run id when the state file predates run uuids.
    #[must_use]
    pub fn worker_party(&self) -> Party {
        if self.state.uuid.is_nil() {
            Party::Worker(legacy_worker_uuid(&self.run_id))
        } else {
            Party::Worker(self.state.uuid)
        }
    }
}

/// One entry of [`list_runs`]: a directory under `runs/` with a readable
/// `run.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: String,
    pub run_dir: PathBuf,
}

/// The run directories under `<fleet>/runs`, newest id first. Unreadable or
/// missing `run.json` files are skipped.
#[must_use]
pub fn list_runs(fleet_dir: &Path) -> Vec<RunSummary> {
    let runs_dir = fleet_dir.join("runs");
    let Ok(entries) = std::fs::read_dir(&runs_dir) else {
        return Vec::new();
    };
    let mut out: Vec<RunSummary> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| run_json_path(p).is_file())
        .map(|p| RunSummary {
            run_id: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            run_dir: p,
        })
        .collect();
    out.sort_by(|a, b| b.run_id.cmp(&a.run_id));
    out
}

/// The non-archived run directories under `<fleet>/runs` whose `run.json`
/// records an owner: newest id first. The unfiltered [`list_runs`] stays for
/// cleanup and diagnostics; this is what a session sees as its own.
#[must_use]
pub fn list_runs_for_owner(fleet_dir: &Path, owner: Uuid) -> Vec<RunSummary> {
    list_runs(fleet_dir)
        .into_iter()
        .filter(|r| {
            load_state(&r.run_dir)
                .ok()
                .and_then(|state| state.orchestrator_id)
                == Some(owner)
        })
        .collect()
}

/// Resolve `<name>` or an id to one run, in strict order:
/// 1. the exact run id (a directory name);
/// 2. the exact run uuid (`state.uuid`);
/// 3. the alias (the run's `name` field) — several live runs sharing an
///    alias is an error naming the candidates, never a silent pick;
/// 4. the legacy `<name>-<14-digit>` directory form, for runs already on
///    disk (state files that predate the uuid scheme).
///
/// Steps 3 and 4 match a sanitized name, so casing and punctuation are
/// ignored; archived runs resolve only when nothing live matches. A name
/// never prefix-matches: `api` never resolves to `api-tests-…`.
///
/// # Errors
///
/// Returns `anyhow::Error` when no run matches `name_or_id` or when several
/// live runs share the alias (the candidates are named).
pub fn find_run(fleet_dir: &Path, name_or_id: &str) -> anyhow::Result<RunRef> {
    let raw = name_or_id.trim();
    let key = sanitize_name(raw);
    let runs: Vec<RunRef> = list_runs(fleet_dir)
        .into_iter()
        .filter_map(|r| {
            load_state(&r.run_dir).ok().map(|state| RunRef {
                run_id: r.run_id,
                run_dir: r.run_dir,
                state,
            })
        })
        .collect();
    let preferred = |candidates: &[RunRef]| {
        candidates
            .iter()
            .find(|c| c.state.status != RunStatus::Archived)
            .or_else(|| candidates.first())
            .cloned()
    };

    // 1. The exact run id (a directory name), sanitized like the rest.
    if let Some(chosen) = preferred(
        &runs
            .iter()
            .filter(|r| r.run_id == key)
            .cloned()
            .collect::<Vec<_>>(),
    ) {
        return Ok(chosen);
    }
    // 2. The exact run uuid, from the raw input (a uuid is not sanitized:
    //    hyphens are part of its syntax).
    if let Ok(uuid) = Uuid::parse_str(raw)
        && let Some(chosen) = preferred(
            &runs
                .iter()
                .filter(|r| r.state.uuid == uuid)
                .cloned()
                .collect::<Vec<_>>(),
        )
    {
        return Ok(chosen);
    }
    // 3. The alias: runs whose `name` field is the key. Several live (not
    //    archived) ones are ambiguous — name them instead of picking.
    let aliased: Vec<RunRef> = runs
        .iter()
        .filter(|r| r.state.name == key)
        .cloned()
        .collect();
    let live = aliased
        .iter()
        .filter(|r| r.state.status != RunStatus::Archived)
        .collect::<Vec<_>>();
    if live.len() > 1 {
        let names = live
            .iter()
            .map(|r| r.run_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "run name \"{key}\" is ambiguous — {} live runs share it: {names}",
            live.len()
        );
    }
    if let Some(chosen) = preferred(&aliased) {
        return Ok(chosen);
    }
    // 4. The legacy `<name>-<14-digit stamp>` form, for runs already on disk.
    let of_name = format!("^{0}-\\d{{14}}$", regex::escape(&key));
    let of_name = regex::Regex::new(&of_name).map_err(|e| anyhow::anyhow!("{e}"))?;
    let legacy: Vec<RunRef> = runs
        .into_iter()
        .filter(|r| of_name.is_match(&r.run_id))
        .collect();
    if let Some(chosen) = preferred(&legacy) {
        return Ok(chosen);
    }
    Err(anyhow::anyhow!(
        "No run found matching \"{name_or_id}\" in {}",
        fleet_dir.join("runs").display()
    ))
}

/// Newest pi session file under `<run_dir>/session`, for `--session` resume
/// hints.
#[must_use]
pub fn find_session_file(run_dir: &Path) -> Option<PathBuf> {
    let dir = run_dir.join("session");
    let newest = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), modified))
        })
        .max_by_key(|(_, modified)| *modified)?;
    Some(newest.0)
}

/// The session file a run can be resumed from: the newest file under its
/// `session/` dir, or — when that is empty, as for a resumed run whose pi
/// keeps writing the resumed file — the recorded `--session` argument when
/// it names an existing file.
#[must_use]
pub fn session_file_or_arg(run_dir: &Path, session_arg: Option<&str>) -> Option<PathBuf> {
    find_session_file(run_dir).or_else(|| {
        session_arg
            .map(Path::new)
            .filter(|p| p.is_file())
            .map(Path::to_path_buf)
    })
}

/// Copy-pasteable command to continue a finished run's session in a new run.
#[must_use]
pub fn resume_hint(state: &RunState, run_dir: &Path) -> String {
    let session = session_file_or_arg(run_dir, state.session_arg.as_deref()).map_or_else(
        || {
            run_dir
                .join("session")
                .join("<session-file>")
                .to_string_lossy()
                .into_owned()
        },
        |p| p.to_string_lossy().into_owned(),
    );
    format!(
        "{} spawn {}-2 --session {session} -- \"<new brief>\"",
        crate::paths::BIN_NAME,
        state.name
    )
}

/// The fleet dir a state file points at, re-materialized as [`FleetPaths`].
#[must_use]
pub fn fleet_paths_of(state: &RunState) -> FleetPaths {
    FleetPaths::new(&state.fleet_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{run_id_for, short_uuid};
    use uuid::Uuid;

    fn fleet_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn base_state(fleet: &Path, run_id: &str) -> RunState {
        RunState::new(
            fleet.to_string_lossy().as_ref(),
            run_id,
            "auth",
            "/tmp/x",
            "b",
            None,
            None,
            Some("HEAD".into()),
            Some("m".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    fn fixed_now() -> i64 {
        parse_ts_ms("2026-08-28T14:15:30Z").unwrap()
    }

    #[test]
    fn run_json_compat() {
        {
            let fleet = fleet_dir("pilotfish-run-");
            let run_dir = fleet.join("runs/auth-20260828141530");
            std::fs::create_dir_all(&run_dir).unwrap();
            let path = run_json_path(&run_dir);
            // A newer writer: extra fields, and known fields omitted.
            std::fs::write(
                &path,
                r#"{
                    "id": "auth-20260828141530",
                    "name": "auth",
                    "status": "running",
                    "cwd": "/tmp/x",
                    "fleetDir": "/tmp/x/.pilotfish",
                    "createdAt": "2026-08-28T14:15:30.000Z",
                    "taskBrief": "b",
                    "someFutureField": {"deep": [1, 2, 3]}
                }"#,
            )
            .unwrap();
            let state = load_state(&run_dir).unwrap();
            assert_eq!(state.status, RunStatus::Running);
            assert_eq!(state.steer_count, 0);
            assert_eq!(state.pending_question, None);
            assert!(!state.is_git);
            assert_eq!(state.commands, Vec::<WorkerCommand>::new());
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let run_dir = fleet.join("runs/auth-20260828141530");
            std::fs::create_dir_all(&run_dir).unwrap();
            std::fs::write(run_json_path(&run_dir), "{oops").unwrap();
            let err = load_state(&run_dir).unwrap_err().to_string();
            assert!(err.contains("Corrupted run.json"), "{err}");
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let run_dir = fleet.join("runs/auth-20260828141530");
            std::fs::create_dir_all(&run_dir).unwrap();
            // A state file written before the cache existed: both catalogue
            // fields present, in the old camelCase spelling.
            std::fs::write(
                run_json_path(&run_dir),
                r#"{
                    "id": "auth-20260828141530",
                    "name": "auth",
                    "status": "running",
                    "cwd": "/tmp/x",
                    "fleetDir": "/tmp/x/.pilotfish",
                    "createdAt": "2026-08-28T14:15:30.000Z",
                    "taskBrief": "b",
                    "availableModels": [{"provider": "anthropic", "id": "claude-opus-5", "name": "Opus"}],
                    "commands": [{"name": "compact-notes", "description": "Summarize the session", "source": "prompt"}]
                }"#,
            )
            .unwrap();
            // Loads without error. Without a fleet cache the catalogue reads
            // empty: the console sources it from pi-cache.json, never from here.
            let state = load_state(&run_dir).unwrap();
            assert_eq!(state.status, RunStatus::Running);
            assert!(state.available_models.is_empty());
            assert!(state.commands.is_empty());
            // A save never writes the catalogue back into run.json.
            save_state(&run_dir, &state).unwrap();
            let raw = std::fs::read_to_string(run_json_path(&run_dir)).unwrap();
            assert!(!raw.contains("availableModels"), "{raw}");
            assert!(!raw.contains("\"commands\""), "{raw}");
        }
    }

    #[test]
    fn catalogue_from_cache() {
        {
            let fleet = fleet_dir("pilotfish-run-");
            let run_dir = fleet.join("runs/auth-20260828141530");
            std::fs::create_dir_all(&run_dir).unwrap();
            save_state(&run_dir, &base_state(&fleet, "auth-20260828141530")).unwrap();
            let cache = PiCache {
                available_models: vec![WorkerModel {
                    provider: "anthropic".into(),
                    id: "claude-opus-5".into(),
                    name: Some("Opus".into()),
                    thinking_levels: Vec::new(),
                    context_window: None,
                    cost: None,
                }],
                commands: vec![WorkerCommand {
                    name: "compact-notes".into(),
                    description: "Summarize the session".into(),
                    source: "prompt".into(),
                }],
                ..PiCache::default()
            };
            write_pi_cache(&fleet, &cache).unwrap();
            let written = read_pi_cache(&fleet).expect("the cache reads back");
            assert_eq!(written.available_models, cache.available_models);
            assert_eq!(written.commands, cache.commands);
            assert!(!written.fetched_at.is_empty(), "the answer is stamped");
            let state = load_state(&run_dir).unwrap();
            assert_eq!(state.available_models, cache.available_models);
            assert_eq!(state.commands, cache.commands);
            // The atomic write leaves no tmp files behind.
            let leftovers: Vec<String> = std::fs::read_dir(&fleet)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains(".tmp"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let run_dir = fleet.join("runs/auth-20260828141530");
            std::fs::create_dir_all(&run_dir).unwrap();
            save_state(&run_dir, &base_state(&fleet, "auth-20260828141530")).unwrap();
            assert_eq!(read_pi_cache(&fleet), None, "no cache file yet");
            let state = load_state(&run_dir).unwrap();
            assert!(state.available_models.is_empty());
            assert!(state.commands.is_empty());
            // A stale (unparsable) cache reads as empty too, never an error.
            std::fs::write(fleet.join(crate::paths::PI_CACHE_FILE), "{oops").unwrap();
            assert_eq!(read_pi_cache(&fleet), None);
            let state = load_state(&run_dir).unwrap();
            assert!(state.available_models.is_empty());
            assert_eq!(state.id, "auth-20260828141530");
        }
        {
            // writers of different fields, side by side, never lose each
            // other's updates: the read-modify-write is locked
            let fleet = fleet_dir("pilotfish-run-cache-race-");
            let writers: Vec<_> = (0..2)
                .map(|which| {
                    let fleet = fleet.clone();
                    std::thread::spawn(move || {
                        for i in 0..40 {
                            update_pi_cache(&fleet, |cache| {
                                if which == 0 {
                                    cache
                                        .available_models
                                        .push(WorkerModel::new("p", format!("m{i}")));
                                } else {
                                    cache.commands.push(WorkerCommand {
                                        name: format!("c{i}"),
                                        description: String::new(),
                                        source: "test".into(),
                                    });
                                }
                            })
                            .unwrap();
                        }
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            let cache = read_pi_cache(&fleet).unwrap();
            assert_eq!(cache.available_models.len(), 40);
            assert_eq!(cache.commands.len(), 40);
        }
    }

    fn write_run(fleet: &Path, run_id: &str, name: &str, status: RunStatus) -> RunState {
        let run_dir = fleet.join("runs").join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut s = RunState::new(
            fleet.to_string_lossy().as_ref(),
            run_id,
            name,
            "/tmp/x",
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
        s.status = status;
        save_state(&run_dir, &s).unwrap();
        s
    }

    #[test]
    fn list_runs_filters() {
        {
            let fleet = fleet_dir("pilotfish-run-");
            write_run(&fleet, "auth-20260828141530", "auth", RunStatus::Running);
            write_run(&fleet, "auth-20260828161530", "auth", RunStatus::Running);
            std::fs::create_dir_all(fleet.join("runs/auth-20260828171530")).unwrap();
            let ids: Vec<String> = list_runs(&fleet).into_iter().map(|r| r.run_id).collect();
            assert_eq!(ids, vec!["auth-20260828161530", "auth-20260828141530"]);
            // No runs directory at all reads as empty.
            assert!(list_runs(&fleet_dir("pilotfish-run-empty-")).is_empty());
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let owner = Uuid::parse_str("6e1c9a86-3b7d-4f5a-9e2c-1b8d4a7f0c3e").unwrap();
            let other = Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap();
            let mut mine = write_run(&fleet, "mine-60828141530", "mine", RunStatus::Running);
            mine.uuid = other;
            mine.orchestrator_id = Some(owner);
            save_state(&fleet.join("runs/mine-60828141530"), &mine).unwrap();
            let mut theirs = write_run(&fleet, "theirs-60828141531", "theirs", RunStatus::Running);
            theirs.orchestrator_id = Some(other);
            save_state(&fleet.join("runs/theirs-60828141531"), &theirs).unwrap();
            // A legacy run without an owner is nobody's.
            write_run(&fleet, "unowned-60828141532", "unowned", RunStatus::Running);
            let owned: Vec<String> = list_runs_for_owner(&fleet, owner)
                .into_iter()
                .map(|r| r.run_id)
                .collect();
            assert_eq!(owned, vec!["mine-60828141530"]);
            // The unfiltered path still lists everything.
            assert_eq!(list_runs(&fleet).len(), 3);
        }
    }

    #[test]
    fn derived_status() {
        {
            let fleet = fleet_dir("pilotfish-run-");
            let mut s = base_state(&fleet, "auth-20260828141530");
            s.status = RunStatus::Running;
            s.pid = Some(1);
            assert_eq!(
                derive_status(&s, |pid| pid == Some(1), fixed_now()),
                RunStatus::Running
            );
            assert_eq!(derive_status(&s, |_| false, fixed_now()), RunStatus::Dead);
            s.status = RunStatus::Settled;
            assert_eq!(
                derive_status(&s, |_| false, fixed_now()),
                RunStatus::Settled
            );
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let s = base_state(&fleet, "auth-20260828141530");
            let created = parse_ts_ms(&s.created_at).unwrap();
            assert_eq!(
                derive_status(&s, |_| false, created + 1000),
                RunStatus::Starting
            );
            assert_eq!(
                derive_status(&s, |_| false, created + STARTING_GRACE_MS + 1),
                RunStatus::Dead
            );
            // A live pid within grace is just starting.
            assert_eq!(
                derive_status(&s, |_| true, created + 1000),
                RunStatus::Starting
            );
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let mut s = base_state(&fleet, "auth-20260828141530");
            s.status = RunStatus::Running;
            s.pid = Some(1);
            assert_eq!(derive_view(&s, |_| true, fixed_now()), DerivedView::Running);
            s.pending_question = Some(PendingQuestion {
                id: "m_1".into(),
                question: "which fixture?".into(),
                options: None,
                context: None,
                asked_at: now_iso(),
            });
            assert_eq!(derive_view(&s, |_| true, fixed_now()), DerivedView::Blocked);
            assert_eq!(derive_view(&s, |_| false, fixed_now()), DerivedView::Dead);
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let mut s = base_state(&fleet, "auth-20260828141530");
            s.status = RunStatus::Running;
            s.pid = Some(1);
            s.pending_dialog = Some(PendingDialog {
                id: "u-1".into(),
                method: "select".into(),
                question: "Pick one".into(),
                options: Some(vec!["a".into()]),
                context: None,
                asked_at: now_iso(),
            });
            assert_eq!(derive_view(&s, |_| true, fixed_now()), DerivedView::Blocked);
            s.pending_dialog = None;
            assert_eq!(derive_view(&s, |_| true, fixed_now()), DerivedView::Running);
            // A state file written before the field existed still loads.
            let run_dir = fleet.join("runs/auth-20260828141530");
            std::fs::create_dir_all(&run_dir).unwrap();
            std::fs::write(
                run_json_path(&run_dir),
                r#"{"id":"auth-20260828141530","name":"auth","status":"running","cwd":"/tmp/x","fleetDir":"/f","createdAt":"2026-08-28T14:15:30.000Z","taskBrief":"b"}"#,
            )
            .unwrap();
            let old = load_state(&run_dir).unwrap();
            assert_eq!(old.pending_dialog, None);
            assert!(old.available_models.is_empty());
        }
        {
            assert!(is_alive(Some(std::process::id() as i32)));
            assert!(!is_alive(None));
            assert!(!is_alive(Some(0)));
            assert!(!is_alive(Some(i32::MAX)));
            assert!(!is_alive(Some(-5)));
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            let mut s = base_state(&fleet, "auth-20260828141530");
            for i in 0..25 {
                record_steering(&mut s, "console", &format!("t{i}"), &format!("m{i}"));
            }
            assert_eq!(s.steer_count, 25);
            assert_eq!(s.steering_log.len(), 20);
            assert_eq!(s.steering_log.last().unwrap().message, "m24");
            assert_eq!(s.steering_log.first().unwrap().message, "m5");
        }
    }

    #[test]
    fn find_run_resolution() {
        {
            let fleet = fleet_dir("pilotfish-run-");
            write_run(&fleet, "auth-20260828141530", "auth", RunStatus::Running);
            write_run(
                &fleet,
                "auth-worker-20260828141531",
                "auth-worker",
                RunStatus::Running,
            );
            assert_eq!(
                find_run(&fleet, "auth").unwrap().run_id,
                "auth-20260828141530"
            );
            assert_eq!(
                find_run(&fleet, "auth-worker").unwrap().run_id,
                "auth-worker-20260828141531"
            );
            // Casing is sanitized away.
            assert_eq!(
                find_run(&fleet, "Auth").unwrap().run_id,
                "auth-20260828141530"
            );
            // Full ids work.
            assert_eq!(
                find_run(&fleet, "auth-worker-20260828141531")
                    .unwrap()
                    .run_id,
                "auth-worker-20260828141531"
            );
            // A prefix of a name resolves to nothing.
            assert!(find_run(&fleet, "auth-work").is_err());
            assert!(find_run(&fleet, "ghost").is_err());
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            write_run(&fleet, "auth-20260828141530", "auth", RunStatus::Running);
            write_run(&fleet, "auth-20260828161530", "auth", RunStatus::Running);
            // Two live runs share the alias: an error naming both candidates,
            // never a silent pick.
            let err = find_run(&fleet, "auth").unwrap_err().to_string();
            assert!(err.contains("ambiguous"), "{err}");
            assert!(err.contains("auth-20260828161530"), "{err}");
            assert!(err.contains("auth-20260828141530"), "{err}");
            // The exact ids resolve regardless.
            assert_eq!(
                find_run(&fleet, "auth-20260828161530").unwrap().run_id,
                "auth-20260828161530"
            );
            // Archive the newest: the alias is unambiguous again, and the
            // remaining live one wins.
            let newest = find_run(&fleet, "auth-20260828161530").unwrap();
            let mut state = newest.state.clone();
            state.status = RunStatus::Archived;
            save_state(&newest.run_dir, &state).unwrap();
            assert_eq!(
                find_run(&fleet, "auth").unwrap().run_id,
                "auth-20260828141530"
            );
            // All archived: still resolvable, explicitly, without ambiguity.
            let older = find_run(&fleet, "auth-20260828141530").unwrap();
            let mut state = older.state.clone();
            state.status = RunStatus::Archived;
            save_state(&older.run_dir, &state).unwrap();
            assert_eq!(
                find_run(&fleet, "auth").unwrap().run_id,
                "auth-20260828161530"
            );
        }
        {
            let fleet = fleet_dir("pilotfish-run-");
            // A run under the new scheme: `<alias>-<short-uuid>`, a recorded
            // uuid, an owner, and state.name as the alias.
            let uuid = Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap();
            let run_id = run_id_for("auth", &uuid);
            assert_eq!(run_id, "auth-f7a8b9c");
            // The id is the alias plus the short uuid; the branch rule shortens
            // the same suffix, so branch names stay valid under the scheme.
            assert_eq!(short_uuid(&uuid), "f7a8b9c");
            assert_eq!(crate::util::short7(&run_id), "f7a8b9c");
            assert_eq!(
                crate::util::branch_for("auth", &run_id),
                "pilotfish/auth-f7a8b9c"
            );
            let mut state = write_run(&fleet, &run_id, "auth", RunStatus::Running);
            state.uuid = uuid;
            state.orchestrator_id =
                Some(Uuid::parse_str("6e1c9a86-3b7d-4f5a-9e2c-1b8d4a7f0c3e").unwrap());
            save_state(&fleet.join("runs").join(&run_id), &state).unwrap();
            // The exact run id (the directory name) resolves.
            assert_eq!(find_run(&fleet, "auth-f7a8b9c").unwrap().run_id, run_id);
            // The exact uuid resolves.
            assert_eq!(
                find_run(&fleet, "9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c")
                    .unwrap()
                    .run_id,
                run_id
            );
            // The alias resolves through the name field.
            assert_eq!(find_run(&fleet, "auth").unwrap().run_id, run_id);
            assert_eq!(
                find_run(&fleet, "AUTH").unwrap().run_id,
                run_id,
                "casing is sanitized away"
            );
            // A uuid that matches nothing is a plain miss.
            assert!(find_run(&fleet, "11111111-1111-4111-8111-111111111111").is_err());
            // A legacy run whose state predates the name field still resolves
            // through its directory form.
            let legacy_dir = fleet.join("runs/db-20260828141530");
            std::fs::create_dir_all(&legacy_dir).unwrap();
            let mut legacy = RunState::new(
                fleet.to_string_lossy().as_ref(),
                "db-20260828141530",
                "",
                "/tmp/x",
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
            legacy.uuid = Uuid::nil();
            legacy.status = RunStatus::Running;
            save_state(&legacy_dir, &legacy).unwrap();
            assert_eq!(
                find_run(&fleet, "db").unwrap().run_id,
                "db-20260828141530",
                "the legacy directory form still resolves"
            );
        }
    }

    #[test]
    fn worker_party_encoding() {
        let fleet = fleet_dir("pilotfish-run-");
        let uuid = Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap();
        let run_id = run_id_for("auth", &uuid);
        let mut state = write_run(&fleet, &run_id, "auth", RunStatus::Running);
        state.uuid = uuid;
        save_state(&fleet.join("runs").join(&run_id), &state).unwrap();
        let run = find_run(&fleet, "auth").unwrap();
        assert_eq!(run.worker_party(), Party::Worker(uuid));
        assert_eq!(run.worker_party().to_string(), format!("worker:{uuid}"));
        // A state predating run uuids gets the stable legacy encoding of its
        // run id — the same party parsing `worker:<run_id>` yields.
        let mut legacy = write_run(&fleet, "old-20260828141530", "old", RunStatus::Running);
        legacy.uuid = Uuid::nil();
        save_state(&fleet.join("runs/old-20260828141530"), &legacy).unwrap();
        let run = find_run(&fleet, "old").unwrap();
        assert_eq!(
            run.worker_party(),
            Party::Worker(crate::fleet::envelope::legacy_worker_uuid(&run.run_id))
        );
        assert_eq!(
            run.worker_party(),
            format!(
                "worker:{}",
                crate::fleet::envelope::legacy_worker_uuid(&run.run_id)
            )
            .parse()
            .unwrap()
        );
    }

    #[test]
    fn resume_hint_newest_session() {
        let fleet = fleet_dir("pilotfish-run-");
        let run_dir = fleet.join("runs/auth-20260828141530");
        std::fs::create_dir_all(run_dir.join("session")).unwrap();
        std::fs::write(run_dir.join("session").join("s1.jsonl"), "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(run_dir.join("session").join("s2.jsonl"), "{}\n").unwrap();
        let s = base_state(&fleet, "auth-20260828141530");
        let hint = resume_hint(&s, &run_dir);
        assert!(
            hint.starts_with("pilotfish spawn auth-2 --session "),
            "{hint}"
        );
        assert!(hint.contains("s2.jsonl"), "{hint}");
        // Without session files, a placeholder stands in.
        let hint2 = resume_hint(&s, &fleet.join("runs/none-20260828141530"));
        assert!(hint2.contains("session/<session-file>"), "{hint2}");
    }

    #[test]
    fn session_file_falls_back_to_arg() {
        let fleet = fleet_dir("pilotfish-run-");
        // A resumed run: its own session/ dir stays empty, and the recorded
        // --session argument (the original run's file) is what fleet_status
        // and resume_hint name.
        let original = fleet.join("other/session/orig.jsonl");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, "{}\n").unwrap();
        let run_dir = fleet.join("runs/auth-20260828141530");
        std::fs::create_dir_all(run_dir.join("session")).unwrap();
        let arg = original.to_string_lossy().into_owned();
        assert_eq!(session_file_or_arg(&run_dir, Some(&arg)), Some(original));
        // An argument that names no file is not a session file.
        assert_eq!(
            session_file_or_arg(&run_dir, Some("/nope/none.jsonl")),
            None
        );
        // A real file under session/ wins over the argument.
        std::fs::write(run_dir.join("session").join("own.jsonl"), "{}\n").unwrap();
        assert_eq!(
            session_file_or_arg(&run_dir, Some(&arg)),
            Some(run_dir.join("session").join("own.jsonl"))
        );
    }
}
