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

use crate::fleet::envelope::{Envelope, Party, append_envelope};
use crate::fleet::event::FleetEvent;
use crate::fleet::run::{DerivedView, RunState, THINKING_LEVELS, derive_view};
use crate::orch::records::{
    Capabilities, OrchestratorCommand, OrchestratorState, PermissionDecisionRecord,
};
use crate::paths::{FleetPaths, SessionKey};
use crate::tui::completions::{
    AgentCommandOption, CompletionState, CompletionTarget, apply_suggestion, completions_for,
};
use crate::tui::keys::{KeyAction, map_key, map_overlay_key};
use crate::tui::model::{
    DashboardRow, OrchSummary, RunRow, SessionTarget, activity_line, build_rows,
    session_display_name, session_label, worker_activity_line,
};
use crate::tui::palette::{
    McpServerInfo, PaletteContext, PaletteItem, PaletteScope, build_items, ranked,
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

/// Sent messages kept per session for `up`-recall.
const HISTORY_CAP: usize = 100;

/// An overlay on top of the conversation; the renderer draws whichever is
/// set. There is no second view: the console is one conversation, and the
/// fleet is something you open over it.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Help,
    /// Every session, and what can be done to the one selected.
    Fleet,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefs {
    /// The row that was open when the console last closed (`orchestrator`
    /// or a run id), restored within the opened session.
    pub last_session: Option<String>,
    /// The session that was open, by uuid: re-anchors the console on open,
    /// so a switch is remembered even when the row changed afterwards.
    pub last_session_uuid: Option<String>,
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
    /// Cut the orchestrator's transcript file down to its recent tail. The
    /// console notices the file shrank on its next poll and replays it.
    TrimTranscript,
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
    /// A block index into the open transcript, so it has to be rebased when
    /// blocks fall off the top — see [`Console::rebase_scroll`].
    scroll: Option<usize>,
    /// The open transcript's dropped count when `scroll` and the search hits
    /// were last in step with it.
    scroll_base: usize,
    /// The last search applied to the open session.
    search: Option<SearchState>,
    /// Permission requests already raised on their own, so dismissing one
    /// does not have it pop straight back up.
    raised_permissions: std::collections::HashSet<String>,
    /// Show every line of an old turn's reasoning and tool output, rather
    /// than folding each to a summary row (`ctrl-o`, `/verbose`).
    verbose: bool,
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
            scroll_base: 0,
            raised_permissions: std::collections::HashSet::new(),
            verbose: false,
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
        self.raise_pending_permission();
    }

    /// A permission prompt blocks the orchestrator, so it opens itself rather
    /// than waiting to be found. Once per request: dismissing one with `esc`
    /// to go and look something up must not trap the console in a loop, and
    /// the approvals count in the status line is the reminder.
    fn raise_pending_permission(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        let Some(request) = self.orch.pending_requests.first() else {
            return;
        };
        if !self.raised_permissions.insert(request.request_id.clone()) {
            return;
        }
        self.open_permission_overlay();
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

    /// Start the orchestrator transcript over, for a replay from the top of
    /// a file the monitor has since trimmed. Anything anchored to a block
    /// index goes with it — those indices meant the old content.
    pub fn reset_orchestrator_transcript(&mut self) {
        self.orch_transcript = Transcript::new();
        self.scroll = None;
        self.scroll_base = 0;
        self.search = None;
    }

    /// Fold one orchestrator record into the transcript.
    pub fn ingest_orchestrator_record(&mut self, record: &crate::orch::records::EventRecord) {
        self.orch_transcript.apply_orchestrator_record(record);
        self.rebase_scroll();
        self.refresh_rows();
    }

    /// Fold one worker event into that worker's transcript.
    pub fn ingest_worker_event(&mut self, run_id: &str, event: &Value) {
        self.worker_transcript_mut(run_id).apply_worker_event(event);
        self.rebase_scroll();
    }

    /// Follow the content a pinned scroll and the search hits point at when
    /// blocks fall off the top. Without this a long session silently shifts
    /// what the reader is looking at every time the transcript trims.
    fn rebase_scroll(&mut self) {
        let dropped = self.open_transcript_dropped();
        let shift = dropped.saturating_sub(self.scroll_base);
        self.scroll_base = dropped;
        if shift == 0 {
            return;
        }
        if let Some(at) = self.scroll.as_mut() {
            *at = at.saturating_sub(shift);
        }
        if let Some(search) = self.search.as_mut() {
            for hit in &mut search.matches {
                *hit = hit.saturating_sub(shift);
            }
        }
    }

    fn open_transcript_dropped(&self) -> usize {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => self.orch_transcript.dropped(),
            SessionTarget::Worker { run_id } => self
                .worker_transcripts
                .get(&run_id)
                .map_or(0, Transcript::dropped),
        }
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

    /// Whether old reasoning and tool output are shown in full.
    #[must_use]
    pub const fn verbose(&self) -> bool {
        self.verbose
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
            self.scroll_base = self.open_transcript_dropped();
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
        // The palette and the search box are text fields, so they read keys
        // the way the composer does. Every other overlay is a list, and reads
        // single letters as its own commands.
        // The palette and the search box are text fields, and so is a
        // permission overlay while a deny reason or a custom answer is being
        // written — a reason starting with "just" must not lose its letters
        // to the list navigation.
        let typed = match &self.overlay {
            None | Some(Overlay::Palette(_) | Overlay::Search(_)) => true,
            Some(Overlay::Permission(state)) => state.denying || state.custom,
            Some(_) => false,
        };
        let action = if typed {
            map_key(key)
        } else {
            map_overlay_key(key)
        };
        self.handle_action(action)
    }

    /// Apply an already-mapped action. The runtime routes mouse wheel events
    /// through here too; an open overlay owns the action either way.
    pub fn handle_action(&mut self, action: KeyAction) -> Vec<Effect> {
        if let Some(overlay) = self.overlay.clone() {
            return self.handle_overlay(overlay, action);
        }
        self.handle_compose(action)
    }

    /// Everything that has to follow the selection, wherever it moved from.
    fn on_selection_changed(&mut self) {
        self.search = None;
        self.scroll = None;
        self.scroll_base = self.open_transcript_dropped();
        self.remember_last_session();
        self.recompute_completion();
    }

    fn move_selection(&mut self, delta: i64) {
        let total = self.rows.len();
        if total == 0 {
            return;
        }
        let next = (self.selected as i64 + delta).rem_euclid(total as i64) as usize;
        if next != self.selected {
            self.selected = next;
            // the search and the scroll belonged to the session that was open
            self.on_selection_changed();
        }
    }

    /// Remember which row the conversation is showing, so reopening the
    /// console comes back to it.
    fn remember_last_session(&mut self) {
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

    fn handle_compose(&mut self, action: KeyAction) -> Vec<Effect> {
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
            // esc walks out of whatever is in the way, innermost first:
            // a suggestion popup, then an answer being composed, then the
            // line itself, and only with nothing left does it stop the turn
            KeyAction::Escape => {
                if self.composer.completion.is_some() && !self.composer.dismissed {
                    self.composer.dismissed = true;
                    return Vec::new();
                }
                if self.composer.answering.take().is_some() {
                    self.composer.input.clear();
                    self.composer.cursor = 0;
                    return Vec::new();
                }
                if !self.composer.input.is_empty() {
                    self.composer.input.clear();
                    self.composer.cursor = 0;
                    self.composer.completion = None;
                    self.history_at = None;
                    return Vec::new();
                }
                self.interrupt_turn()
            }
            KeyAction::OpenFleet => {
                self.overlay = Some(Overlay::Fleet);
                Vec::new()
            }
            KeyAction::ToggleVerbose => self.toggle_verbose(),
            KeyAction::OpenPalette => self.open_palette(PaletteScope::All),
            // the same key opens the box and then walks the matches, the way
            // a shell's reverse search does
            KeyAction::Search => {
                if self.search.is_some() {
                    self.step_match(1);
                } else {
                    self.overlay = Some(Overlay::Search(SearchState::default()));
                }
                Vec::new()
            }
            KeyAction::ToggleMouse => self.toggle_mouse(),
            // the wheel and the page keys scroll the transcript while the
            // composer has focus; typing is never interrupted
            KeyAction::ScrollHalfUp => {
                self.scroll_page(-(self.viewport_rows as i64) / 2);
                Vec::new()
            }
            KeyAction::ScrollHalfDown => {
                self.scroll_page(self.viewport_rows as i64 / 2);
                Vec::new()
            }
            KeyAction::ScrollPageUp => {
                self.scroll_page(-(self.viewport_rows as i64));
                Vec::new()
            }
            KeyAction::ScrollPageDown => {
                self.scroll_page(self.viewport_rows as i64);
                Vec::new()
            }
            KeyAction::First => {
                self.scroll = Some(0);
                Vec::new()
            }
            KeyAction::Last => {
                self.scroll = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Esc with an empty composer: stop whatever the selected session is
    /// doing. Nothing running means nothing to stop, and saying so beats a
    /// key that silently does nothing.
    fn interrupt_turn(&mut self) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => {
                if self.orch.turn_active || self.orch_transcript.turn_active() {
                    self.notice("■ stopping the orchestrator's turn", false);
                    vec![Effect::Interrupt]
                } else {
                    Vec::new()
                }
            }
            SessionTarget::Worker { .. } => self.stop_selected(),
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
                Effect::TrimTranscript => {
                    let path = self.fleet.orchestrator_events(&self.orch_key);
                    let trimmed = crate::orch::records::trim_events_file(
                        &path,
                        crate::orch::client::MAX_RESTORED_LINES,
                    );
                    self.notice(
                        if trimmed {
                            format!(
                                "· transcript cut to its last {} records",
                                crate::orch::client::MAX_RESTORED_LINES
                            )
                        } else {
                            "· transcript is already short enough".to_string()
                        },
                        false,
                    );
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

mod commands;
mod overlays;

#[cfg(test)]
mod tests;
