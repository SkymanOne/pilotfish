//! App state and the update loop for the fleet console: one orchestrator
//! transcript, one dashboard, one composer, one overlay at a time. The state
//! machine is pure — `handle_key` turns a key event into view-model changes
//! plus a list of [`Effect`]s, and `execute_all` carries those out (ops for
//! the run verbs, envelopes for the mailboxes). Nothing here touches the
//! terminal; `runtime.rs` feeds keys and draws the view model.
//!
//! Ported from the TypeScript `src/tui/App.tsx`, `src/tui/prefs.ts` and
//! `src/tui/workerActions.ts`, reshaped for modal keys and the
//! dashboard-with-drill-down layout.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
use crossterm::event::KeyEvent;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::fleet::envelope::{Envelope, Party, append_envelope};
use crate::fleet::event::FleetEvent;
use crate::fleet::run::{DerivedView, RunState, THINKING_LEVELS, derive_view};
use crate::orch::args::{PERMISSION_MODES, describe_permission_mode};
use crate::orch::protocol::PermissionRequest;
use crate::orch::records::{
    Capabilities, OrchestratorCommand, OrchestratorState, PermissionDecisionRecord,
};
use crate::paths::{FleetPaths, SessionKey};
use crate::tui::completions::{
    AgentCommandOption, CompletionState, CompletionTarget, apply_suggestion, completions_for,
    resolve_command,
};
use crate::tui::keys::{KeyAction, Mode, map_key};
use crate::tui::model::{
    DashboardRow, OrchSummary, RunRow, SessionTarget, activity_line, build_rows,
    session_display_name, session_label, worker_activity_line,
};
use crate::tui::palette::{
    McpServerInfo, PaletteAction, PaletteContext, PaletteItem, PaletteScope, build_items, ranked,
};
use crate::tui::transcript::Transcript;
use crate::util::now_ms;

/// How long a toolbar note stays up.
const FLASH_MS: i64 = 6_000;

/// How old the capability view may get before the console asks the agent
/// again. Short enough that a skill installed mid-session shows up on the
/// next palette, long enough that hammering `:` is not one control request
/// per keystroke.
const CAPABILITIES_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30);

/// What claude's own `/thinking` accepts; pi workers use [`THINKING_LEVELS`].
const CLAUDE_EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Rail widths the cycle steps through (the session list beside the
/// transcript).
pub const RAIL_MODES: [&str; 4] = ["compact", "auto", "wide", "full"];

/// Sent messages kept per session for `up`-recall.
const HISTORY_CAP: usize = 100;

/// Which view the console shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// The home: one row per session.
    #[default]
    Dashboard,
    /// One session's transcript, the session list beside it, composer below.
    Session,
}

/// An overlay on top of the base layout; the renderer draws whichever is set.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Help,
    Confirm(ConfirmState),
    /// A permission prompt or `AskUserQuestion` from the orchestrator.
    Permission(PermissionOverlay),
    /// The fuzzy command palette.
    Palette(PaletteState),
    /// Searching the open session's transcript.
    Search(SearchState),
    /// The selected session's full brief, scrollable.
    Brief(BriefState),
}

/// The full-brief viewer's own state (`b` in normal mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefState {
    /// The brief text: the run's `taskBrief`, or the rendered orchestrator
    /// prompt for the orchestrator session.
    pub text: String,
    /// First wrapped line the popup shows; the draw clamps it to the viewport.
    pub offset: usize,
    /// The source was missing: show the placeholder dimmed instead of text.
    pub placeholder: bool,
}

/// A blocking yes/no for anything that would destroy work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmState {
    pub message: String,
    pub action: ConfirmAction,
}

/// What confirming actually does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    RemoveWorker {
        run_id: String,
        force: bool,
    },
    /// Stop the active session and close the console (`Q` / bare `/shutdown`).
    Shutdown,
    /// Stop one named session's orchestrator and its workers; the console
    /// stays open and other sessions are untouched.
    ShutdownSession(SessionKey),
}

/// The permission/question overlay's own state (port of `Approval.tsx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOverlay {
    /// Index into the orchestrator's pending requests.
    pub at: usize,
    /// For an `AskUserQuestion`: which question is being answered.
    pub question: usize,
    /// The highlighted option; one past the last option is "something else".
    pub selected: usize,
    /// Typing a deny reason.
    pub denying: bool,
    /// Typing a custom answer.
    pub custom: bool,
    /// The deny reason or custom answer being typed.
    pub input: String,
}

/// The palette overlay's own state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteState {
    pub query: String,
    pub scope: PaletteScope,
    pub selected: usize,
    /// Everything on offer, in grouped build order.
    pub items: Vec<PaletteItem>,
    /// The query-filtered view into `items`: indices in ranked order.
    pub visible: Vec<usize>,
}

impl PaletteState {
    /// The item the selection is on, if any.
    #[must_use]
    pub fn selected_item(&self) -> Option<&PaletteItem> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    /// Refilter after a query change, keeping the selection sane.
    pub fn refilter(&mut self) {
        let scores = ranked(&self.query, &self.items);
        self.visible = scores.into_iter().map(|r| r.index).collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }
}

/// The search overlay's own state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchState {
    pub query: String,
    /// Block indices that match, oldest first.
    pub matches: Vec<usize>,
    /// Which match `n`/`N` are on.
    pub current: Option<usize>,
}

/// The composer's answer target: set while `a` is answering a pending
/// question or dialog, so `enter` resolves it instead of steering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answering {
    pub run_id: String,
    pub question_id: String,
    pub kind: AnswerKind,
}

/// Whether the pending thing is a `fleet_ask` question or a pi dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerKind {
    Question,
    Dialog,
}

/// The composer: text, cursor, and the completion popup's state.
#[derive(Debug, Clone, Default)]
pub struct Composer {
    pub input: String,
    /// Cursor position, in characters.
    pub cursor: usize,
    pub completion: Option<CompletionState>,
    pub completion_index: usize,
    /// The user dismissed the popup with esc; the next keystroke brings it back.
    pub dismissed: bool,
    /// Set while the composer is answering a pending question or dialog.
    pub answering: Option<Answering>,
}

/// A passing note above the composer; clears itself after a few seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub text: String,
    pub error: bool,
    pub at: i64,
}

/// Remembered preferences, kept in `fleet.json` under a namespaced key so the
/// watcher's cursors survive us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    /// Width of the session list beside the transcript.
    pub rail_mode: String,
    /// The row that was open when the console last closed (`orchestrator`
    /// or a run id), restored within the opened session.
    pub last_session: Option<String>,
    /// The session that was open, by uuid: re-anchors the console on open,
    /// so a switch is remembered even when the row changed afterwards.
    pub last_session_uuid: Option<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            rail_mode: "auto".to_string(),
            last_session: None,
            last_session_uuid: None,
        }
    }
}

/// Everything the console launch flags carry; constructed verbatim by
/// `main.rs`, so the field set is a frozen contract.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub cwd: Option<std::path::PathBuf>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub remote_control: Option<String>,
    pub fresh: bool,
    pub budget: Option<String>,
    pub progress_events: bool,
}

/// An action the state machine wants carried out: written envelopes, ops
/// calls, or a console exit. Pure tests assert on these.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// An ordinary user message to the orchestrator (its own slash commands
    /// and skills ride here, verbatim).
    SendToOrchestrator(String),
    /// Stop the orchestrator's running turn.
    Interrupt,
    /// Ask the agent what it currently offers, so the palette and the
    /// completions show what is installed now rather than what was
    /// installed when the session started.
    RefreshCapabilities,
    /// The same question, asked of a running worker: pi's catalogue is
    /// fleet-wide, so any live worker can answer for the installation.
    RefreshWorkerCapabilities { run_id: String },
    /// Set the orchestrator's reasoning effort.
    SetEffort(String),
    /// Set how the orchestrator's tool use is approved.
    SetPermissionMode(String),
    /// Switch the orchestrator's model, live; claude validates the name.
    SetOrchestratorModel(String),
    /// Put the orchestrator on Remote Control (restarts the claude child).
    RemoteControl(Option<String>),
    /// Allow or deny a permission prompt, or answer an `AskUserQuestion`.
    ResolvePermission {
        request_id: String,
        decision: PermissionDecisionRecord,
    },
    /// Stop the orchestrator for good (`/shutdown`).
    StopOrchestrator,
    /// Stop one named session's orchestrator (`/shutdown <key>`); its
    /// workers are aborted alongside, and the console stays open.
    StopSession(SessionKey),
    /// Point the console at another session (`/session <key>`, `/session
    /// new`): the runtime saves the current session's watcher cursors and
    /// re-anchors the polls on the new key. No IO happens here — `execute`
    /// leaves it to the runtime's event loop, which watches for it.
    SwitchSession(SessionKey),
    /// Steer a running worker (delivered after its current tool call).
    WorkerSteer { run_id: String, message: String },
    /// Queue a message for after the worker finishes its current work.
    WorkerFollowUp { run_id: String, message: String },
    /// Answer a pending `fleet_ask` question or extension dialog.
    WorkerAnswer {
        run_id: String,
        question_id: Option<String>,
        message: String,
    },
    /// Abort a worker.
    WorkerAbort { run_id: String },
    /// Change a worker's reasoning level.
    WorkerThinking { run_id: String, level: String },
    /// Switch a running worker's model.
    WorkerModel {
        run_id: String,
        model_id: String,
        provider: Option<String>,
    },
    /// A worker's own slash command, delivered as a `command` envelope — the
    /// one form that expands extension commands as well as skills.
    WorkerCommand { run_id: String, message: String },
    /// Remove a worker: worktree, branch, dashboard row (`force` aborts first).
    RemoveWorker { run_id: String, force: bool },
    /// Take the mouse, or hand it back to the terminal so its own selection
    /// works. Terminal IO, so the runtime's event loop does it; `execute`
    /// leaves it alone, the way it leaves [`Effect::Quit`].
    SetMouseCapture(bool),
    /// Write the remembered preferences.
    SavePrefs,
    /// Close the console; workers keep running.
    Quit,
}

/// One run as the console holds it: id plus its durable state.
#[derive(Debug, Clone)]
pub struct RunEntry {
    pub run_id: String,
    pub state: RunState,
}

/// An `AskUserQuestion`'s question, as the overlay picks from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskQuestion {
    pub question: String,
    pub options: Option<Vec<String>>,
}

/// The questions of an `AskUserQuestion` request.
#[must_use]
pub fn questions_of(request_input: &Value) -> Vec<AskQuestion> {
    request_input
        .get("questions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|q| AskQuestion {
                    question: q
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    options: q.get("options").and_then(Value::as_array).map(|opts| {
                        opts.iter()
                            .filter_map(|o| o.get("label").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    }),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The console's whole state, minus the terminal.
pub struct Console {
    fleet: FleetPaths,
    /// The orchestrator session this console renders: its transcript,
    /// inbox and prompt all live under `orchestrators/<key>/`. The runtime
    /// sets it before the first draw.
    pub(crate) orch_key: SessionKey,
    mode: Mode,
    view: View,
    selected: usize,
    rows: Vec<DashboardRow>,
    runs: Vec<RunEntry>,
    diff_stats: HashMap<String, String>,
    files: Vec<String>,
    orch: OrchestratorState,
    /// What the agent offers right now, read from `capabilities.json` rather
    /// than from a snapshot in `state.json`.
    caps: Capabilities,
    orch_transcript: Transcript,
    worker_transcripts: HashMap<String, Transcript>,
    composer: Composer,
    history: HashMap<String, Vec<String>>,
    history_at: Option<usize>,
    overlay: Option<Overlay>,
    /// Where the open session's view is pinned: `None` follows the tail.
    scroll: Option<usize>,
    /// The last search applied to the open session.
    search: Option<SearchState>,
    /// Answers gathered so far for a multi-question `AskUserQuestion`.
    permission_answers: HashMap<String, String>,
    prefs: Prefs,
    flash: Option<Flash>,
    /// Rows the transcript pane shows; the runtime sets it, scrolling uses it.
    pub viewport_rows: usize,
    /// Transient optimistic effort, until the monitor's state confirms it.
    pending_effort: Option<String>,
    /// Per-run optimistic thinking levels, until the worker monitor persists
    /// them into the run's state and the next poll confirms it.
    pending_thinking: HashMap<String, String>,
    /// Is the mouse ours? While it is, the wheel scrolls the transcript and
    /// the terminal never sees a drag, so its own selection cannot run.
    /// Releasing it hands both back — see [`Self::toggle_mouse`]. Never
    /// remembered across launches: a console that came up unable to scroll
    /// with the wheel, for a reason set days ago, is a bug report.
    mouse_captured: bool,
}

impl Console {
    /// A console over the fleet at `fleet`.
    #[must_use]
    pub fn new(fleet: FleetPaths) -> Self {
        // the monitor always writes one; an empty string would read as none
        let orch = OrchestratorState {
            permission_mode: "default".to_string(),
            ..OrchestratorState::default()
        };
        Self {
            fleet,
            caps: Capabilities::default(),
            // The session this console renders; the runtime replaces the
            // default with the fleet's current session before the first draw.
            orch_key: SessionKey::default(),
            mode: Mode::Normal,
            view: View::Dashboard,
            mouse_captured: true,
            selected: 0,
            rows: Vec::new(),
            runs: Vec::new(),
            diff_stats: HashMap::new(),
            files: Vec::new(),
            orch,
            orch_transcript: Transcript::new(),
            worker_transcripts: HashMap::new(),
            composer: Composer::default(),
            history: HashMap::new(),
            history_at: None,
            overlay: None,
            scroll: None,
            search: None,
            permission_answers: HashMap::new(),
            prefs: Prefs::default(),
            flash: None,
            viewport_rows: 20,
            pending_effort: None,
            pending_thinking: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Feeds — the runtime polls the fleet and hands the console its facts

    /// Replace the run list (the watcher's poll).
    pub fn set_runs(&mut self, runs: Vec<RunEntry>) {
        self.runs = runs;
        self.reconcile_pending_thinking();
        self.refresh_rows();
    }

    /// Fold still-unconfirmed thinking cycles into the fresh states (the
    /// statusline reads `state.thinking_level`), and forget one the moment
    /// the polled state catches up to it — the monitor now owns that level.
    fn reconcile_pending_thinking(&mut self) {
        let mut confirmed = Vec::new();
        for run in &mut self.runs {
            let Some(pending) = self.pending_thinking.get(&run.run_id) else {
                continue;
            };
            if run.state.thinking_level.as_deref() == Some(pending.as_str()) {
                confirmed.push(run.run_id.clone());
            } else {
                run.state.thinking_level = Some(pending.clone());
            }
        }
        for run_id in confirmed {
            self.pending_thinking.remove(&run_id);
        }
    }

    /// Replace the orchestrator state (state.json poll).
    /// Replace the capability view; the console re-reads the file every poll
    /// so a refresh the monitor answered shows up without a restart.
    pub fn set_capabilities(&mut self, caps: Capabilities) {
        self.caps = caps;
    }

    pub fn set_orchestrator_state(&mut self, state: OrchestratorState) {
        self.orch = state;
        self.pending_effort = None;
        self.refresh_rows();
    }

    /// Note a worker's diff stat for its dashboard row, when the runtime has
    /// one (the console never computes it; `diff` is expensive).
    pub fn set_diff_stat(&mut self, run_id: &str, stat: impl Into<String>) {
        self.diff_stats.insert(run_id.to_string(), stat.into());
        self.refresh_rows();
    }

    /// Drop a worker's diff stat — its worktree went away, or the diff no
    /// longer applies to the run.
    pub fn clear_diff_stat(&mut self, run_id: &str) {
        if self.diff_stats.remove(run_id).is_some() {
            self.refresh_rows();
        }
    }

    /// Repository files for `@` completion.
    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = files;
        self.recompute_completion();
    }

    /// Fold one orchestrator record into the transcript.
    pub fn ingest_orchestrator_record(&mut self, record: &crate::orch::records::EventRecord) {
        self.orch_transcript.apply_orchestrator_record(record);
        self.refresh_rows();
    }

    /// Fold one worker event into that worker's transcript.
    pub fn ingest_worker_event(&mut self, run_id: &str, event: &Value) {
        self.worker_transcript_mut(run_id).apply_worker_event(event);
    }

    /// Fold new fleet events in and return the effect that forwards them to
    /// the orchestrator (the runtime executes it).
    pub fn ingest_fleet_events(&mut self, events: &[FleetEvent], batch_text: &str) -> Vec<Effect> {
        if events.is_empty() {
            return Vec::new();
        }
        self.orch_transcript.push_fleet(events, batch_text);
        vec![Effect::SendToOrchestrator(batch_text.to_string())]
    }

    /// A passing note: toolbar above the composer, and a line in the
    /// transcript to look back at.
    pub fn notice(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        self.flash = Some(Flash {
            text: text.clone(),
            error,
            at: now_ms(),
        });
        if error {
            self.orch_transcript.push_error(&text);
        } else {
            self.orch_transcript.push_notice(&text);
        }
    }

    /// A note that belongs in the toolbar and nowhere else.
    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        self.flash = Some(Flash {
            text,
            error,
            at: now_ms(),
        });
    }

    /// The clock moved: expire the flash.
    pub fn tick(&mut self, now: i64) {
        if self
            .flash
            .as_ref()
            .is_some_and(|flash| now - flash.at > FLASH_MS)
        {
            self.flash = None;
        }
    }

    // -----------------------------------------------------------------------
    // View model for the renderer

    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub const fn view(&self) -> View {
        self.view
    }

    #[must_use]
    pub fn rows(&self) -> &[DashboardRow] {
        &self.rows
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub const fn composer(&self) -> &Composer {
        &self.composer
    }

    #[must_use]
    pub const fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    #[must_use]
    pub const fn flash(&self) -> Option<&Flash> {
        self.flash.as_ref()
    }

    #[must_use]
    pub const fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    /// The orchestrator's transcript.
    #[must_use]
    pub const fn orchestrator_transcript(&self) -> &Transcript {
        &self.orch_transcript
    }

    /// A worker's transcript, building an empty one on first look.
    pub fn worker_transcript(&mut self, run_id: &str) -> &Transcript {
        self.worker_transcript_mut(run_id)
    }

    /// The selected session's transcript.
    #[must_use]
    pub fn open_transcript(&mut self) -> &Transcript {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => &self.orch_transcript,
            SessionTarget::Worker { run_id } => self.worker_transcripts.entry(run_id).or_default(),
        }
    }

    /// The session scroll offset: `None` follows the tail.
    #[must_use]
    pub const fn scroll(&self) -> Option<usize> {
        self.scroll
    }

    /// The search state of the open session, for highlight and `n`/`N`.
    #[must_use]
    pub const fn search(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    /// The prompt the composer shows: who it is talking to, or what it is
    /// answering. The orchestrator prompt names the served session (its
    /// alias, or the short uuid while none is derived).
    #[must_use]
    pub fn composer_prompt(&self) -> String {
        if let Some(answering) = &self.composer.answering {
            return format!("answer ({}) > ", answering.question_id);
        }
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => format!("{} > ", self.session_name()),
            SessionTarget::Worker { run_id } => {
                let label = self.run_state(&run_id).map_or_else(
                    || "worker".to_string(),
                    |s| {
                        let view = derive_view(s, crate::fleet::run::is_alive, now_ms());
                        format!("{} ({view})", s.name)
                    },
                );
                format!("{label} > ")
            }
        }
    }

    /// The selected session's target.
    #[must_use]
    pub fn selected_target(&self) -> SessionTarget {
        self.rows.get(self.selected).map_or_else(
            || SessionTarget::Orchestrator(self.orch_key.uuid),
            |row| row.target.clone(),
        )
    }

    /// The selected row, if any.
    #[must_use]
    pub fn selected_row(&self) -> Option<&DashboardRow> {
        self.rows.get(self.selected)
    }

    /// Select the session by dashboard key (`orchestrator` or a run id);
    /// `false` when no such row exists, leaving the selection where it was
    /// (the caller falls back to the orchestrator). This is how the console
    /// acts on the remembered `lastSession` preference at open.
    pub fn select_target(&mut self, key: &str) -> bool {
        let Some(index) = self.rows.iter().position(|row| row.key == key) else {
            return false;
        };
        if self.selected != index {
            self.selected = index;
            // the search belonged to the session that was open
            self.search = None;
            self.scroll = None;
        }
        true
    }

    fn run_state(&self, run_id: &str) -> Option<&RunState> {
        self.runs
            .iter()
            .find(|r| r.run_id == run_id)
            .map(|r| &r.state)
    }

    /// The worker party of `run_id`: its recorded uuid, or the stable legacy
    /// encoding while its state is not loaded (or predates run uuids).
    fn worker_party(&self, run_id: &str) -> Party {
        self.run_state(run_id).map_or_else(
            || Party::Worker(crate::fleet::envelope::legacy_worker_uuid(run_id)),
            |state| Party::Worker(state.uuid),
        )
    }

    /// What the selected session is doing, for the line above the composer.
    #[must_use]
    pub fn activity_line(&self, now: i64) -> Option<String> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => activity_line(self.orch_transcript.activity(), now),
            SessionTarget::Worker { run_id } => {
                let state = self.run_state(&run_id)?;
                let view = derive_view(state, crate::fleet::run::is_alive, now);
                worker_activity_line(state, view, now)
            }
        }
    }

    /// The orchestrator's effort as shown: optimistic, else what state says.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.pending_effort
            .as_deref()
            .or(self.orch.effort.as_deref())
    }

    fn refresh_rows(&mut self) {
        let summary = OrchSummary {
            turn_active: self.orch.turn_active || self.orch_transcript.turn_active(),
            exited: self.orch.exited.is_some() || self.orch_transcript.exited(),
            pending_approvals: self.orch.pending_requests.len(),
            session_uuid: self.orch_key.uuid,
            session_name: self.session_name(),
        };
        let rows: Vec<RunRow<'_>> = self
            .runs
            .iter()
            .map(|run| RunRow {
                run_id: &run.run_id,
                state: &run.state,
                diff_stat: self.diff_stats.get(&run.run_id).map(String::as_str),
            })
            .collect();
        self.rows = build_rows(&summary, &rows, now_ms());
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// What to call the served orchestrator session in the rail and the
    /// composer: `orchestrator`, plus its alias when it has one.
    fn session_name(&self) -> String {
        session_label(self.orch_key.alias.as_deref())
    }

    fn worker_transcript_mut(&mut self, run_id: &str) -> &mut Transcript {
        self.worker_transcripts
            .entry(run_id.to_string())
            .or_default()
    }

    // -----------------------------------------------------------------------
    // Preferences (fleet.json, namespaced)

    /// Load remembered preferences, keeping the watcher's other keys intact.
    pub fn load_prefs(&mut self) {
        let path = self.fleet.fleet_json();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        if let Some(prefs) = value.get("console").and_then(Value::as_object) {
            if let Some(mode) = prefs.get("railMode").and_then(Value::as_str)
                && RAIL_MODES.contains(&mode)
            {
                self.prefs.rail_mode = mode.to_string();
            }
            if let Some(session) = prefs.get("lastSessionUuid").and_then(Value::as_str) {
                self.prefs.last_session_uuid = Some(session.to_string());
            }
            if let Some(row) = prefs.get("lastSession").and_then(Value::as_str) {
                // the pre-split console overloaded this field with the
                // session uuid; a uuid here migrates into lastSessionUuid
                if row.parse::<uuid::Uuid>().is_ok() {
                    self.prefs.last_session_uuid = Some(row.to_string());
                } else {
                    self.prefs.last_session = Some(row.to_string());
                }
            }
        }
    }

    /// Write the remembered preferences under the store's lock, so a write
    /// interleaving with the monitor's heartbeat never loses either side's
    /// data: the store now round-trips unknown keys (the `"console"` one
    /// included), and every fleet.json write the console makes goes through
    /// the same `fleet.json.lock` the monitor uses.
    pub fn save_prefs(&self) {
        let fleet_dir = self.fleet.root().to_path_buf();
        let prefs = json!({
            "railMode": self.prefs.rail_mode,
            "lastSession": self.prefs.last_session,
            "lastSessionUuid": self.prefs.last_session_uuid,
        });
        let _ = crate::orch::session::with_store_mutation(&fleet_dir, |store| {
            store.extra.insert("console".into(), prefs);
        });
    }

    // -----------------------------------------------------------------------
    // Key handling

    /// Turn a key press into view-model changes plus effects to carry out.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if self.overlay.is_some() {
            // An overlay owns the keys: it reads them like the composer does,
            // so printable characters are always text and the mode is moot.
            let action = map_key(Mode::Insert, key);
            return self.handle_action(action);
        }
        let action = map_key(self.mode, key);
        self.handle_action(action)
    }

    /// Apply an already-mapped action. The runtime routes mouse wheel events
    /// through here too; an open overlay owns the action either way.
    pub fn handle_action(&mut self, action: KeyAction) -> Vec<Effect> {
        if let Some(overlay) = self.overlay.clone() {
            return self.handle_overlay(overlay, action);
        }
        match self.mode {
            Mode::Normal => self.handle_normal(action),
            Mode::Insert => self.handle_insert(action),
        }
    }

    fn handle_overlay(&mut self, overlay: Overlay, action: KeyAction) -> Vec<Effect> {
        match overlay {
            Overlay::Help => {
                if matches!(
                    action,
                    KeyAction::Help
                        | KeyAction::Back
                        | KeyAction::Quit
                        | KeyAction::Open
                        | KeyAction::Send
                        | KeyAction::LeaveInsert
                ) {
                    self.overlay = None;
                }
                Vec::new()
            }
            Overlay::Confirm(state) => self.handle_confirm(state, action),
            Overlay::Permission(state) => self.handle_permission(state, action),
            Overlay::Palette(state) => self.handle_palette(state, action),
            Overlay::Search(state) => self.handle_search(state, action),
            Overlay::Brief(state) => self.handle_brief(state, action),
        }
    }

    fn handle_confirm(&mut self, state: ConfirmState, action: KeyAction) -> Vec<Effect> {
        let yes = matches!(action, KeyAction::InsertChar('y' | 'Y'));
        let no = matches!(action, KeyAction::InsertChar('n' | 'N'))
            || matches!(action, KeyAction::LeaveInsert | KeyAction::Send);
        if !yes && !no {
            return Vec::new();
        }
        self.overlay = None;
        if !yes {
            self.toast(
                match state.action {
                    ConfirmAction::RemoveWorker { .. } => "· removal cancelled",
                    ConfirmAction::Shutdown | ConfirmAction::ShutdownSession(_) => {
                        "· shutdown cancelled"
                    }
                },
                false,
            );
            return Vec::new();
        }
        match state.action {
            ConfirmAction::RemoveWorker { run_id, force } => {
                self.notice(format!("■ removing {}", self.name_of(&run_id)), false);
                vec![Effect::RemoveWorker { run_id, force }]
            }
            ConfirmAction::Shutdown => self.shutdown_effects(),
            ConfirmAction::ShutdownSession(key) => self.shutdown_effects_for(&key),
        }
    }

    /// Stop everything: every live worker aborted, the orchestrator stopped,
    /// the console closed.
    fn shutdown_effects(&self) -> Vec<Effect> {
        let mut effects = Vec::new();
        for run in &self.runs {
            if !Self::is_live(&run.state) {
                continue;
            }
            effects.push(Effect::WorkerAbort {
                run_id: run.run_id.clone(),
            });
        }
        effects.push(Effect::StopOrchestrator);
        effects.push(Effect::Quit);
        effects
    }

    /// Stop one named session: abort its live workers and stop its
    /// orchestrator. The console stays open and no other session is touched.
    fn shutdown_effects_for(&self, key: &SessionKey) -> Vec<Effect> {
        let mut effects = Vec::new();
        for summary in crate::fleet::run::list_runs_for_owner(self.fleet.root(), key.uuid) {
            let Ok(state) = crate::fleet::run::load_state(&summary.run_dir) else {
                continue;
            };
            if Self::is_live(&state) {
                effects.push(Effect::WorkerAbort {
                    run_id: summary.run_id,
                });
            }
        }
        effects.push(Effect::StopSession(key.clone()));
        effects
    }

    #[allow(clippy::too_many_lines)]
    fn handle_permission(
        &mut self,
        mut state: PermissionOverlay,
        action: KeyAction,
    ) -> Vec<Effect> {
        let Some(request) = self.orch.pending_requests.get(state.at).cloned() else {
            self.overlay = None;
            return Vec::new();
        };
        let is_question = crate::orch::protocol::is_ask_user_question(&request.request);
        let questions = questions_of(&request.request.input);

        // typing a deny reason or a custom answer
        if state.denying || state.custom {
            match action {
                KeyAction::InsertChar(ch) => state.input.push(ch),
                KeyAction::InsertBackspace => {
                    state.input.pop();
                }
                KeyAction::Send => {
                    let value = state.input.trim().to_string();
                    state.input.clear();
                    if state.denying {
                        return self.deny_current(state, request, value);
                    }
                    return self.answer_question(state, request, &questions, value);
                }
                KeyAction::LeaveInsert => {
                    state.denying = false;
                    state.custom = false;
                    state.input.clear();
                }
                _ => {}
            }
            self.overlay = Some(Overlay::Permission(state));
            return Vec::new();
        }

        if is_question && !questions.is_empty() {
            let current = &questions[state.question.min(questions.len() - 1)];
            let option_count = current.options.as_ref().map_or(0, Vec::len);
            match action {
                KeyAction::CompletionNext | KeyAction::Move(1) => {
                    state.selected = (state.selected + 1) % (option_count + 1);
                }
                KeyAction::CompletionPrev | KeyAction::Move(-1) => {
                    state.selected = (state.selected + option_count) % (option_count + 1);
                }
                KeyAction::Send | KeyAction::Open => {
                    if state.selected >= option_count {
                        // "✎ something else": start typing
                        state.custom = true;
                    } else {
                        let answer = current
                            .options
                            .as_ref()
                            .and_then(|o| o.get(state.selected))
                            .cloned()
                            .unwrap_or_default();
                        return self.answer_question(state, request, &questions, answer);
                    }
                }
                KeyAction::LeaveInsert => {
                    self.overlay = None;
                    return Vec::new();
                }
                _ => {}
            }
            self.overlay = Some(Overlay::Permission(state));
            return Vec::new();
        }

        // a plain permission prompt
        match action {
            KeyAction::InsertChar('y' | 'Y') => self.allow_current(state, request, None),
            KeyAction::InsertChar('a' | 'A') => {
                let suggestions = request.request.permission_suggestions.clone();
                self.allow_current(state, request, Some(suggestions))
            }
            KeyAction::InsertChar('n' | 'N') => {
                state.denying = true;
                self.overlay = Some(Overlay::Permission(state));
                Vec::new()
            }
            KeyAction::LeaveInsert => {
                self.overlay = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Record one `AskUserQuestion` answer; emit the effect on the last one.
    fn answer_question(
        &mut self,
        mut state: PermissionOverlay,
        request: PermissionRequest,
        questions: &[AskQuestion],
        answer: String,
    ) -> Vec<Effect> {
        let current = &questions[state.question.min(questions.len().saturating_sub(1))];
        self.permission_answers
            .insert(current.question.clone(), answer);
        if state.question + 1 < questions.len() {
            state.question += 1;
            state.selected = 0;
            state.custom = false;
            self.overlay = Some(Overlay::Permission(state));
            return Vec::new();
        }
        let answers = Value::Object(
            self.permission_answers
                .drain()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        );
        let effect = Effect::ResolvePermission {
            request_id: request.request_id,
            decision: PermissionDecisionRecord::Answer { answers },
        };
        self.advance_permission(state);
        vec![effect]
    }

    fn deny_current(
        &mut self,
        state: PermissionOverlay,
        request: PermissionRequest,
        reason: String,
    ) -> Vec<Effect> {
        let effect = Effect::ResolvePermission {
            request_id: request.request_id,
            decision: PermissionDecisionRecord::Deny {
                message: if reason.is_empty() {
                    "denied by the user".to_string()
                } else {
                    reason
                },
            },
        };
        self.advance_permission(state);
        vec![effect]
    }

    fn allow_current(
        &mut self,
        state: PermissionOverlay,
        request: PermissionRequest,
        updated_permissions: Option<Vec<Value>>,
    ) -> Vec<Effect> {
        let effect = Effect::ResolvePermission {
            request_id: request.request_id,
            decision: PermissionDecisionRecord::Allow {
                updated_permissions,
            },
        };
        self.advance_permission(state);
        vec![effect]
    }

    /// Move past the request just handled; close when none are left.
    fn advance_permission(&mut self, mut state: PermissionOverlay) {
        state.at += 1;
        state.question = 0;
        state.selected = 0;
        state.denying = false;
        state.custom = false;
        state.input.clear();
        if state.at >= self.orch.pending_requests.len() {
            self.overlay = None;
        } else {
            self.overlay = Some(Overlay::Permission(state));
        }
    }

    fn handle_palette(&mut self, mut state: PaletteState, action: KeyAction) -> Vec<Effect> {
        match action {
            KeyAction::InsertChar(ch) => {
                state.query.push(ch);
                state.refilter();
            }
            KeyAction::InsertBackspace => {
                state.query.pop();
                state.refilter();
            }
            KeyAction::CompletionNext | KeyAction::Move(1) => {
                if !state.visible.is_empty() {
                    state.selected = (state.selected + 1) % state.visible.len();
                }
            }
            KeyAction::CompletionPrev | KeyAction::Move(-1) => {
                if !state.visible.is_empty() {
                    state.selected =
                        (state.selected + state.visible.len() - 1) % state.visible.len();
                }
            }
            KeyAction::First => state.selected = 0,
            KeyAction::Last => state.selected = state.visible.len().saturating_sub(1),
            KeyAction::Send | KeyAction::Open => {
                let chosen = state.selected_item().cloned();
                self.overlay = None;
                if let Some(item) = chosen {
                    return self.run_palette_action(item.action);
                }
                return Vec::new();
            }
            KeyAction::LeaveInsert | KeyAction::Back => {
                self.overlay = None;
                return Vec::new();
            }
            _ => {}
        }
        self.overlay = Some(Overlay::Palette(state));
        Vec::new()
    }

    /// What a chosen palette entry does.
    fn run_palette_action(&mut self, action: PaletteAction) -> Vec<Effect> {
        match action {
            PaletteAction::ConsoleCommand(name) => match resolve_command(&name) {
                // commands that take an argument prefill the composer
                Some(spec) if spec.takes_argument => {
                    self.mode = Mode::Insert;
                    self.composer.input = format!("{name} ");
                    self.composer.cursor = self.composer.input.chars().count();
                    self.composer.dismissed = true;
                    Vec::new()
                }
                Some(_) => self.submit(&name),
                None => Vec::new(),
            },
            PaletteAction::AgentCommand {
                name,
                takes_argument,
            } => {
                if takes_argument {
                    self.mode = Mode::Insert;
                    self.composer.input = format!("/{name} ");
                    self.composer.cursor = self.composer.input.chars().count();
                    self.composer.dismissed = true;
                    Vec::new()
                } else {
                    self.submit(&format!("/{name}"))
                }
            }
            PaletteAction::Model { model_id, provider } => self.model_effect(&model_id, provider),
            PaletteAction::JumpTo(index) => {
                if index < self.rows.len() {
                    self.selected = index;
                }
                Vec::new()
            }
            PaletteAction::Reference => Vec::new(),
        }
    }

    fn handle_search(&mut self, mut state: SearchState, action: KeyAction) -> Vec<Effect> {
        match action {
            KeyAction::InsertChar(ch) => state.query.push(ch),
            KeyAction::InsertBackspace => {
                state.query.pop();
            }
            KeyAction::Send | KeyAction::Open | KeyAction::LeaveInsert | KeyAction::Back => {
                // keep the matches found so far
                self.apply_search(state.query.clone());
                self.overlay = None;
                return Vec::new();
            }
            _ => {}
        }
        // live matches as the query grows
        state.matches = self.search_matches(&state.query);
        state.current = state.matches.first().copied();
        self.overlay = Some(Overlay::Search(state));
        Vec::new()
    }

    /// `b`: pop the selected session's full brief; the composer keeps its
    /// message. A worker's is its `taskBrief`; the orchestrator's is the
    /// rendered session `prompt.md`, or a dimmed placeholder when the
    /// monitor has not written it yet.
    fn open_brief(&mut self) -> Vec<Effect> {
        let (text, placeholder) = match self.selected_target() {
            SessionTarget::Worker { run_id } => match self.run_state(&run_id) {
                Some(state) if !state.task_brief.trim().is_empty() => {
                    (state.task_brief.clone(), false)
                }
                _ => {
                    let text = format!("(no brief recorded for {})", self.name_of(&run_id));
                    (text, true)
                }
            },
            SessionTarget::Orchestrator(_) => {
                match std::fs::read_to_string(self.fleet.orchestrator_prompt(&self.orch_key)) {
                    Ok(text) if !text.trim().is_empty() => (text, false),
                    _ => (
                        "(no orchestrator prompt yet — the monitor writes prompt.md at boot)"
                            .to_string(),
                        true,
                    ),
                }
            }
        };
        self.overlay = Some(Overlay::Brief(BriefState {
            text,
            offset: 0,
            placeholder,
        }));
        Vec::new()
    }

    /// The brief popup owns its keys: esc (and the other close keys) drop
    /// it, the wheel and the scroll keys move the window, everything else is
    /// absorbed — typing never lands in the composer while it is up.
    fn handle_brief(&mut self, mut state: BriefState, action: KeyAction) -> Vec<Effect> {
        let step = (self.viewport_rows / 2).max(1);
        match action {
            KeyAction::ScrollHalfUp | KeyAction::ScrollPageUp => {
                state.offset = state.offset.saturating_sub(step);
            }
            KeyAction::ScrollHalfDown | KeyAction::ScrollPageDown => {
                state.offset = state.offset.saturating_add(step);
            }
            KeyAction::Help
            | KeyAction::Back
            | KeyAction::Open
            | KeyAction::Send
            | KeyAction::LeaveInsert => {
                self.overlay = None;
                return Vec::new();
            }
            _ => {}
        }
        self.overlay = Some(Overlay::Brief(state));
        Vec::new()
    }

    /// Case-insensitive matches over the open session's transcript blocks.
    fn search_matches(&self, query: &str) -> Vec<usize> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_lowercase();
        let blocks: &[crate::tui::transcript::Block] = match self.selected_target() {
            SessionTarget::Orchestrator(_) => self.orch_transcript.blocks(),
            SessionTarget::Worker { run_id } => self
                .worker_transcripts
                .get(&run_id)
                .map_or(&[], |t| t.blocks()),
        };
        blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.text.to_lowercase().contains(&q))
            .map(|(index, _)| index)
            .collect()
    }

    fn apply_search(&mut self, query: String) {
        let matches = self.search_matches(&query);
        let current = matches.first().copied();
        self.search = Some(SearchState {
            query,
            matches,
            current,
        });
    }

    #[allow(clippy::too_many_lines)]
    fn handle_normal(&mut self, action: KeyAction) -> Vec<Effect> {
        match action {
            KeyAction::Move(delta) => {
                self.move_selection(i64::from(delta));
                Vec::new()
            }
            KeyAction::First => {
                match self.view {
                    View::Dashboard => self.selected = 0,
                    View::Session => self.scroll = Some(0),
                }
                Vec::new()
            }
            KeyAction::Last => {
                match self.view {
                    View::Dashboard => self.selected = self.rows.len().saturating_sub(1),
                    View::Session => self.scroll = None,
                }
                Vec::new()
            }
            KeyAction::Open => {
                if self.view == View::Dashboard {
                    self.open_selected();
                }
                Vec::new()
            }
            KeyAction::Back => {
                self.view = View::Dashboard;
                Vec::new()
            }
            KeyAction::NextSession => {
                self.move_selection(1);
                Vec::new()
            }
            KeyAction::PrevSession => {
                self.move_selection(-1);
                Vec::new()
            }
            KeyAction::JumpTo(index) => {
                if index < self.rows.len() {
                    self.selected = index;
                }
                Vec::new()
            }
            KeyAction::Search => {
                self.overlay = Some(Overlay::Search(SearchState::default()));
                Vec::new()
            }
            KeyAction::OpenPalette => self.open_palette(PaletteScope::All),
            KeyAction::Help => {
                self.overlay = Some(Overlay::Help);
                Vec::new()
            }
            KeyAction::Brief => self.open_brief(),
            KeyAction::Quit => vec![Effect::Quit],
            KeyAction::EnterInsert => {
                self.enter_insert();
                Vec::new()
            }
            KeyAction::Shutdown => {
                self.overlay = Some(Overlay::Confirm(ConfirmState {
                    message: self.shutdown_question(),
                    action: ConfirmAction::Shutdown,
                }));
                Vec::new()
            }
            KeyAction::Answer => self.answer_selected(),
            KeyAction::Stop => self.stop_selected(),
            KeyAction::Remove => self.remove_selected(),
            KeyAction::CycleThinking => self.cycle_thinking(),
            KeyAction::Models => self.open_palette(PaletteScope::Models),
            KeyAction::PermissionMode => self.cycle_permission_mode(),
            KeyAction::ToggleMouse => self.toggle_mouse(),
            KeyAction::ScrollHalfDown => {
                if self.view == View::Session {
                    self.scroll_page(self.viewport_rows as i64 / 2);
                }
                Vec::new()
            }
            KeyAction::ScrollHalfUp => {
                if self.view == View::Session {
                    self.scroll_page(-(self.viewport_rows as i64) / 2);
                }
                Vec::new()
            }
            KeyAction::ScrollPageDown => {
                if self.view == View::Session {
                    self.scroll_page(self.viewport_rows as i64);
                }
                Vec::new()
            }
            KeyAction::ScrollPageUp => {
                if self.view == View::Session {
                    self.scroll_page(-(self.viewport_rows as i64));
                }
                Vec::new()
            }
            KeyAction::NextMatch => {
                self.step_match(1);
                Vec::new()
            }
            KeyAction::PrevMatch => {
                self.step_match(-1);
                Vec::new()
            }
            // typing starts a message, keeping the character
            KeyAction::InsertChar(ch) => {
                self.enter_insert();
                self.composer.input.push(ch);
                self.composer.cursor = self.composer.input.chars().count();
                self.composer.dismissed = false;
                self.recompute_completion();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn move_selection(&mut self, delta: i64) {
        let total = self.rows.len();
        if total == 0 {
            return;
        }
        let next = (self.selected as i64 + delta).rem_euclid(total as i64) as usize;
        if next != self.selected {
            self.selected = next;
            // the search belonged to the session that was open
            self.search = None;
            self.scroll = None;
        }
    }

    fn open_selected(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.view = View::Session;
        self.scroll = None;
        self.search = None;
        if let Some(row) = self.rows.get(self.selected) {
            self.prefs.last_session = Some(row.key.clone());
        }
    }

    fn scroll_page(&mut self, delta: i64) {
        let blocks = self.open_transcript_blocks_len();
        if blocks == 0 {
            return;
        }
        let current = self.scroll.unwrap_or_else(|| blocks.saturating_sub(1));
        let next = (current as i64 + delta).clamp(0, blocks.saturating_sub(1) as i64) as usize;
        self.scroll = Some(next);
    }

    fn open_transcript_blocks_len(&self) -> usize {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => self.orch_transcript.blocks().len(),
            SessionTarget::Worker { run_id } => self
                .worker_transcripts
                .get(&run_id)
                .map_or(0, |t| t.blocks().len()),
        }
    }

    fn step_match(&mut self, delta: i64) {
        let Some(search) = &mut self.search else {
            return;
        };
        if search.matches.is_empty() {
            return;
        }
        let total = search.matches.len() as i64;
        let current = search.current.map_or(0, |c| c as i64);
        let next = (current + delta).rem_euclid(total) as usize;
        search.current = Some(next);
        // pin the view at the match
        self.scroll = Some(search.matches[next]);
    }

    fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.composer.dismissed = false;
        self.recompute_completion();
    }

    fn handle_insert(&mut self, action: KeyAction) -> Vec<Effect> {
        match action {
            KeyAction::InsertChar(ch) => {
                let cursor = self
                    .composer
                    .cursor
                    .min(self.composer.input.chars().count());
                let byte = char_to_byte(&self.composer.input, cursor);
                self.composer.input.insert(byte, ch);
                self.composer.cursor += 1;
                self.composer.dismissed = false;
                self.history_at = None;
                self.recompute_completion();
                Vec::new()
            }
            KeyAction::InsertBackspace => {
                let cursor = self
                    .composer
                    .cursor
                    .min(self.composer.input.chars().count());
                if cursor > 0 {
                    let start = char_to_byte(&self.composer.input, cursor - 1);
                    let end = char_to_byte(&self.composer.input, cursor);
                    self.composer.input.replace_range(start..end, "");
                    self.composer.cursor -= 1;
                    self.history_at = None;
                    self.recompute_completion();
                }
                Vec::new()
            }
            KeyAction::InsertDelete => {
                let cursor = self
                    .composer
                    .cursor
                    .min(self.composer.input.chars().count());
                if cursor < self.composer.input.chars().count() {
                    let start = char_to_byte(&self.composer.input, cursor);
                    let end = char_to_byte(&self.composer.input, cursor + 1);
                    self.composer.input.replace_range(start..end, "");
                    self.recompute_completion();
                }
                Vec::new()
            }
            KeyAction::InsertLeft => {
                self.composer.cursor = self.composer.cursor.saturating_sub(1);
                Vec::new()
            }
            KeyAction::InsertRight => {
                let len = self.composer.input.chars().count();
                self.composer.cursor = (self.composer.cursor + 1).min(len);
                Vec::new()
            }
            KeyAction::InsertHome => {
                self.composer.cursor = 0;
                Vec::new()
            }
            KeyAction::InsertEnd => {
                self.composer.cursor = self.composer.input.chars().count();
                Vec::new()
            }
            KeyAction::Newline => {
                let cursor = self
                    .composer
                    .cursor
                    .min(self.composer.input.chars().count());
                let byte = char_to_byte(&self.composer.input, cursor);
                self.composer.input.insert(byte, '\n');
                self.composer.cursor += 1;
                Vec::new()
            }
            KeyAction::Send => {
                let input = self.composer.input.clone();
                self.submit(&input)
            }
            KeyAction::AcceptCompletion => {
                self.accept_completion();
                Vec::new()
            }
            KeyAction::CompletionNext | KeyAction::CompletionPrev => {
                let delta = if action == KeyAction::CompletionNext {
                    1
                } else {
                    -1
                };
                let open = self
                    .composer
                    .completion
                    .as_ref()
                    .is_some_and(|c| !c.items.is_empty())
                    && !self.composer.dismissed;
                if open {
                    let total = self
                        .composer
                        .completion
                        .as_ref()
                        .map_or(1, |c| c.items.len());
                    self.composer.completion_index = (self.composer.completion_index as i64 + delta)
                        .rem_euclid(total as i64)
                        as usize;
                } else {
                    self.recall_history(delta);
                }
                Vec::new()
            }
            KeyAction::LeaveInsert => {
                self.mode = Mode::Normal;
                self.composer.answering = None;
                self.composer.dismissed = true;
                Vec::new()
            }
            // the wheel (and the odd ctrl-d/ctrl-u) scroll the transcript
            // while the composer has focus; typing is never interrupted
            KeyAction::ScrollHalfUp | KeyAction::ScrollHalfDown => {
                if self.view == View::Session {
                    let delta = if action == KeyAction::ScrollHalfUp {
                        -1
                    } else {
                        1
                    };
                    self.scroll_page(delta * (self.viewport_rows as i64 / 2));
                }
                Vec::new()
            }
            KeyAction::PaletteInInsert => self.open_palette(PaletteScope::All),
            _ => Vec::new(),
        }
    }

    fn recompute_completion(&mut self) {
        let ctx = crate::tui::completions::CompletionContext {
            target: match self.selected_target() {
                SessionTarget::Orchestrator(_) => CompletionTarget::Orchestrator,
                SessionTarget::Worker { .. } => CompletionTarget::Worker,
            },
            workers: self
                .runs
                .iter()
                .map(|r| {
                    let view = derive_view(&r.state, crate::fleet::run::is_alive, now_ms());
                    (r.state.name.clone(), view.to_string())
                })
                .collect(),
            files: self.files.clone(),
            agent_commands: self.agent_commands_for_target(),
        };
        self.composer.completion = completions_for(&self.composer.input, &ctx);
        self.composer.completion_index = 0;
    }

    /// The agent's own commands for whichever session is selected, verbatim.
    fn agent_commands_for_target(&self) -> Vec<AgentCommandOption> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => self
                .caps
                .commands
                .iter()
                .map(|c| {
                    AgentCommandOption::from_orchestrator(
                        &c.name,
                        c.description.as_deref(),
                        c.argument_hint.as_deref(),
                    )
                })
                .collect(),
            SessionTarget::Worker { run_id } => self
                .run_state(&run_id)
                .map(|state| {
                    state
                        .commands
                        .iter()
                        .map(|c| {
                            AgentCommandOption::from_worker(&c.name, &c.description, &c.source)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn accept_completion(&mut self) {
        let Some(completion) = self.composer.completion.clone() else {
            return;
        };
        let Some(suggestion) = completion
            .items
            .get(
                self.composer
                    .completion_index
                    .min(completion.items.len() - 1),
            )
            .cloned()
        else {
            return;
        };
        self.composer.input = apply_suggestion(&self.composer.input, &completion, &suggestion);
        self.composer.cursor = self.composer.input.chars().count();
        self.composer.completion_index = 0;
        self.composer.dismissed = true;
    }

    /// `up`/`down` with no completions open: recall what you sent here.
    fn recall_history(&mut self, delta: i64) {
        let key = match self.selected_target() {
            SessionTarget::Orchestrator(_) => self.orch_key.uuid.to_string(),
            SessionTarget::Worker { run_id } => run_id,
        };
        let entries = self.history.entry(key).or_default();
        if entries.is_empty() {
            return;
        }
        let at = match (self.history_at, delta < 0) {
            (None, true) => entries.len() - 1,
            (None, false) => {
                self.history_at = None;
                self.composer.input.clear();
                self.composer.cursor = 0;
                return;
            }
            (Some(at), true) => at.saturating_sub(1),
            (Some(at), false) => at + 1,
        };
        if at >= entries.len() {
            self.history_at = None;
            self.composer.input.clear();
            self.composer.cursor = 0;
            return;
        }
        self.history_at = Some(at);
        self.composer.input = entries[at].clone();
        self.composer.cursor = self.composer.input.chars().count();
        self.composer.dismissed = true;
    }

    /// Open the palette over the last-known capabilities, and ask for fresh
    /// ones when they are stale. The palette draws immediately from what is
    /// on disk; the answer lands on a later poll.
    fn open_palette(&mut self, scope: PaletteScope) -> Vec<Effect> {
        let ctx = self.palette_context();
        let items = build_items(&ctx, scope);
        let mut state = PaletteState {
            query: String::new(),
            scope,
            selected: 0,
            items,
            visible: Vec::new(),
        };
        state.refilter();
        self.overlay = Some(Overlay::Palette(state));
        self.refresh_capabilities_if_stale()
    }

    /// One [`Effect::RefreshCapabilities`] when the capability view has aged
    /// past [`CAPABILITIES_MAX_AGE`], nothing when it is fresh.
    fn refresh_capabilities_if_stale(&self) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.caps.is_stale(CAPABILITIES_MAX_AGE) {
            effects.push(Effect::RefreshCapabilities);
        }
        // pi's catalogue is a property of the installation, so any running
        // worker can answer for it; with none running there is nobody to ask
        // and the last answer stands.
        let pi_stale = crate::fleet::run::read_pi_cache(self.fleet.root()).is_none_or(|cache| {
            crate::util::age_of(&cache.fetched_at).is_none_or(|age| age > CAPABILITIES_MAX_AGE)
        });
        if pi_stale && let Some(run_id) = self.first_live_run_id() {
            effects.push(Effect::RefreshWorkerCapabilities { run_id });
        }
        effects
    }

    /// A worker whose monitor can still answer an RPC, if there is one.
    fn first_live_run_id(&self) -> Option<String> {
        self.runs
            .iter()
            .find(|entry| {
                matches!(
                    crate::fleet::run::derive_view(
                        &entry.state,
                        crate::fleet::run::is_alive,
                        crate::util::now_ms(),
                    ),
                    crate::fleet::run::DerivedView::Running
                        | crate::fleet::run::DerivedView::Blocked
                )
            })
            .map(|entry| entry.run_id.clone())
    }

    fn palette_context(&self) -> PaletteContext {
        let target = self.selected_target();
        let target_is_worker = target.is_worker();
        let worker_commands = if target_is_worker {
            self.run_state_worker(&target)
                .map(|state| {
                    state
                        .commands
                        .iter()
                        .map(|c| {
                            AgentCommandOption::from_worker(&c.name, &c.description, &c.source)
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        PaletteContext {
            target_is_worker,
            orchestrator_commands: self
                .caps
                .commands
                .iter()
                .map(|c| {
                    AgentCommandOption::from_orchestrator(
                        &c.name,
                        c.description.as_deref(),
                        c.argument_hint.as_deref(),
                    )
                })
                .collect(),
            worker_commands,
            mcp_servers: self.mcp_infos(),
            worker_models: if target_is_worker {
                self.run_state_worker(&target)
                    .map(|state| state.available_models.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            sessions: self.rows.iter().map(|row| row.name.clone()).collect(),
        }
    }

    fn run_state_worker(&self, target: &SessionTarget) -> Option<&RunState> {
        let SessionTarget::Worker { run_id } = target else {
            return None;
        };
        self.run_state(run_id)
    }

    /// The orchestrator's MCP servers with their tools (from the system init
    /// message) and status.
    fn mcp_infos(&self) -> Vec<McpServerInfo> {
        self.caps
            .mcp_servers
            .iter()
            .map(|server| {
                let prefix = format!("mcp__{}__", server.name);
                let tools = self
                    .caps
                    .tools
                    .iter()
                    .filter(|tool| tool.starts_with(&prefix))
                    .cloned()
                    .collect();
                McpServerInfo {
                    name: server.name.clone(),
                    status: server.status.clone(),
                    tools,
                }
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Session commands (`/sessions`, `/session`, `/shutdown <key>`)

    /// `/sessions`: every live session with its alias, short uuid, worker
    /// count and monitor health — `running` from a live monitor with a fresh
    /// heartbeat, `wedged` from a live one that stopped stamping (two missed
    /// stamps inside the 15 s grace), `stopped` for a row with no live
    /// monitor (never started, or its pid is gone). The full list goes into
    /// the transcript to look back at; the toolbar carries a tally.
    fn show_sessions(&mut self) {
        let sessions = crate::orch::session::list_sessions(self.fleet.root());
        if sessions.is_empty() {
            self.notice("· no sessions yet — /session new starts one", false);
            return;
        }
        let counts = self.worker_counts_by_session();
        let now = crate::util::now_ms();
        let mut lines = Vec::with_capacity(sessions.len());
        let mut running = 0;
        let mut wedged = 0;
        let mut stopped = 0;
        for session in &sessions {
            let short = crate::util::short_uuid(&session.uuid);
            let label = match session.alias.as_deref() {
                Some(alias) => format!("{alias} ({short})"),
                None => short,
            };
            let workers = counts.get(&session.uuid).copied().unwrap_or(0);
            let health = match crate::orch::session::monitor_health(
                session,
                crate::fleet::run::is_alive,
                now,
            ) {
                crate::orch::session::MonitorHealth::Running => {
                    running += 1;
                    "running"
                }
                crate::orch::session::MonitorHealth::Wedged => {
                    wedged += 1;
                    "wedged"
                }
                crate::orch::session::MonitorHealth::Stopped => {
                    stopped += 1;
                    "stopped"
                }
            };
            let active = session.uuid == self.orch_key.uuid;
            lines.push(format!(
                "{}{} · {} worker{} · {}",
                if active { "▸ " } else { "  " },
                label,
                workers,
                if workers == 1 { "" } else { "s" },
                health
            ));
        }
        self.orch_transcript.push_notice(&lines.join("\n"));
        self.toast(
            format!(
                "· {} session{} · {running} running · {wedged} wedged · {stopped} stopped",
                sessions.len(),
                if sessions.len() == 1 { "" } else { "s" },
            ),
            false,
        );
    }

    /// One session's worker count, non-archived runs only: `runs/` is
    /// scanned as-written for the `/sessions` table, regardless of which
    /// session the console serves.
    fn worker_counts_by_session(&self) -> HashMap<Uuid, usize> {
        let mut counts = HashMap::new();
        for summary in crate::fleet::run::list_runs(self.fleet.root()) {
            let Ok(state) = crate::fleet::run::load_state(&summary.run_dir) else {
                continue;
            };
            if state.status == crate::fleet::run::RunStatus::Archived {
                continue;
            }
            if let Some(owner) = state.orchestrator_id {
                *counts.entry(owner).or_default() += 1;
            }
        }
        counts
    }

    /// `/session new [alias]` and `/session <uuid-or-alias>`: create or
    /// resolve a session and point the console at it. The switch itself is
    /// the runtime's job ([`Effect::SwitchSession`]); here the session is
    /// recorded as the remembered one and the command is answered.
    fn session_command(&mut self, argument: &str) -> Vec<Effect> {
        let mut words = argument.split_whitespace();
        match words.next() {
            None => {
                self.notice(
                    "· usage: /session new [alias], or /session <uuid-or-alias> — /sessions lists them",
                    false,
                );
                Vec::new()
            }
            Some("new") => {
                let alias: Option<String> = {
                    let rest: Vec<&str> = words.collect();
                    (!rest.is_empty()).then(|| rest.join(" "))
                };
                match crate::orch::session::create_session(self.fleet.root(), alias.as_deref()) {
                    Ok(session) => {
                        self.prefs.last_session_uuid = Some(session.uuid.to_string());
                        let label = session_display_name(session.alias.as_deref(), session.uuid);
                        self.notice(
                            format!(
                                "· new session {label} — the orchestrator derives an alias from its first prompt"
                            ),
                            false,
                        );
                        vec![Effect::SavePrefs, Effect::SwitchSession(session.key())]
                    }
                    Err(err) => {
                        self.notice(format!("! creating a session: {err:#}"), true);
                        Vec::new()
                    }
                }
            }
            Some(key) => match crate::orch::session::resolve_session_by_key(self.fleet.root(), key)
            {
                Ok(session) => {
                    if session.uuid == self.orch_key.uuid {
                        self.toast("· already on this session", false);
                        return Vec::new();
                    }
                    let label = session_display_name(session.alias.as_deref(), session.uuid);
                    self.notice(
                        format!("· switched to {label} — /sessions lists every session"),
                        false,
                    );
                    self.prefs.last_session_uuid = Some(session.uuid.to_string());
                    vec![Effect::SavePrefs, Effect::SwitchSession(session.key())]
                }
                Err(err) => {
                    // a miss may be an ambiguous alias: the error names the
                    // colliding sessions so the user can pick a uuid
                    self.notice(format!("! {err:#}"), true);
                    Vec::new()
                }
            },
        }
    }

    /// `/shutdown` with no argument stops the active session and closes the
    /// console (the classic `Q`); with a key it stops that session only —
    /// its workers aborted, its orchestrator told to stop — and the console
    /// stays open, so other sessions and their workers are untouched.
    fn shutdown_command(&mut self, argument: &str) -> Vec<Effect> {
        let (message, action) = if argument.is_empty() {
            (self.shutdown_question(), ConfirmAction::Shutdown)
        } else {
            match crate::orch::session::resolve_session_by_key(self.fleet.root(), argument) {
                Ok(session) if session.uuid == self.orch_key.uuid => {
                    // naming the active session is the same shutdown
                    (self.shutdown_question(), ConfirmAction::Shutdown)
                }
                Ok(session) => {
                    let live = self.live_worker_count_for(session.uuid);
                    let label = session_display_name(session.alias.as_deref(), session.uuid);
                    (
                        format!(
                            "Stop {label}'s orchestrator and its {live} running {}? Worktrees and branches are kept; the console stays open.",
                            if live == 1 { "worker" } else { "workers" }
                        ),
                        ConfirmAction::ShutdownSession(session.key()),
                    )
                }
                Err(err) => {
                    // the miss may be an ambiguous alias: the error names the
                    // colliding sessions so the user can pick a uuid
                    self.notice(format!("! {err:#}"), true);
                    return Vec::new();
                }
            }
        };
        self.overlay = Some(Overlay::Confirm(ConfirmState { message, action }));
        Vec::new()
    }

    /// Reset the console onto another session: new key, fresh transcripts;
    /// the runtime follows up by re-anchoring the polls on that key
    /// ([`Effect::SwitchSession`]), which is what actually repopulates the
    /// rows, runs and orchestrator state.
    pub fn begin_session(&mut self, key: &SessionKey) {
        self.orch_key = key.clone();
        self.orch_transcript = Transcript::new();
        self.worker_transcripts.clear();
        self.diff_stats.clear();
        self.runs.clear();
        self.rows.clear();
        self.selected = 0;
        self.view = View::Dashboard;
        self.scroll = None;
        self.search = None;
        self.permission_answers.clear();
        self.pending_effort = None;
        self.pending_thinking.clear();
        self.composer.answering = None;
        self.flash = None;
    }

    fn name_of(&self, run_id: &str) -> String {
        self.run_state(run_id)
            .map_or_else(|| run_id.to_string(), |s| s.name.clone())
    }

    fn is_live(state: &RunState) -> bool {
        !matches!(
            derive_view(state, crate::fleet::run::is_alive, now_ms()),
            DerivedView::Settled
                | DerivedView::Stopped
                | DerivedView::Error
                | DerivedView::Dead
                | DerivedView::Archived
        )
    }

    fn live_worker_count(&self) -> usize {
        self.runs
            .iter()
            .filter(|run| Self::is_live(&run.state))
            .count()
    }

    /// How many of another session's workers are live, for the `/shutdown
    /// <key>` confirmation.
    fn live_worker_count_for(&self, owner: Uuid) -> usize {
        crate::fleet::run::list_runs_for_owner(self.fleet.root(), owner)
            .iter()
            .filter(|summary| {
                crate::fleet::run::load_state(&summary.run_dir)
                    .is_ok_and(|state| Self::is_live(&state))
            })
            .count()
    }

    fn shutdown_question(&self) -> String {
        let live = self.live_worker_count();
        format!(
            "Stop the orchestrator and {live} running {}? Worktrees and branches are kept.",
            if live == 1 { "worker" } else { "workers" }
        )
    }

    /// `a`: answer the selected session's pending question or dialog.
    fn answer_selected(&mut self) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => {
                if self.orch.pending_requests.is_empty() {
                    self.toast("! the orchestrator has nothing waiting for an answer", true);
                    return Vec::new();
                }
                // a question with no options is answered in your own words
                let mut custom = false;
                let request = &self.orch.pending_requests[0];
                if crate::orch::protocol::is_ask_user_question(&request.request) {
                    let questions = questions_of(&request.request.input);
                    if questions
                        .first()
                        .is_some_and(|first| first.options.as_ref().is_none_or(Vec::is_empty))
                    {
                        custom = true;
                    }
                }
                self.overlay = Some(Overlay::Permission(PermissionOverlay {
                    at: 0,
                    question: 0,
                    selected: 0,
                    denying: false,
                    custom,
                    input: String::new(),
                }));
                Vec::new()
            }
            SessionTarget::Worker { run_id } => {
                let Some(state) = self.run_state(&run_id).cloned() else {
                    self.toast("! that worker is gone", true);
                    return Vec::new();
                };
                if let Some(question) = &state.pending_question {
                    self.enter_insert();
                    self.composer.input.clear();
                    self.composer.cursor = 0;
                    self.composer.answering = Some(Answering {
                        run_id,
                        question_id: question.id.clone(),
                        kind: AnswerKind::Question,
                    });
                    return Vec::new();
                }
                if let Some(dialog) = &state.pending_dialog {
                    self.enter_insert();
                    // a select or confirm dialog answers with one of its options
                    let prefill = match dialog.method.as_str() {
                        "select" | "confirm" => dialog
                            .options
                            .as_ref()
                            .and_then(|o| o.first())
                            .cloned()
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    self.composer.input = prefill;
                    self.composer.cursor = self.composer.input.chars().count();
                    self.composer.answering = Some(Answering {
                        run_id,
                        question_id: dialog.id.clone(),
                        kind: AnswerKind::Dialog,
                    });
                    return Vec::new();
                }
                self.toast(
                    format!(
                        "! {} has no pending question — press i to steer it instead",
                        state.name
                    ),
                    true,
                );
                Vec::new()
            }
        }
    }

    /// `s`: stop the selected session.
    fn stop_selected(&mut self) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => {
                if self.orch.turn_active || self.orch_transcript.turn_active() {
                    self.toast("· interrupt requested", false);
                    return vec![Effect::Interrupt];
                }
                self.toast("· the orchestrator is idle", false);
                Vec::new()
            }
            SessionTarget::Worker { run_id } => {
                let Some(state) = self.run_state(&run_id).cloned() else {
                    self.toast("! that worker is gone", true);
                    return Vec::new();
                };
                let view = derive_view(&state, crate::fleet::run::is_alive, now_ms());
                if is_terminal_view(view) {
                    self.toast(
                        format!("! {} is {view} — nothing to stop", state.name),
                        true,
                    );
                    return Vec::new();
                }
                self.notice(format!("■ abort requested for {}", state.name), false);
                vec![Effect::WorkerAbort { run_id }]
            }
        }
    }

    /// `x`: remove the selected worker, asking first.
    fn remove_selected(&mut self) -> Vec<Effect> {
        let SessionTarget::Worker { run_id } = self.selected_target() else {
            self.toast("! /remove needs a worker selected", true);
            return Vec::new();
        };
        let Some(state) = self.run_state(&run_id).cloned() else {
            self.toast("! that worker is gone", true);
            return Vec::new();
        };
        let view = derive_view(&state, crate::fleet::run::is_alive, now_ms());
        let message = if is_terminal_view(view) {
            format!("Remove {}'s worktree and branch?", state.name)
        } else {
            format!(
                "{} is {view}. Abort it and remove its worktree and branch?",
                state.name
            )
        };
        self.overlay = Some(Overlay::Confirm(ConfirmState {
            message,
            action: ConfirmAction::RemoveWorker {
                run_id,
                force: !is_terminal_view(view),
            },
        }));
        Vec::new()
    }

    /// `t`: cycle the selected session's thinking level.
    fn cycle_thinking(&mut self) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => {
                let current = self.effort().map(str::to_string);
                let next = next_level(&CLAUDE_EFFORT_LEVELS, current.as_deref());
                self.pending_effort = Some(next.clone());
                self.toast(format!("· thinking {next}"), false);
                vec![Effect::SetEffort(next)]
            }
            SessionTarget::Worker { run_id } => {
                let Some(state) = self.run_state(&run_id).cloned() else {
                    self.toast("! that worker is gone", true);
                    return Vec::new();
                };
                let view = derive_view(&state, crate::fleet::run::is_alive, now_ms());
                if is_terminal_view(view) {
                    self.toast(
                        format!(
                            "! {} is {view} — its thinking level no longer matters",
                            state.name
                        ),
                        true,
                    );
                    return Vec::new();
                }
                let current = self
                    .pending_thinking
                    .get(&run_id)
                    .map(String::as_str)
                    .or(state.thinking_level.as_deref());
                let next = next_level(&worker_thinking_levels(&state), current);
                // optimistic, like the orchestrator's pending_effort: the
                // statusline reads it via the state overlay in set_runs, and
                // the next press advances from it instead of the stale state
                self.pending_thinking.insert(run_id.clone(), next.clone());
                self.toast(format!("· {} thinking {next}", state.name), false);
                vec![Effect::WorkerThinking {
                    run_id,
                    level: next,
                }]
            }
        }
    }

    /// `v`: hand the mouse to the terminal, or take it back.
    ///
    /// Mouse capture is what lets the wheel scroll the transcript, and it is
    /// also what stops the terminal ever seeing a drag — so while it is on,
    /// the terminal's own click-and-drag selection cannot run and there is
    /// no way to copy what is on screen. Releasing it trades the wheel for
    /// selection; the status line says which is in force, because a wheel
    /// that silently stopped working is worse than either.
    fn toggle_mouse(&mut self) -> Vec<Effect> {
        self.mouse_captured = !self.mouse_captured;
        if self.mouse_captured {
            self.toast("· mouse taken back — the wheel scrolls again", false);
        } else {
            self.toast(
                "· mouse released — drag to select and copy, v takes it back",
                false,
            );
        }
        vec![Effect::SetMouseCapture(self.mouse_captured)]
    }

    /// Is the mouse the console's, rather than the terminal's?
    #[must_use]
    pub const fn mouse_captured(&self) -> bool {
        self.mouse_captured
    }

    /// `p`: cycle the orchestrator's permission mode.
    fn cycle_permission_mode(&mut self) -> Vec<Effect> {
        if !self.selected_target().is_worker() {
            let index = PERMISSION_MODES
                .iter()
                .position(|m| *m == self.orch.permission_mode)
                .map_or(0, |i| (i + 1) % PERMISSION_MODES.len());
            let next = PERMISSION_MODES[index];
            self.orch.permission_mode = next.to_string();
            self.toast(
                format!("· permissions → {next}: {}", describe_permission_mode(next)),
                false,
            );
            return vec![Effect::SetPermissionMode(next.to_string())];
        }
        self.toast("! /permissions is orchestrator-only (tab switches)", true);
        Vec::new()
    }

    /// The `/model` effect for the selected session: an orchestrator command
    /// or a worker `model` envelope; claude validates its own names.
    fn model_effect(&self, model_id: &str, provider: Option<String>) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => {
                vec![Effect::SetOrchestratorModel(model_id.to_string())]
            }
            SessionTarget::Worker { run_id } => vec![Effect::WorkerModel {
                run_id,
                model_id: model_id.to_string(),
                provider,
            }],
        }
    }

    // -----------------------------------------------------------------------
    // The composer's line

    /// Run one composer line (also the palette's path into commands).
    pub fn submit(&mut self, value: &str) -> Vec<Effect> {
        let text = value.trim().to_string();
        self.composer.input.clear();
        self.composer.cursor = 0;
        self.composer.completion = None;
        self.composer.completion_index = 0;
        self.composer.dismissed = false;
        self.history_at = None;
        self.flash = None;
        if text.is_empty() {
            return Vec::new();
        }
        // answering a pending question or dialog: enter resolves it
        if let Some(answering) = self.composer.answering.take() {
            return vec![Effect::WorkerAnswer {
                run_id: answering.run_id,
                question_id: Some(answering.question_id),
                message: text,
            }];
        }
        // the console's global commands work wherever the selection is
        let (head, argument) = match text.split_once(' ') {
            Some((head, rest)) => (head, rest.trim()),
            None => (text.as_str(), ""),
        };
        match resolve_command(head).map(|spec| spec.name) {
            Some("/quit") => return vec![Effect::Quit],
            Some("/help") => {
                self.overlay = Some(Overlay::Help);
                return Vec::new();
            }
            Some("/shutdown") => return self.shutdown_command(argument),
            Some("/sessions") => {
                self.show_sessions();
                return Vec::new();
            }
            Some("/session") => return self.session_command(argument),
            Some("/rail") => return self.set_rail((!argument.is_empty()).then_some(argument)),
            Some("/mouse") => return self.toggle_mouse(),
            _ => {}
        }
        self.remember_history(&text);
        match self.selected_target() {
            SessionTarget::Worker { run_id } => self.submit_to_worker(&run_id, &text),
            SessionTarget::Orchestrator(_) => self.submit_to_orchestrator(&text),
        }
    }

    fn set_rail(&mut self, want: Option<&str>) -> Vec<Effect> {
        let Some(want) = want.map(str::trim).filter(|w| !w.is_empty()) else {
            self.notice(
                format!(
                    "· session list {}. Set one of {}",
                    self.prefs.rail_mode,
                    RAIL_MODES.join(", ")
                ),
                false,
            );
            return Vec::new();
        };
        if !RAIL_MODES.contains(&want) {
            self.notice(format!("! usage: /rail <{}>", RAIL_MODES.join("|")), true);
            return Vec::new();
        }
        self.prefs.rail_mode = want.to_string();
        self.notice(format!("· session list {want}"), false);
        vec![Effect::SavePrefs]
    }

    fn remember_history(&mut self, text: &str) {
        let key = match self.selected_target() {
            SessionTarget::Orchestrator(_) => self.orch_key.uuid.to_string(),
            SessionTarget::Worker { run_id } => run_id,
        };
        let entries = self.history.entry(key).or_default();
        if entries.last().map(String::as_str) != Some(text) {
            entries.push(text.to_string());
        }
        if entries.len() > HISTORY_CAP {
            entries.remove(0);
        }
    }

    /// Route one composer line aimed at a worker (port of `workerActions.ts`).
    #[allow(clippy::too_many_lines)]
    fn submit_to_worker(&mut self, run_id: &str, text: &str) -> Vec<Effect> {
        let Some(state) = self.run_state(run_id).cloned() else {
            self.notice("! that worker is gone", true);
            return Vec::new();
        };
        let view = derive_view(&state, crate::fleet::run::is_alive, now_ms());
        let finished = is_terminal_view(view);
        let (head, argument) = match text.split_once(' ') {
            Some((head, rest)) => (head, rest.trim()),
            None => (text, ""),
        };
        if let Some(spec) = resolve_command(head) {
            return match spec.name {
                "/stop" => {
                    if finished {
                        self.notice(
                            format!("! {} is {view} — nothing to stop", state.name),
                            true,
                        );
                        Vec::new()
                    } else {
                        self.notice(format!("■ abort requested for {}", state.name), false);
                        vec![Effect::WorkerAbort {
                            run_id: run_id.to_string(),
                        }]
                    }
                }
                "/followup" => {
                    if argument.is_empty() {
                        self.notice("! usage: /followup <message>", true);
                        return Vec::new();
                    }
                    if finished {
                        self.notice(self.resumed_refusal(&state, run_id, view), true);
                        return Vec::new();
                    }
                    self.notice(
                        format!("→ follow-up queued for {}: {argument}", state.name),
                        false,
                    );
                    vec![Effect::WorkerFollowUp {
                        run_id: run_id.to_string(),
                        message: argument.to_string(),
                    }]
                }
                "/answer" => {
                    let (question_id, message) =
                        parse_answer(argument, state.pending_question.as_ref());
                    if message.is_empty() {
                        self.notice("! usage: /answer [<questionId>] <text>", true);
                        return Vec::new();
                    }
                    if finished {
                        self.notice(
                            format!(
                                "! {} is {view} — nothing is waiting for an answer",
                                state.name
                            ),
                            true,
                        );
                        return Vec::new();
                    }
                    let Some(question_id) = question_id else {
                        self.notice(
                            format!(
                                "! {} has no pending question — type a message to steer it instead",
                                state.name
                            ),
                            true,
                        );
                        return Vec::new();
                    };
                    self.notice(
                        format!("→ answered {} ({question_id}): {message}", state.name),
                        false,
                    );
                    vec![Effect::WorkerAnswer {
                        run_id: run_id.to_string(),
                        question_id: Some(question_id),
                        message: message.to_string(),
                    }]
                }
                "/thinking" => {
                    let level = argument.to_lowercase();
                    let levels = worker_thinking_levels(&state);
                    if !levels.contains(&level.as_str()) {
                        self.notice(
                            format!(
                                "! usage: /thinking <{}> — what {} has",
                                levels.join("|"),
                                state.active_model.as_deref().unwrap_or("this model")
                            ),
                            true,
                        );
                        return Vec::new();
                    }
                    if finished {
                        self.notice(
                            format!(
                                "! {} is {view} — its thinking level no longer matters",
                                state.name
                            ),
                            true,
                        );
                        return Vec::new();
                    }
                    self.notice(format!("→ {} thinking level → {level}", state.name), false);
                    let run_id = run_id.to_string();
                    self.pending_thinking.insert(run_id.clone(), level.clone());
                    vec![Effect::WorkerThinking { run_id, level }]
                }
                "/model" => {
                    if argument.is_empty() {
                        let current = state.model_label().unwrap_or("default model");
                        self.toast(
                            format!("· model {current} — /model <name> switches it (pi validates)"),
                            false,
                        );
                        return Vec::new();
                    }
                    self.notice(format!("→ {} model → {argument}", state.name), false);
                    vec![Effect::WorkerModel {
                        run_id: run_id.to_string(),
                        model_id: argument.to_string(),
                        provider: None,
                    }]
                }
                "/remove" => self.remove_selected(),
                _ => {
                    self.notice(
                        format!("! {} is a console command, not a worker one", spec.name),
                        true,
                    );
                    Vec::new()
                }
            };
        }
        if text.starts_with('/') {
            // not one of ours: if the worker offers it, let pi expand it
            let known = state
                .commands
                .iter()
                .any(|c| format!("/{}", c.name) == head);
            if known {
                if finished {
                    self.notice(self.resumed_refusal(&state, run_id, view), true);
                    return Vec::new();
                }
                self.notice(format!("→ sent {head} to {}", state.name), false);
                return vec![Effect::WorkerCommand {
                    run_id: run_id.to_string(),
                    message: text.to_string(),
                }];
            }
            let offered: Vec<String> = state
                .commands
                .iter()
                .take(6)
                .map(|c| format!("/{}", c.name))
                .collect();
            self.notice(
                format!(
                    "! unknown command {head} — /answer, /followup, /stop, /remove, /help, /quit{}",
                    if offered.is_empty() {
                        String::new()
                    } else {
                        format!(", or the worker's own: {}", offered.join(", "))
                    }
                ),
                true,
            );
            return Vec::new();
        }
        if finished {
            self.notice(self.resumed_refusal(&state, run_id, view), true);
            return Vec::new();
        }
        self.notice(format!("→ steer queued for {}: {text}", state.name), false);
        vec![Effect::WorkerSteer {
            run_id: run_id.to_string(),
            message: text.to_string(),
        }]
    }

    fn resumed_refusal(&self, state: &RunState, run_id: &str, view: DerivedView) -> String {
        format!(
            "! {} is {view} — {}",
            state.name,
            crate::fleet::run::resume_hint(state, &self.fleet.run_dir(run_id))
        )
    }

    /// Route one composer line aimed at the orchestrator.
    #[allow(clippy::too_many_lines)]
    fn submit_to_orchestrator(&mut self, text: &str) -> Vec<Effect> {
        let (head, argument) = match text.split_once(' ') {
            Some((head, rest)) => (head, rest.trim()),
            None => (text, ""),
        };
        if let Some(spec) = resolve_command(head) {
            return match spec.name {
                "/thinking" => {
                    let level = argument.to_lowercase();
                    if !CLAUDE_EFFORT_LEVELS.contains(&level.as_str()) {
                        self.notice(
                            format!("! usage: /thinking <{}>", CLAUDE_EFFORT_LEVELS.join("|")),
                            true,
                        );
                        return Vec::new();
                    }
                    self.pending_effort = Some(level.clone());
                    self.toast(format!("· thinking {level}"), false);
                    vec![Effect::SetEffort(level)]
                }
                "/model" => {
                    if argument.is_empty() {
                        let current = self
                            .orch_transcript
                            .model()
                            .or(self.orch.model.as_deref())
                            .unwrap_or("unknown");
                        self.toast(
                            format!(
                                "· model {current} — /model <name> switches it (claude validates)"
                            ),
                            false,
                        );
                        return Vec::new();
                    }
                    self.toast(format!("· model → {argument}"), false);
                    vec![Effect::SetOrchestratorModel(argument.to_string())]
                }
                "/permissions" => {
                    if argument.is_empty() {
                        let current = self.orch.permission_mode.clone();
                        self.notice(
                            format!(
                                "· permissions: {current} — {}. Set one of {}",
                                describe_permission_mode(&current),
                                PERMISSION_MODES.join(", ")
                            ),
                            false,
                        );
                        return Vec::new();
                    }
                    if !PERMISSION_MODES.contains(&argument) {
                        let why = if argument == "bypassPermissions" {
                            "bypassPermissions is not offered here: it would skip the approval overlay entirely"
                        } else {
                            "usage: /permissions <default|auto|acceptEdits|dontAsk|plan>"
                        };
                        self.notice(format!("! {why}"), true);
                        return Vec::new();
                    }
                    self.orch.permission_mode = argument.to_string();
                    self.notice(
                        format!(
                            "· permissions → {argument}: {}",
                            describe_permission_mode(argument)
                        ),
                        false,
                    );
                    vec![Effect::SetPermissionMode(argument.to_string())]
                }
                _ => {
                    self.notice(
                        format!("! {} is a console command, not a message", spec.name),
                        true,
                    );
                    Vec::new()
                }
            };
        }
        if text.starts_with('/') {
            // neither ours nor one claude offers: almost certainly a typo,
            // and sending it would put a question about a command in the log
            let known = self
                .caps
                .commands
                .iter()
                .any(|c| format!("/{}", c.name) == head);
            if !known {
                let available: Vec<String> = self
                    .caps
                    .commands
                    .iter()
                    .map(|c| format!("/{}", c.name))
                    .collect();
                let near = suggest_command(head, &available);
                self.notice(
                    format!(
                        "! unknown command {head}{}",
                        near.map_or_else(String::new, |n| format!(" — did you mean {n}?"))
                    ),
                    true,
                );
                return Vec::new();
            }
        }
        self.orch_transcript.push_sent(text);
        vec![Effect::SendToOrchestrator(text.to_string())]
    }

    // -----------------------------------------------------------------------
    // Carrying effects out

    /// Carry effects out: ops for the run verbs, envelopes for the mailboxes.
    /// The runtime calls this after `handle_key`/`submit`.
    pub async fn execute_all(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            self.execute(effect).await;
        }
    }

    async fn execute(&mut self, effect: Effect) {
        let repo_root = self
            .fleet
            .root()
            .parent()
            .map_or_else(|| self.fleet.root().to_path_buf(), Path::to_path_buf);
        let result: anyhow::Result<()> = async {
            match effect {
                Effect::SendToOrchestrator(text) => {
                    self.append_orchestrator(&OrchestratorCommand::User { text })?;
                }
                Effect::Interrupt => {
                    self.append_orchestrator(&OrchestratorCommand::Interrupt)?;
                }
                Effect::RefreshCapabilities => {
                    self.append_orchestrator(&OrchestratorCommand::RefreshCapabilities)?;
                }
                Effect::RefreshWorkerCapabilities { run_id } => {
                    append_envelope(
                        &self.fleet.run_inbox(&run_id),
                        &Envelope::refresh_capabilities(Party::Console, self.worker_party(&run_id)),
                    )?;
                }
                Effect::SetEffort(level) => {
                    self.append_orchestrator(&OrchestratorCommand::Effort { level })?;
                }
                Effect::SetPermissionMode(mode) => {
                    self.append_orchestrator(&OrchestratorCommand::PermissionMode { mode })?;
                }
                Effect::SetOrchestratorModel(name) => {
                    self.append_orchestrator(&OrchestratorCommand::Model { name })?;
                }
                Effect::RemoteControl(name) => {
                    self.append_orchestrator(&OrchestratorCommand::RemoteControl { name })?;
                }
                Effect::ResolvePermission {
                    request_id,
                    decision,
                } => {
                    self.append_orchestrator(&OrchestratorCommand::Permission {
                        request_id,
                        decision,
                    })?;
                }
                Effect::StopOrchestrator => {
                    self.append_orchestrator(&OrchestratorCommand::Stop)?;
                }
                Effect::StopSession(key) => {
                    self.append_orchestrator_to(&key, &OrchestratorCommand::Stop)?;
                }
                Effect::SwitchSession(_) => {
                    // the runtime's event loop watches for it: nothing to
                    // carry out here (it re-anchors the polls, watcher and
                    // monitor, which live in `runtime.rs`)
                }
                Effect::WorkerSteer { run_id, message } => {
                    crate::ops::steer::send(&run_id, Some(&repo_root), &message).await?;
                }
                Effect::WorkerFollowUp { run_id, message } => {
                    crate::ops::steer::followup(&run_id, Some(&repo_root), &message).await?;
                }
                Effect::WorkerAnswer {
                    run_id,
                    question_id,
                    message,
                } => {
                    crate::ops::steer::answer(
                        &run_id,
                        Some(&repo_root),
                        question_id.as_deref(),
                        &message,
                    )
                    .await?;
                }
                Effect::WorkerAbort { run_id } => {
                    crate::ops::steer::stop(&run_id, Some(&repo_root)).await?;
                }
                Effect::WorkerThinking { run_id, level } => {
                    append_envelope(
                        &self.fleet.run_inbox(&run_id),
                        &Envelope::thinking(Party::Console, self.worker_party(&run_id), level),
                    )?;
                }
                Effect::WorkerModel {
                    run_id,
                    model_id,
                    provider,
                } => {
                    append_envelope(
                        &self.fleet.run_inbox(&run_id),
                        &Envelope::model(
                            Party::Console,
                            self.worker_party(&run_id),
                            model_id,
                            provider,
                        ),
                    )?;
                }
                Effect::WorkerCommand { run_id, message } => {
                    append_envelope(
                        &self.fleet.run_inbox(&run_id),
                        &Envelope::command(Party::Console, self.worker_party(&run_id), message),
                    )?;
                }
                Effect::RemoveWorker { run_id, force } => {
                    crate::ops::integrate::cleanup(&run_id, Some(&repo_root), force).await?;
                }
                Effect::SavePrefs => self.save_prefs(),
                Effect::SetMouseCapture(_) | Effect::Quit => {}
            }
            Ok(())
        }
        .await;
        if let Err(err) = result {
            self.notice(format!("! {err:#}"), true);
        }
    }

    fn append_orchestrator(&self, command: &OrchestratorCommand) -> std::io::Result<()> {
        self.append_orchestrator_to(&self.orch_key, command)
    }

    /// Write one envelope into a session's inbox; [`Console::append_orchestrator`]
    /// is the console's own session, [`Console::append_orchestrator_to`] any
    /// named one (a `/shutdown <key>` of another session). The `to` party
    /// rides the legacy default spelling either way; the physical inbox the
    /// envelope lands in is what routes it.
    fn append_orchestrator_to(
        &self,
        key: &SessionKey,
        command: &OrchestratorCommand,
    ) -> std::io::Result<()> {
        let envelope = command.to_envelope(Party::Console);
        append_envelope(&self.fleet.orchestrator_inbox(key), &envelope)
    }
}

/// Is this derived view past working? (Steering, stopping: no longer applies.)
#[must_use]
pub const fn is_terminal_view(view: DerivedView) -> bool {
    matches!(
        view,
        DerivedView::Settled
            | DerivedView::Stopped
            | DerivedView::Error
            | DerivedView::Dead
            | DerivedView::Archived
    )
}

/// The next level in a list, wrapping; an unknown current level starts at the front.
#[must_use]
pub fn next_level(levels: &[&str], current: Option<&str>) -> String {
    let at = current.and_then(|c| levels.iter().position(|l| *l == c));
    let next = at.map_or(0, |at| (at + 1) % levels.len());
    levels[next].to_string()
}

/// The thinking levels a worker will actually take: the ones pi says its
/// model has, or all of them while pi has not said. pi answers `success` to
/// a level the model does not have and keeps running at the old one, so
/// offering the full list would keep promising what cannot happen.
#[must_use]
pub fn worker_thinking_levels(state: &RunState) -> Vec<&str> {
    if state.available_thinking_levels.is_empty() {
        THINKING_LEVELS.to_vec()
    } else {
        state
            .available_thinking_levels
            .iter()
            .map(String::as_str)
            .collect()
    }
}

#[cfg(test)]
mod thinking_levels_tests {
    use super::*;

    #[test]
    fn a_worker_offers_only_the_levels_its_model_has() {
        let mut state = RunState::default();
        assert_eq!(
            worker_thinking_levels(&state),
            THINKING_LEVELS.to_vec(),
            "until pi says, every level is on the table"
        );
        state.available_thinking_levels = vec!["off".into(), "high".into(), "xhigh".into()];
        assert_eq!(worker_thinking_levels(&state), vec!["off", "high", "xhigh"]);
        // and cycling stays inside them instead of stopping on a level that
        // would be accepted and ignored
        let levels = worker_thinking_levels(&state);
        assert_eq!(next_level(&levels, Some("xhigh")), "off");
        assert_eq!(next_level(&levels, Some("high")), "xhigh");
    }
}

/// Parse `/answer [<questionId>] <text>` — the id is optional, never spaced.
#[must_use]
pub fn parse_answer<'a>(
    rest: &'a str,
    pending: Option<&crate::fleet::run::PendingQuestion>,
) -> (Option<String>, &'a str) {
    let trimmed = rest.trim();
    let mut words = trimmed.splitn(2, ' ');
    let first = words.next().unwrap_or("");
    let others = words.next().unwrap_or("");
    let looks_like_id = (first.starts_with("q_")
        || (first.starts_with('u') && first.contains('-')))
        && !first.is_empty();
    if looks_like_id && !others.is_empty() {
        return (Some(first.to_string()), others);
    }
    (pending.map(|p| p.id.clone()), trimmed)
}

/// The closest command to what was typed, when it is a near miss.
#[must_use]
pub fn suggest_command(typed: &str, available: &[String]) -> Option<String> {
    let target = typed.to_lowercase();
    let mut best: Option<(String, usize)> = None;
    for name in available {
        let distance = edit_distance(&target, &name.to_lowercase());
        if best.as_ref().is_none_or(|(_, d)| distance < *d) {
            best = Some((name.clone(), distance));
        }
    }
    let (name, distance) = best?;
    // only worth offering when it is a near miss, not a different word
    (distance <= std::cmp::max(2, target.chars().count() / 3)).then_some(name)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (prev[j] + 1)
                .min(current[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b.len()]
}

fn char_to_byte(input: &str, char_index: usize) -> usize {
    input
        .char_indices()
        .nth(char_index)
        .map_or(input.len(), |(byte, _)| byte)
}

/// Run the console until the user quits; workers keep running after.
///
/// The terminal half is `runtime.rs`: this resolves the fleet, refuses a
/// non-interactive launch with guidance, takes the single-instance lock
/// (before the terminal goes raw, so a refusal prints to a normal shell),
/// installs the terminal and the panic hook, and hands the event loop to
/// [`crate::tui::runtime::run_console`]. The terminal is restored on every
/// exit path — panic included — before anything prints.
/// Run the console: the terminal, the lock, and the event loop.
///
/// # Errors
/// Terminal bring-up (raw mode, alternate screen) and draw failures; the
/// console's own problems surface as notices, not errors.
pub async fn run_app(options: TuiOptions) -> anyhow::Result<crate::cli::ExitCode> {
    let cwd = match options.cwd.clone() {
        Some(dir) => dir,
        None => std::env::current_dir().context("no working directory")?,
    };
    if !crate::tui::runtime::is_interactive() {
        anyhow::bail!(
            "the fleet console needs an interactive terminal.\n\
             Run it in one, or drive the fleet headlessly: \
             `parl spawn <name> -- \"<brief>\"`, `parl status`, `parl report <name>`."
        );
    }
    let fleet = FleetPaths::discover(&cwd);
    fleet
        .ensure()
        .context("creating the fleet state directory")?;
    let lock = crate::tui::runtime::ConsoleLock::acquire(&fleet)?;
    crate::tui::runtime::install_panic_hook();
    let mut terminal = crate::tui::runtime::enter()?;

    let result =
        crate::tui::runtime::run_console(&mut terminal, fleet.clone(), &lock, options).await;

    // the terminal comes back before anything prints, whatever happened
    crate::tui::runtime::restore();
    drop(lock);

    let code = result?;
    // what is left running decides the goodbye
    let orch_key = crate::tui::runtime::resolve_console_key(&fleet);
    let orchestrator_exited = std::fs::read_to_string(fleet.orchestrator_state(&orch_key))
        .ok()
        .and_then(|raw| serde_json::from_str::<OrchestratorState>(&raw).ok())
        .and_then(|state| state.exited)
        .is_some();
    if orchestrator_exited {
        println!(
            "Shutdown requested; the orchestrator is stopping. `parl status` shows the workers. \
             Worktrees and branches are kept."
        );
    } else {
        println!(
            "The orchestrator and its workers keep running. `parl` reopens this console where \
             you left it; `parl status` lists the workers. `/shutdown` inside the console stops \
             everything."
        );
    }
    Ok(code)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fleet::run::{
        PendingDialog, PendingQuestion, RunStatus, WorkerCommand, WorkerModel,
    };
    use crate::orch::protocol::{AgentCommand, CanUseToolRequest, McpServerStatus};
    use crate::orch::session::OrchestratorSession;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn test_console() -> Console {
        let dir = std::env::temp_dir().join(format!(
            "parl-tui-app-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Console::new(FleetPaths::new(dir))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn enter() -> KeyEvent {
        key(KeyCode::Enter)
    }

    fn esc() -> KeyEvent {
        key(KeyCode::Esc)
    }

    fn tab() -> KeyEvent {
        key(KeyCode::Tab)
    }

    fn shift_tab() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
    }

    fn running_run(run_id: &str, name: &str) -> RunEntry {
        let mut state = RunState::new(
            "/f", run_id, name, "/repo", "brief", None, None, None, None, None, None, None, None,
            None, None, None,
        );
        state.status = RunStatus::Running;
        state.pid = Some(std::process::id() as i32);
        RunEntry {
            run_id: run_id.to_string(),
            state,
        }
    }

    fn setup_with_worker() -> Console {
        let mut console = test_console();
        console.set_runs(vec![running_run("db-20260829120000", "db")]);
        console
    }

    /// Type `text` as if in the composer (starting from normal mode).
    fn type_text(c: &mut Console, text: &str) {
        if c.mode() == Mode::Normal {
            c.handle_key(ch('i'));
        }
        for character in text.chars() {
            c.handle_key(ch(character));
        }
    }

    // -- navigation ----------------------------------------------------------

    #[test]
    fn the_dashboard_selects_between_the_orchestrator_and_workers() {
        let mut c = setup_with_worker();
        assert_eq!(c.selected(), 0, "the orchestrator first");
        c.handle_key(ch('j'));
        assert_eq!(c.selected(), 1);
        c.handle_key(ch('j'));
        assert_eq!(c.selected(), 0, "wraps around");
        c.handle_key(ch('k'));
        assert_eq!(c.selected(), 1);
        c.handle_key(ch('G'));
        assert_eq!(c.selected(), 1);
        c.handle_key(ch('g'));
        assert_eq!(c.selected(), 0);
        c.handle_key(key(KeyCode::Down));
        assert_eq!(c.selected(), 1);
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.selected(), 0);
        c.handle_key(ch('2'));
        assert_eq!(c.selected(), 1, "1-9 jumps to the nth session");
        c.handle_key(ch('9'));
        assert_eq!(c.selected(), 1, "out of range jumps nowhere");
    }

    // -- selection and diff stats --------------------------------------------

    #[test]
    fn select_target_picks_a_row_by_key_and_refuses_unknown_keys() {
        let mut c = test_console();
        c.set_runs(vec![running_run("auth-20260830000000", "auth")]);
        assert_eq!(c.selected(), 0, "the orchestrator starts selected");
        assert!(c.select_target("auth-20260830000000"));
        assert_eq!(c.selected(), 1);
        assert_eq!(
            c.selected_target(),
            SessionTarget::Worker {
                run_id: "auth-20260830000000".into()
            }
        );
        assert!(c.select_target("orchestrator"));
        assert_eq!(c.selected(), 0);
        // an unknown key leaves the selection alone: the caller falls back
        assert!(!c.select_target("ghost-20260830000000"));
        assert_eq!(c.selected(), 0);
    }

    #[test]
    fn diff_stats_show_on_a_row_and_clear_again() {
        let mut c = test_console();
        c.set_runs(vec![running_run("auth-20260830000000", "auth")]);
        c.set_diff_stat("auth-20260830000000", "+12 −3");
        assert_eq!(c.rows()[1].diff_stat.as_deref(), Some("+12 −3"));
        c.clear_diff_stat("auth-20260830000000");
        assert_eq!(c.rows()[1].diff_stat, None);
    }

    #[test]
    fn enter_opens_a_session_and_esc_returns_to_the_dashboard() {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        assert_eq!(c.view(), View::Dashboard);
        c.handle_key(enter());
        assert_eq!(c.view(), View::Session);
        c.handle_key(enter());
        assert_eq!(c.view(), View::Session, "enter does nothing in a session");
        c.handle_key(esc());
        assert_eq!(c.view(), View::Dashboard);
    }

    #[test]
    fn tab_cycles_sessions_in_both_views() {
        let mut c = setup_with_worker();
        c.handle_key(tab());
        assert_eq!(c.selected(), 1);
        c.handle_key(shift_tab());
        assert_eq!(c.selected(), 0);
        c.handle_key(enter());
        c.handle_key(tab());
        assert_eq!(c.view(), View::Session);
        assert_eq!(c.selected(), 1, "the drill-down follows the selection");
    }

    // -- modes ---------------------------------------------------------------

    #[test]
    fn i_enters_insert_and_esc_leaves() {
        let mut c = setup_with_worker();
        assert_eq!(c.mode(), Mode::Normal);
        c.handle_key(ch('i'));
        assert_eq!(c.mode(), Mode::Insert);
        c.handle_key(esc());
        assert_eq!(c.mode(), Mode::Normal);
    }

    #[test]
    fn typing_a_printable_in_normal_mode_enters_insert_keeping_the_char() {
        let mut c = setup_with_worker();
        c.handle_key(ch('h'));
        assert_eq!(c.mode(), Mode::Insert);
        assert_eq!(c.composer().input, "h");
        c.handle_key(ch('e'));
        assert_eq!(c.composer().input, "he");
        // and 'q' inside insert mode does not quit
        c.handle_key(ch('q'));
        assert_eq!(c.composer().input, "heq");
        c.handle_key(enter());
        // it went to the orchestrator as a message, not as a quit
        assert!(
            c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.text == "> heq")
        );
    }

    #[test]
    fn insert_mode_edits_the_composer() {
        let mut c = setup_with_worker();
        type_text(&mut c, "abc");
        assert_eq!(c.composer().input, "abc");
        c.handle_key(key(KeyCode::Left));
        c.handle_key(ch('X'));
        assert_eq!(c.composer().input, "abXc");
        c.handle_key(key(KeyCode::Backspace));
        assert_eq!(c.composer().input, "abc");
        c.handle_key(key(KeyCode::Home));
        c.handle_key(ch('-'));
        assert_eq!(c.composer().input, "-abc");
        c.handle_key(key(KeyCode::End));
        c.handle_key(ch('!'));
        assert_eq!(c.composer().input, "-abc!");
        c.handle_key(key(KeyCode::Delete));
        assert_eq!(c.composer().input, "-abc!");
    }

    // -- sending -------------------------------------------------------------

    #[test]
    fn enter_sends_an_orchestrator_message() {
        let mut c = setup_with_worker();
        type_text(&mut c, "hi");
        let effects = c.handle_key(enter());
        assert_eq!(effects, vec![Effect::SendToOrchestrator("hi".to_string())]);
        assert_eq!(c.composer().input, "");
        assert!(
            c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.text == "> hi")
        );
    }

    #[test]
    fn text_steers_the_selected_worker() {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        type_text(&mut c, "hi");
        let effects = c.handle_key(enter());
        assert_eq!(
            effects,
            vec![Effect::WorkerSteer {
                run_id: "db-20260829120000".to_string(),
                message: "hi".to_string(),
            }]
        );
    }

    #[test]
    fn a_modified_enter_inserts_a_newline_instead_of_sending() {
        for modifier in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut c = setup_with_worker();
            c.handle_key(ch('i'));
            c.handle_key(ch('a'));
            c.handle_key(KeyEvent::new(KeyCode::Enter, modifier));
            c.handle_key(ch('b'));
            let effects = c.handle_key(enter());
            assert_eq!(
                effects,
                vec![Effect::SendToOrchestrator("a\nb".to_string())],
                "{modifier:?}+enter"
            );
        }
    }

    #[test]
    fn an_unknown_command_is_caught_not_sent() {
        let mut c = setup_with_worker();
        c.submit("/pemissions auto");
        assert!(
            c.flash()
                .unwrap()
                .text
                .contains("unknown command /pemissions")
        );
    }

    #[test]
    fn a_mistyped_command_gets_a_did_you_mean() {
        assert_eq!(
            suggest_command("/pemissions", &["/permissions".to_string()]),
            Some("/permissions".to_string())
        );
        assert_eq!(
            suggest_command("/zzzzzz", &["/permissions".to_string()]),
            None
        );
    }

    #[test]
    fn an_agent_command_the_orchestrator_offers_goes_verbatim() {
        let mut c = setup_with_worker();
        c.set_capabilities(Capabilities {
            commands: vec![AgentCommand {
                name: "usage".into(),
                description: Some("Show usage".into()),
                argument_hint: None,
                aliases: None,
            }],
            ..Capabilities::default()
        });
        let effects = c.submit("/usage");
        assert_eq!(
            effects,
            vec![Effect::SendToOrchestrator("/usage".to_string())]
        );
    }

    #[test]
    fn a_worker_command_goes_as_a_command_envelope() {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.commands = vec![WorkerCommand {
            name: "skill:review".into(),
            description: "Review the diff".into(),
            source: "skill".into(),
        }];
        c.set_runs(vec![entry]);
        c.handle_key(ch('j'));
        let effects = c.submit("/skill:review");
        assert_eq!(
            effects,
            vec![Effect::WorkerCommand {
                run_id: "db-20260829120000".to_string(),
                message: "/skill:review".to_string(),
            }]
        );
    }

    // -- completions ---------------------------------------------------------

    #[test]
    fn slash_offers_console_then_agent_commands_and_tab_accepts() {
        let mut c = setup_with_worker();
        c.set_capabilities(Capabilities {
            commands: vec![AgentCommand {
                name: "usage".into(),
                description: None,
                argument_hint: None,
                aliases: None,
            }],
            ..Capabilities::default()
        });
        c.handle_key(ch('i'));
        c.handle_key(ch('/'));
        let completion = c.composer().completion.as_ref().unwrap();
        let labels: Vec<&str> = completion.items.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"/help"));
        assert!(!labels.contains(&"/answer"), "worker-only commands hide");
        assert!(labels.contains(&"/usage"), "the agent's own ride along");
        // tab accepts the highlighted suggestion
        c.handle_key(ch('q'));
        c.handle_key(tab());
        assert_eq!(c.composer().input, "/quit", "/quit takes no argument");
        // accepting does not run it
        assert_eq!(c.mode(), Mode::Insert);
    }

    #[test]
    fn at_offers_workers_then_files() {
        let mut c = setup_with_worker();
        c.set_files(vec!["src/main.rs".into()]);
        c.handle_key(ch('@'));
        let completion = c.composer().completion.as_ref().unwrap();
        assert_eq!(completion.items[0].label, "@db");
        c.handle_key(ch('d'));
        c.handle_key(ch('b'));
        c.handle_key(tab());
        assert_eq!(c.composer().input, "@db");
    }

    #[test]
    fn up_recalls_what_you_sent_when_no_completions_are_open() {
        let mut c = setup_with_worker();
        c.submit("first message");
        c.submit("second message");
        type_text(&mut c, "x");
        // still in insert mode: up recalls, it does not move the selection
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.composer().input, "second message");
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.composer().input, "first message");
        c.handle_key(key(KeyCode::Down));
        assert_eq!(c.composer().input, "second message");
    }

    // -- the palette ---------------------------------------------------------

    #[test]
    fn ctrl_k_opens_the_palette_over_everything() {
        let mut c = setup_with_worker();
        c.set_capabilities(Capabilities {
            commands: vec![AgentCommand {
                name: "usage".into(),
                description: None,
                argument_hint: None,
                aliases: None,
            }],
            mcp_servers: vec![McpServerStatus {
                name: "fleet".into(),
                status: "connected".into(),
            }],
            tools: vec!["Bash".into(), "mcp__fleet__fleet_spawn".into()],
            ..Capabilities::default()
        });
        c.handle_key(ctrl('k'));
        let Overlay::Palette(palette) = c.overlay().unwrap() else {
            panic!("palette should be open");
        };
        let has_group = |label: &str| {
            palette.items.iter().any(|item| match &item.group {
                crate::tui::palette::PaletteGroup::Console => label == "console",
                crate::tui::palette::PaletteGroup::Agent { .. } => label == "agent",
                crate::tui::palette::PaletteGroup::Servers => label == "servers",
                crate::tui::palette::PaletteGroup::Models => label == "models",
                crate::tui::palette::PaletteGroup::Sessions => label == "sessions",
            })
        };
        for group in ["console", "agent", "servers", "models", "sessions"] {
            assert!(has_group(group), "{group} group missing");
        }
        // the mcp tool landed, and the sessions are jumpable
        assert!(
            palette
                .items
                .iter()
                .any(|i| i.label == "mcp__fleet__fleet_spawn")
        );
        assert!(palette.items.iter().any(|i| i.label == "db"));
        // fuzzy narrowing works
        let mut narrowed = palette.clone();
        narrowed.query = "usg".into();
        narrowed.refilter();
        assert!(
            narrowed
                .visible
                .iter()
                .any(|i| narrowed.items[*i].label == "/usage")
        );
    }

    #[test]
    fn the_palette_runs_a_console_command_and_jumps_sessions() {
        let mut c = setup_with_worker();
        c.handle_key(ctrl('k'));
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "/help".into();
        palette.refilter();
        let effects = c.handle_palette(palette, KeyAction::Send);
        assert!(effects.is_empty());
        assert!(matches!(c.overlay(), Some(Overlay::Help)));
        c.handle_key(esc());

        // jump to the worker session
        c.handle_key(ctrl('k'));
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "db".into();
        palette.refilter();
        c.handle_palette(palette, KeyAction::Send);
        assert_eq!(c.selected(), 1, "the jump landed on the worker row");
    }

    #[test]
    fn the_palette_prefills_commands_that_take_an_argument() {
        let mut c = setup_with_worker();
        c.handle_key(ctrl('k'));
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "/thinking".into();
        palette.refilter();
        c.handle_palette(palette, KeyAction::Send);
        assert_eq!(c.mode(), Mode::Insert);
        assert_eq!(c.composer().input, "/thinking ");
    }

    #[test]
    fn m_opens_the_palette_over_models_only() {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.available_models = vec![WorkerModel {
            provider: "anthropic".into(),
            id: "claude-opus-5".into(),
            name: Some("Opus".into()),
        }];
        c.set_runs(vec![entry]);
        c.handle_key(ch('j'));
        c.handle_key(ch('m'));
        let Overlay::Palette(palette) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(palette.scope, PaletteScope::Models);
        assert!(
            palette
                .items
                .iter()
                .all(|i| matches!(i.action, PaletteAction::Model { .. }))
        );
        // choosing one switches the worker's model
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "opus".into();
        palette.refilter();
        let effects = c.handle_palette(palette, KeyAction::Send);
        assert_eq!(
            effects,
            vec![Effect::WorkerModel {
                run_id: "db-20260829120000".to_string(),
                model_id: "claude-opus-5".to_string(),
                provider: Some("anthropic".to_string()),
            }]
        );
    }

    #[test]
    fn choosing_a_model_on_the_orchestrator_targets_the_orchestrator() {
        let mut c = setup_with_worker();
        c.handle_key(ch('m'));
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "fable".into();
        palette.refilter();
        let effects = c.handle_palette(palette, KeyAction::Send);
        assert_eq!(
            effects,
            vec![Effect::SetOrchestratorModel("fable".to_string())]
        );
    }

    // -- normal-mode session actions ------------------------------------------

    #[test]
    fn a_answers_a_pending_question() {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.pending_question = Some(PendingQuestion {
            id: "q_1".into(),
            question: "bcrypt or argon2?".into(),
            options: None,
            context: None,
            asked_at: crate::util::now_iso(),
        });
        c.set_runs(vec![entry]);
        c.handle_key(ch('j'));
        c.handle_key(ch('a'));
        assert_eq!(c.mode(), Mode::Insert);
        let answering = c.composer().answering.as_ref().unwrap();
        assert_eq!(answering.question_id, "q_1");
        type_text(&mut c, "use");
        let effects = c.handle_key(enter());
        assert_eq!(
            effects,
            vec![Effect::WorkerAnswer {
                run_id: "db-20260829120000".to_string(),
                question_id: Some("q_1".to_string()),
                message: "use".to_string(),
            }]
        );
        assert!(c.composer().answering.is_none(), "the answer is consumed");
    }

    #[test]
    fn a_answers_a_pending_dialog_too() {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.pending_dialog = Some(PendingDialog {
            id: "u-1".into(),
            method: "select".into(),
            question: "Pick one".into(),
            options: Some(vec!["yes".into(), "no".into()]),
            context: None,
            asked_at: crate::util::now_iso(),
        });
        c.set_runs(vec![entry]);
        c.handle_key(ch('j'));
        c.handle_key(ch('a'));
        // a select dialog is prefilled with its first option
        assert_eq!(c.composer().input, "yes");
        let answering = c.composer().answering.as_ref().unwrap();
        assert_eq!(answering.question_id, "u-1");
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::WorkerAnswer { message, .. } if message == "yes"
        ));
    }

    #[test]
    fn a_without_anything_pending_says_so() {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        c.handle_key(ch('a'));
        assert!(c.flash().unwrap().text.contains("no pending question"));
        assert_eq!(c.mode(), Mode::Normal);
    }

    #[test]
    fn s_stops_the_worker_and_interrupts_the_orchestrator() {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        let effects = c.handle_key(ch('s'));
        assert_eq!(
            effects,
            vec![Effect::WorkerAbort {
                run_id: "db-20260829120000".to_string(),
            }]
        );
        // the orchestrator: interrupt only when a turn is active
        c.handle_key(ch('g'));
        let effects = c.handle_key(ch('s'));
        assert!(
            effects.is_empty(),
            "an idle orchestrator has nothing to stop"
        );
        c.orch_transcript.push_sent("hello");
        let effects = c.handle_key(ch('s'));
        assert_eq!(effects, vec![Effect::Interrupt]);
    }

    #[test]
    fn x_asks_before_removing() {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        c.handle_key(ch('x'));
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(confirm.message.contains("Abort it and remove"));
        // n cancels; nothing was removed
        c.handle_key(ch('n'));
        assert!(c.overlay().is_none());
        // y removes with force (the worker is running)
        c.handle_key(ch('x'));
        let effects = c.handle_key(ch('y'));
        assert_eq!(
            effects,
            vec![Effect::RemoveWorker {
                run_id: "db-20260829120000".to_string(),
                force: true,
            }]
        );
    }

    #[test]
    fn t_cycles_the_thinking_level_of_whichever_session_is_selected() {
        let mut c = setup_with_worker();
        // the orchestrator cycles claude's effort
        let effects = c.handle_key(ch('t'));
        assert_eq!(effects, vec![Effect::SetEffort("low".to_string())]);
        assert_eq!(c.effort(), Some("low"), "optimistic until state confirms");
        // the worker cycles pi's from the level it reports
        c.handle_key(ch('j'));
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.thinking_level = Some("high".into());
        c.set_runs(vec![entry]);
        let effects = c.handle_key(ch('t'));
        assert_eq!(
            effects,
            vec![Effect::WorkerThinking {
                run_id: "db-20260829120000".to_string(),
                level: "xhigh".to_string(),
            }]
        );
        // and the next press advances from the optimistically written level,
        // even though the monitor has not written it back into run.json yet
        let effects = c.handle_key(ch('t'));
        assert!(matches!(
            &effects[0],
            Effect::WorkerThinking { level, .. } if level == "max"
        ));
    }

    #[test]
    fn v_hands_the_mouse_to_the_terminal_and_takes_it_back() {
        let mut c = test_console();
        assert!(c.mouse_captured(), "the console owns the wheel by default");

        let effects = c.handle_key(ch('v'));
        assert_eq!(effects, vec![Effect::SetMouseCapture(false)]);
        assert!(!c.mouse_captured());
        assert!(
            c.flash().is_some_and(|f| f.text.contains("select")),
            "it says what just happened"
        );

        let effects = c.handle_key(ch('v'));
        assert_eq!(effects, vec![Effect::SetMouseCapture(true)]);
        assert!(c.mouse_captured());

        // and the command reaches the same place, for the palette
        c.mode = Mode::Insert;
        c.composer.input = "/mouse".to_string();
        c.composer.cursor = 6;
        let effects = c.handle_key(enter());
        assert_eq!(effects, vec![Effect::SetMouseCapture(false)]);
        assert!(!c.mouse_captured());
    }

    #[test]
    fn t_cycles_a_worker_thinking_level_without_the_monitor_writeback() {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.thinking_level = Some("high".into());
        c.set_runs(vec![entry]);

        // the monitor never writes the applied level back into run.json: the
        // polled state still says "high", yet the press advances anyway
        let first = c.handle_key(ch('t'));
        assert!(matches!(
            &first[0],
            Effect::WorkerThinking { level, .. } if level == "xhigh"
        ));
        let second = c.handle_key(ch('t'));
        assert!(
            matches!(
                &second[0],
                Effect::WorkerThinking { level, .. } if level == "max"
            ),
            "the second press must advance from the optimistic level: {second:?}"
        );

        // a re-poll with the stale state folds the optimistic level into the
        // view, so the statusline shows it; a poll that catches up forgets it
        let mut stale = running_run("db-20260829120000", "db");
        stale.state.thinking_level = Some("high".into());
        c.set_runs(vec![stale]);
        assert_eq!(
            c.runs[0].state.thinking_level.as_deref(),
            Some("max"),
            "the statusline reads the optimistic level until the state catches up"
        );
        let mut caught_up = running_run("db-20260829120000", "db");
        caught_up.state.thinking_level = Some("max".into());
        c.set_runs(vec![caught_up]);
        assert_eq!(
            c.pending_thinking.len(),
            0,
            "the monitor owns the level now"
        );
        let next = c.handle_key(ch('t'));
        assert!(
            matches!(
                &next[0],
                Effect::WorkerThinking { level, .. } if level == "off"
            ),
            "max wraps to off: {next:?}"
        );
    }

    #[test]
    fn p_cycles_the_permission_mode_orchestrator_only() {
        let mut c = setup_with_worker();
        let effects = c.handle_key(ch('p'));
        assert_eq!(effects, vec![Effect::SetPermissionMode("auto".to_string())]);
        // and refuses on a worker
        c.handle_key(ch('j'));
        c.handle_key(ch('p'));
        assert!(c.flash().unwrap().text.contains("orchestrator-only"));
    }

    #[test]
    fn b_opens_the_full_brief_popup_and_scrolls_it() {
        let mut c = setup_with_worker();
        // the orchestrator's brief is the rendered prompt; none on a fresh
        // fleet, so the popup says so dimmed instead of erroring
        c.handle_key(ch('b'));
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!("expected the brief overlay");
        };
        assert!(state.placeholder, "no prompt.md yet on a fresh fleet");
        c.handle_key(esc());
        assert!(c.overlay().is_none(), "esc closes the brief");

        // a worker's brief is its taskBrief
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.task_brief = "Build the auth module.\n\nDo not touch tests.".into();
        c.set_runs(vec![entry]);
        c.handle_key(ch('j'));
        c.handle_key(ch('b'));
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(
            state.text, "Build the auth module.\n\nDo not touch tests.",
            "the full brief, not the one-line transcript summary"
        );
        assert!(!state.placeholder);

        // the wheel routes here as the scroll actions, half a viewport each
        let offset = |c: &Console| {
            let Overlay::Brief(state) = c.overlay().unwrap() else {
                panic!();
            };
            state.offset
        };
        c.handle_action(KeyAction::ScrollHalfDown);
        c.handle_action(KeyAction::ScrollHalfDown);
        assert_eq!(offset(&c), 20, "two notches of the 20-row viewport");
        c.handle_action(KeyAction::ScrollHalfUp);
        assert_eq!(offset(&c), 10);
        c.handle_action(KeyAction::Back);
        assert!(c.overlay().is_none());

        // with a rendered prompt on disk, the orchestrator shows it
        std::fs::create_dir_all(c.fleet.orchestrator_dir(&c.orch_key)).unwrap();
        std::fs::write(
            c.fleet.orchestrator_prompt(&c.orch_key),
            "You are the orchestrator.",
        )
        .unwrap();
        c.handle_key(ch('k'));
        c.handle_key(ch('b'));
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(state.text, "You are the orchestrator.");
        assert!(!state.placeholder);
        c.handle_action(KeyAction::Back);

        // a worker whose record is gone (or brief empty) reads as a placeholder
        let mut empty = running_run("db-20260829120000", "db");
        empty.state.task_brief = String::new();
        c.set_runs(vec![empty]);
        c.handle_key(ch('j'));
        let effects = c.handle_key(ch('b'));
        assert!(effects.is_empty());
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(state.placeholder, "an empty brief is not a brief");
    }

    #[test]
    fn q_quits_and_q_upper_asks_before_shutdown() {
        let mut c = setup_with_worker();
        let effects = c.handle_key(ch('q'));
        assert_eq!(effects, vec![Effect::Quit]);
        let effects = c.handle_key(ch('Q'));
        assert!(effects.is_empty(), "shutdown waits for confirmation");
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(
            confirm
                .message
                .contains("Stop the orchestrator and 1 running worker")
        );
        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::WorkerAbort {
            run_id: "db-20260829120000".to_string(),
        }));
        assert!(effects.contains(&Effect::StopOrchestrator));
        assert!(effects.contains(&Effect::Quit));
        // n cancels
        c.handle_key(ch('Q'));
        let effects = c.handle_key(ch('n'));
        assert!(effects.is_empty());
        assert!(c.overlay().is_none());
    }

    // -- scrolling and search --------------------------------------------------

    #[test]
    fn the_session_view_scrolls() {
        let mut c = setup_with_worker();
        for i in 0..40 {
            c.orch_transcript.push_notice(&format!("line {i}"));
        }
        c.handle_key(enter());
        assert_eq!(c.scroll(), None, "the tail is followed by default");
        c.handle_key(ch('g'));
        assert_eq!(c.scroll(), Some(0), "g pins the top");
        c.handle_key(ctrl('d'));
        assert_eq!(c.scroll(), Some(10), "half of the 20-row viewport");
        c.handle_key(ctrl('f'));
        assert_eq!(c.scroll(), Some(30));
        c.handle_key(ctrl('u'));
        assert_eq!(c.scroll(), Some(20));
        c.handle_key(ctrl('b'));
        assert_eq!(c.scroll(), Some(0));
        c.handle_key(ch('G'));
        assert_eq!(c.scroll(), None, "G follows the tail again");
        // on the dashboard these keys select instead
        c.handle_key(esc());
        c.handle_key(ch('g'));
        assert_eq!(c.selected(), 0);
        c.handle_key(ch('G'));
        assert_eq!(c.selected(), 1);
    }

    #[test]
    fn search_finds_and_navigates_matches() {
        let mut c = setup_with_worker();
        c.orch_transcript.push_notice("the quick brown fox");
        c.orch_transcript.push_notice("lazy dog");
        c.orch_transcript.push_notice("another quick fox");
        c.handle_key(enter());
        c.handle_key(ch('/'));
        let Overlay::Search(search) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(search.matches.is_empty());
        c.handle_key(ch('q'));
        c.handle_key(ch('u'));
        let Overlay::Search(search) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(search.matches.len(), 2, "live matches as you type");
        c.handle_key(enter());
        assert!(c.overlay().is_none());
        assert_eq!(c.search().unwrap().current, Some(0));
        c.handle_key(ch('n'));
        assert_eq!(c.search().unwrap().current, Some(1));
        c.handle_key(ch('n'));
        assert_eq!(c.search().unwrap().current, Some(0), "wraps");
        c.handle_key(ch('N'));
        assert_eq!(c.search().unwrap().current, Some(1));
        // and the view is pinned at the match
        assert!(c.scroll().is_some());
    }

    // -- overlays --------------------------------------------------------------

    #[test]
    fn help_opens_and_closes() {
        let mut c = setup_with_worker();
        c.handle_key(ch('?'));
        assert!(matches!(c.overlay(), Some(Overlay::Help)));
        c.handle_key(esc());
        assert!(c.overlay().is_none());
    }

    #[test]
    fn the_permission_overlay_allows_denies_and_answers() {
        let mut c = setup_with_worker();
        let request = PermissionRequest {
            request_id: "req_1".into(),
            request: CanUseToolRequest {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "touch a.txt"}),
                tool_use_id: "t1".into(),
                title: Some("Run touch a.txt".into()),
                permission_suggestions: vec![serde_json::json!({"type": "addRules"})],
                ..CanUseToolRequest::default()
            },
            received_at: crate::util::now_iso(),
        };
        let state = OrchestratorState {
            pending_requests: vec![request],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        c.handle_key(ch('a')); // 'a' with approvals pending opens the overlay
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        let effects = c.handle_key(ch('y'));
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Allow { .. },
                ..
            }
        ));
        // deny with a reason
        let state = OrchestratorState {
            pending_requests: vec![PermissionRequest {
                request_id: "req_2".into(),
                request: CanUseToolRequest {
                    tool_name: "Bash".into(),
                    ..CanUseToolRequest::default()
                },
                received_at: crate::util::now_iso(),
            }],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        c.handle_key(ch('a'));
        c.handle_key(ch('n')); // start the deny reason
        c.handle_key(ch('n'));
        c.handle_key(ch('o'));
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Deny { message },
                ..
            } if message == "no"
        ));
    }

    #[test]
    fn an_ask_user_question_is_answered_from_its_options_or_in_your_own_words() {
        let mut c = setup_with_worker();
        let make_request = |id: &str, options: Value| PermissionRequest {
            request_id: id.into(),
            request: CanUseToolRequest {
                tool_name: "AskUserQuestion".into(),
                input: serde_json::json!({"questions": [
                    {"question": "Which hash?", "options": options},
                ]}),
                tool_use_id: "t2".into(),
                ..CanUseToolRequest::default()
            },
            received_at: crate::util::now_iso(),
        };
        let state = OrchestratorState {
            pending_requests: vec![make_request(
                "req_3",
                serde_json::json!([{"label": "bcrypt"}, {"label": "argon2"}]),
            )],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        c.handle_key(ch('a'));
        // the first option is highlighted; down moves, enter answers
        c.handle_key(key(KeyCode::Down));
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Answer { answers },
                ..
            } if answers["Which hash?"] == "argon2"
        ));
        // and a custom answer
        let state = OrchestratorState {
            pending_requests: vec![make_request(
                "req_4",
                serde_json::json!([{"label": "bcrypt"}]),
            )],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        c.handle_key(ch('a'));
        c.handle_key(key(KeyCode::Down)); // onto "something else"
        c.handle_key(enter()); // start typing
        c.handle_key(ch('s'));
        c.handle_key(ch('c'));
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Answer { answers },
                ..
            } if answers["Which hash?"] == "sc"
        ));
    }

    // -- orchestrator settings -------------------------------------------------

    #[test]
    fn slash_thinking_validates_and_sets_effort() {
        let mut c = setup_with_worker();
        let effects = c.submit("/thinking nonsense");
        assert!(c.flash().unwrap().text.contains("usage: /thinking"));
        assert!(effects.is_empty());
        let effects = c.submit("/thinking high");
        assert_eq!(effects, vec![Effect::SetEffort("high".to_string())]);
        // a settings change is a passing note, not a message
        assert!(
            !c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.kind == crate::tui::transcript::BlockKind::User)
        );
    }

    #[test]
    fn slash_model_switches_either_side() {
        let mut c = setup_with_worker();
        let effects = c.submit("/model fable");
        assert_eq!(
            effects,
            vec![Effect::SetOrchestratorModel("fable".to_string())]
        );
        c.handle_key(ch('j'));
        let effects = c.submit("/model claude-opus-5");
        assert_eq!(
            effects,
            vec![Effect::WorkerModel {
                run_id: "db-20260829120000".to_string(),
                model_id: "claude-opus-5".to_string(),
                provider: None,
            }]
        );
    }

    #[test]
    fn slash_permissions_reports_refuses_and_sets() {
        let mut c = setup_with_worker();
        c.submit("/permissions");
        assert!(c.flash().unwrap().text.contains("permissions: default"));
        c.submit("/permissions bypassPermissions");
        assert!(c.flash().unwrap().text.contains("not offered"));
        let effects = c.submit("/permissions auto");
        assert_eq!(effects, vec![Effect::SetPermissionMode("auto".to_string())]);
    }

    #[test]
    fn slash_rail_is_remembered() {
        let mut c = setup_with_worker();
        let effects = c.submit("/rail wide");
        assert_eq!(effects, vec![Effect::SavePrefs]);
        assert_eq!(c.prefs().rail_mode, "wide");
        c.submit("/rail nonsense");
        assert!(c.flash().unwrap().text.contains("usage: /rail"));
    }

    #[test]
    fn slash_shutdown_asks_first() {
        let mut c = setup_with_worker();
        let effects = c.submit("/shutdown");
        assert!(effects.is_empty(), "waits for confirmation");
        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::Quit));
    }

    #[test]
    fn slash_answer_targets_the_pending_question() {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.pending_question = Some(PendingQuestion {
            id: "q_1".into(),
            question: "which?".into(),
            options: None,
            context: None,
            asked_at: crate::util::now_iso(),
        });
        c.set_runs(vec![entry]);
        c.handle_key(ch('j'));
        let effects = c.submit("/answer use argon2");
        assert_eq!(
            effects,
            vec![Effect::WorkerAnswer {
                run_id: "db-20260829120000".to_string(),
                question_id: Some("q_1".to_string()),
                message: "use argon2".to_string(),
            }]
        );
    }

    #[test]
    fn a_finished_worker_refuses_steering_with_a_resume_hint() {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.status = RunStatus::Settled;
        entry.state.pid = None;
        c.set_runs(vec![entry]);
        c.handle_key(ch('j'));
        c.submit("hello");
        assert!(c.flash().unwrap().text.contains("is settled"));
    }

    #[test]
    fn slash_quit_and_slash_help_work_over_a_worker_too() {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        let effects = c.submit("/quit");
        assert_eq!(effects, vec![Effect::Quit]);
        let effects = c.submit("/help");
        assert!(effects.is_empty());
        assert!(matches!(c.overlay(), Some(Overlay::Help)));
    }

    // -- feeds ------------------------------------------------------------------

    #[test]
    fn fleet_events_are_forwarded_to_the_orchestrator() {
        let mut c = setup_with_worker();
        let events = vec![crate::fleet::event::FleetEvent::new(
            crate::fleet::event::FleetEventKind::Question,
            "db-20260829120000",
            "db",
            vec![],
        )];
        let effects = c.ingest_fleet_events(&events, "BATCH");
        assert_eq!(
            effects,
            vec![Effect::SendToOrchestrator("BATCH".to_string())]
        );
        assert!(
            c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.text.contains("⚑ question db"))
        );
    }

    #[test]
    fn the_flash_expires_after_a_few_seconds() {
        let mut c = setup_with_worker();
        c.toast("hello", false);
        let at = c.flash().unwrap().at;
        c.tick(at + FLASH_MS + 1);
        assert!(c.flash().is_none());
        c.toast("hello", false);
        let at = c.flash().unwrap().at;
        c.tick(at + 100);
        assert!(c.flash().is_some());
    }

    #[test]
    fn the_next_level_wraps_and_unknowns_start_at_the_front() {
        assert_eq!(next_level(&["a", "b"], Some("a")), "b");
        assert_eq!(next_level(&["a", "b"], Some("b")), "a");
        assert_eq!(next_level(&["a", "b"], None), "a");
        assert_eq!(next_level(&["a", "b"], Some("zzz")), "a");
    }

    #[test]
    fn parse_answer_takes_the_id_when_it_leads() {
        let pending = PendingQuestion {
            id: "q_9".into(),
            question: String::new(),
            options: None,
            context: None,
            asked_at: String::new(),
        };
        let (id, message) = parse_answer("q_3 use argon2", Some(&pending));
        assert_eq!(id.as_deref(), Some("q_3"));
        assert_eq!(message, "use argon2");
        let (id, message) = parse_answer("use argon2", Some(&pending));
        assert_eq!(id.as_deref(), Some("q_9"), "the pending id fills in");
        assert_eq!(message, "use argon2");
        let (id, message) = parse_answer("q_3", Some(&pending));
        assert_eq!(id.as_deref(), Some("q_9"), "an id alone is not an answer");
        assert_eq!(message, "q_3");
        let (id, _) = parse_answer("u-1 yes", None);
        assert_eq!(id.as_deref(), Some("u-1"), "dialog ids parse too");
    }

    #[test]
    fn edit_distance_matches_the_reference() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
    }

    // -- sessions -----------------------------------------------------------

    /// Persist sessions into the console's fleet, the way the orchestrator
    /// slice's store does.
    fn write_sessions(c: &Console, sessions: Vec<OrchestratorSession>) {
        let mut store = crate::orch::session::FleetSessions::new();
        for session in sessions {
            store.upsert(session);
        }
        crate::orch::session::save(c.fleet.root(), &mut store).unwrap();
    }

    /// Persist a running run owned by `owner` into the console's fleet.
    fn write_run(c: &Console, run_id: &str, name: &str, owner: Uuid) {
        let mut entry = running_run(run_id, name);
        entry.state.orchestrator_id = Some(owner);
        let dir = c.fleet.run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        crate::fleet::run::save_state(&dir, &entry.state).unwrap();
    }

    #[test]
    fn selected_target_and_rows_carry_the_served_session_uuid() {
        let mut c = test_console();
        let key = SessionKey::new(Some("docs".into()), Uuid::new_v4());
        c.orch_key = key.clone();
        c.set_runs(vec![running_run("auth-20260830000000", "auth")]);
        assert_eq!(
            c.rows()[0].target,
            SessionTarget::Orchestrator(key.uuid),
            "the orchestrator row names its session"
        );
        assert_eq!(c.rows()[0].name, "orchestrator · docs");
        assert_eq!(c.selected_target(), SessionTarget::Orchestrator(key.uuid));
        assert_eq!(c.selected_target().key(), "orchestrator");

        // an alias-less session says what it is rather than showing a hex
        // prefix nobody can read, and the composer prompt follows
        c.orch_key = SessionKey::new(None, Uuid::new_v4());
        c.set_runs(Vec::new());
        assert_eq!(c.rows()[0].name, "orchestrator");
        assert_eq!(c.composer_prompt(), "orchestrator > ");
        // and so does the legacy default session
        c.orch_key = SessionKey::default();
        c.set_runs(Vec::new());
        assert_eq!(c.rows()[0].name, "orchestrator");
        assert_eq!(c.composer_prompt(), "orchestrator > ");
    }

    #[test]
    fn sessions_with_no_rows_says_so() {
        let mut c = test_console();
        let effects = c.submit("/sessions");
        assert!(effects.is_empty());
        assert!(c.flash().unwrap().text.contains("no sessions yet"));
    }

    #[test]
    fn sessions_lists_alias_short_uuid_workers_and_health() {
        use time::{Duration, OffsetDateTime};

        let mut c = test_console();
        let mut running = OrchestratorSession::new("/repo");
        running.alias = Some("add-auth".into());
        running.pid = Some(std::process::id() as i32); // live monitor
        running.last_heartbeat = Some(crate::util::now_iso());
        let mut wedged = OrchestratorSession::new("/repo");
        wedged.alias = Some("stale".into());
        wedged.pid = Some(std::process::id() as i32); // alive, but not stamping
        wedged.last_heartbeat = Some(crate::util::iso_at(
            OffsetDateTime::now_utc() - Duration::minutes(10),
        ));
        let stopped = OrchestratorSession::new("/repo"); // no pid, no heartbeat
        write_sessions(&c, vec![running.clone(), wedged.clone(), stopped.clone()]);
        write_run(&c, "auth-20260830000000", "auth", running.uuid);

        let effects = c.submit("/sessions");
        assert!(effects.is_empty());
        let text = c
            .orchestrator_transcript()
            .blocks()
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("add-auth"), "{text}");
        assert!(
            text.contains(&crate::util::short_uuid(&running.uuid)),
            "{text}"
        );
        assert!(text.contains("1 worker"), "{text}");
        assert!(text.contains("· running"), "{text}");
        assert!(text.contains("stale ("), "{text}");
        assert!(text.contains("· wedged"), "{text}");
        assert!(text.contains("· stopped"), "{text}");
        // the toolbar carries a tally, one line
        let flash = c.flash().unwrap();
        assert!(
            flash
                .text
                .contains("3 sessions · 1 running · 1 wedged · 1 stopped"),
            "{}",
            flash.text
        );
    }

    #[test]
    fn session_new_creates_a_session_and_switches_to_it() {
        let mut c = test_console();
        let effects = c.submit("/session new");
        assert_eq!(effects.len(), 2, "{effects:?}");
        assert!(matches!(&effects[0], Effect::SavePrefs));
        let Effect::SwitchSession(key) = &effects[1] else {
            panic!("expected a switch: {effects:?}");
        };
        assert!(
            key.alias.is_none(),
            "no alias: none until the orchestrator derives it"
        );
        assert!(!key.uuid.is_nil());
        let store = crate::orch::session::load(c.fleet.root()).unwrap();
        assert!(
            store.sessions.contains_key(&key.uuid),
            "the row is persisted"
        );
        assert_eq!(
            c.prefs().last_session_uuid.as_deref(),
            Some(key.uuid.to_string().as_str()),
            "the new session becomes the remembered one"
        );

        let effects = c.submit("/session new db-ops");
        let Effect::SwitchSession(key) = &effects[1] else {
            panic!("{effects:?}");
        };
        assert_eq!(key.alias.as_deref(), Some("db-ops"));
    }

    #[test]
    fn session_switch_resolves_uuid_then_alias_and_refuses_unknown() {
        let mut c = test_console();
        let session =
            crate::orch::session::create_session(c.fleet.root(), Some("add-auth")).unwrap();

        // by uuid
        let effects = c.submit(&format!("/session {}", session.uuid));
        assert!(matches!(
            &effects[1],
            Effect::SwitchSession(key) if key.uuid == session.uuid
        ));
        assert_eq!(
            c.prefs().last_session_uuid.as_deref(),
            Some(session.uuid.to_string().as_str())
        );
        // by alias
        let effects = c.submit("/session add-auth");
        assert!(matches!(
            &effects[1],
            Effect::SwitchSession(key) if key.uuid == session.uuid
        ));
        // switching to the session already served is a no-op
        c.orch_key = session.key();
        let effects = c.submit("/session add-auth");
        assert!(effects.is_empty());
        assert!(c.flash().unwrap().text.contains("already on this session"));
        // unknown keys are refused, not guessed
        let effects = c.submit("/session nope");
        assert!(effects.is_empty());
        assert!(
            c.flash().unwrap().text.contains("no orchestrator session"),
            "{}",
            c.flash().unwrap().text
        );
        // bare /session shows the usage
        let effects = c.submit("/session");
        assert!(effects.is_empty());
        assert!(c.flash().unwrap().text.contains("usage: /session"));
    }

    #[test]
    fn prefs_and_store_writes_interleave_without_losing_either() {
        let mut c = test_console();
        // a session row, as the monitor owns it
        let session = crate::orch::session::create_session(c.fleet.root(), Some("alpha")).unwrap();
        c.prefs.rail_mode = "wide".into();
        c.prefs.last_session_uuid = Some(session.uuid.to_string());

        // order 1: the store writes a heartbeat, then the console writes prefs
        crate::orch::session::touch_heartbeat(c.fleet.root(), session.uuid).unwrap();
        c.save_prefs();
        let raw = std::fs::read_to_string(c.fleet.fleet_json()).unwrap();
        assert!(raw.contains("\"railMode\": \"wide\""), "{raw}");
        let store = crate::orch::session::load(c.fleet.root()).unwrap();
        assert!(
            store.sessions[&session.uuid].last_heartbeat.is_some(),
            "the heartbeat survived the console's write"
        );
        assert!(
            store
                .extra
                .get("console")
                .and_then(|v| v.get("railMode"))
                .and_then(serde_json::Value::as_str)
                == Some("wide"),
            "the store itself round-trips the console key now"
        );

        // order 2: the console writes prefs, then the store writes a heartbeat
        c.save_prefs();
        crate::orch::session::touch_heartbeat(c.fleet.root(), session.uuid).unwrap();
        let raw = std::fs::read_to_string(c.fleet.fleet_json()).unwrap();
        assert!(raw.contains("\"railMode\": \"wide\""), "{raw}");
        assert!(
            raw.contains(&session.uuid.to_string()),
            "the session row survived the prefs write: {raw}"
        );
        let store = crate::orch::session::load(c.fleet.root()).unwrap();
        assert_eq!(store.sessions.len(), 1);
        assert!(
            store.sessions[&session.uuid].last_heartbeat.is_some(),
            "the heartbeat survived the second prefs write"
        );
    }

    #[test]
    fn load_prefs_migrates_the_legacy_session_uuid_and_keeps_row_and_uuid_separate() {
        let c = test_console();
        let uuid = Uuid::new_v4();
        // a pre-split console wrote the session uuid into lastSession
        std::fs::write(
            c.fleet.fleet_json(),
            json!({
                "version": 2,
                "sessions": {},
                "console": {"lastSession": uuid.to_string()},
            })
            .to_string(),
        )
        .unwrap();
        let mut reopened = Console::new(FleetPaths::new(c.fleet.root().to_path_buf()));
        reopened.load_prefs();
        assert_eq!(
            reopened.prefs().last_session_uuid.as_deref(),
            Some(uuid.to_string().as_str()),
            "the legacy uuid lands in the session field"
        );
        assert_eq!(reopened.prefs().last_session, None, "not doubled");

        // a row key is a row key, a uuid a uuid: the fields stay separate
        std::fs::write(
            c.fleet.fleet_json(),
            json!({
                "version": 2,
                "sessions": {},
                "console": {
                    "lastSession": "auth-20260830000000",
                    "lastSessionUuid": uuid.to_string(),
                },
            })
            .to_string(),
        )
        .unwrap();
        let mut reopened = Console::new(FleetPaths::new(c.fleet.root().to_path_buf()));
        reopened.load_prefs();
        assert_eq!(
            reopened.prefs().last_session.as_deref(),
            Some("auth-20260830000000")
        );
        assert_eq!(
            reopened.prefs().last_session_uuid.as_deref(),
            Some(uuid.to_string().as_str())
        );
        // and save_prefs persists both under the console key
        reopened.prefs.rail_mode = "compact".into();
        reopened.save_prefs();
        let raw = std::fs::read_to_string(c.fleet.fleet_json()).unwrap();
        assert!(raw.contains("\"lastSessionUuid\": \""), "{raw}");
        assert!(
            raw.contains("\"lastSession\": \"auth-20260830000000\""),
            "{raw}"
        );
        assert!(raw.contains("\"railMode\": \"compact\""), "{raw}");
    }

    #[test]
    fn an_ambiguous_alias_names_the_colliding_sessions() {
        let mut c = test_console();
        let first = crate::orch::session::create_session(c.fleet.root(), Some("dup")).unwrap();
        let second = crate::orch::session::create_session(c.fleet.root(), Some("dup")).unwrap();
        // resolving the alias must not pick silently: the error names both
        let effects = c.submit("/session dup");
        assert!(effects.is_empty());
        let flash = c.flash().unwrap();
        assert!(
            flash.text.contains("several live sessions"),
            "{}",
            flash.text
        );
        assert!(
            flash.text.contains(&crate::util::short_uuid(&first.uuid)),
            "{}",
            flash.text
        );
        assert!(
            flash.text.contains(&crate::util::short_uuid(&second.uuid)),
            "{}",
            flash.text
        );
        // the same honesty applies to /shutdown <key>
        let effects = c.submit("/shutdown dup");
        assert!(effects.is_empty());
        assert!(c.overlay().is_none());
        assert!(
            c.flash().unwrap().text.contains("several live sessions"),
            "{}",
            c.flash().unwrap().text
        );
    }

    #[test]
    fn shutdown_with_a_key_stops_only_that_session() {
        let mut c = test_console();
        let mut mine = OrchestratorSession::new("/repo");
        mine.alias = Some("mine".into());
        c.orch_key = mine.key();
        write_sessions(&c, vec![mine.clone()]);
        write_run(&c, "db-20260829120000", "db", mine.uuid);
        let other = crate::orch::session::create_session(c.fleet.root(), Some("docs")).unwrap();
        write_run(&c, "docs-20260829120001", "docs", other.uuid);
        c.set_runs(vec![running_run("db-20260829120000", "db")]);

        let effects = c.submit("/shutdown docs");
        assert!(effects.is_empty(), "waits for confirmation");
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!("expected a confirm");
        };
        assert!(confirm.message.contains("docs"), "{}", confirm.message);
        assert_eq!(confirm.action, ConfirmAction::ShutdownSession(other.key()));

        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::WorkerAbort {
            run_id: "docs-20260829120001".into(),
        }));
        assert!(effects.contains(&Effect::StopSession(other.key())));
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                Effect::WorkerAbort { run_id, .. } if run_id == "db-20260829120000"
            )),
            "the active session's workers are untouched: {effects:?}"
        );
        assert!(!effects.contains(&Effect::Quit), "the console stays open");
    }

    #[test]
    fn shutdown_naming_the_active_session_is_the_classic_shutdown() {
        let mut c = test_console();
        let active = crate::orch::session::create_session(c.fleet.root(), None).unwrap();
        c.orch_key = active.key();
        let _effects = c.submit(&format!("/shutdown {}", active.uuid));
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!("expected a confirm");
        };
        assert_eq!(confirm.action, ConfirmAction::Shutdown);
        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::StopOrchestrator));
        assert!(effects.contains(&Effect::Quit));
    }

    #[test]
    fn shutdown_with_an_unknown_key_is_refused() {
        let mut c = test_console();
        let effects = c.submit("/shutdown nobody");
        assert!(effects.is_empty());
        assert!(c.overlay().is_none());
        assert!(
            c.flash().unwrap().text.contains("no orchestrator session"),
            "{}",
            c.flash().unwrap().text
        );
    }

    #[test]
    fn begin_session_resets_console_state_for_the_new_session() {
        let mut c = setup_with_worker();
        c.orch_transcript.push_sent("hello");
        c.handle_key(enter()); // in the session view now
        c.toast("a note", false);
        let other = crate::orch::session::create_session(c.fleet.root(), Some("docs")).unwrap();
        c.begin_session(&other.key());
        assert_eq!(c.orch_key, other.key());
        assert_eq!(c.view(), View::Dashboard);
        assert!(
            c.runs.is_empty(),
            "the new session's runs arrive with the feeds"
        );
        assert!(c.rows().is_empty());
        assert!(c.worker_transcripts.is_empty());
        assert!(c.orch_transcript.blocks().is_empty());
        assert_eq!(c.selected(), 0);
        assert!(
            c.flash().is_none(),
            "the old session's notes do not carry over"
        );
    }

    #[test]
    fn is_terminal_view_covers_the_graveyard() {
        assert!(!is_terminal_view(DerivedView::Running));
        assert!(!is_terminal_view(DerivedView::Blocked));
        assert!(!is_terminal_view(DerivedView::Starting));
        assert!(is_terminal_view(DerivedView::Settled));
        assert!(is_terminal_view(DerivedView::Archived));
    }
}
