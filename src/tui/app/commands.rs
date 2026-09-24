//! The composer's line and the commands it can carry: the console's own
//! slash commands, the session verbs (`/sessions`, `/session`, `/shutdown`),
//! and the per-session actions the fleet panel's letters reach.
//!
//! A child module of `app`, like `overlays`: `Console`'s state stays private
//! to the console, and only what the parent calls back into is `pub(super)`.

use std::collections::HashMap;

use uuid::Uuid;

use crate::fleet::run::{DerivedView, RunState, derive_view};
use crate::orch::args::{PERMISSION_MODES, describe_permission_mode};
use crate::paths::SessionKey;
use crate::tui::completions::resolve_command;
use crate::tui::model::SessionTarget;

use crate::tui::transcript::Transcript;

use crate::util::now_ms;

use super::{
    AnswerKind, Answering, CLAUDE_EFFORT_LEVELS, ConfirmAction, ConfirmState, Console, Effect,
    HISTORY_CAP, NO_SESSION, Overlay, PermissionOverlay, is_terminal_view, next_level,
    parse_answer, questions_of, session_display_name, suggest_command, worker_thinking_levels,
};

impl Console {
    // -----------------------------------------------------------------------
    // Session commands (`/sessions`, `/session`, `/shutdown <key>`)

    /// `/sessions`: every live session with its alias, short uuid, worker
    /// count and monitor health — `running` from a live monitor with a fresh
    /// heartbeat, `wedged` from a live one that stopped stamping (two missed
    /// stamps inside the 15 s grace), `stopped` for a row with no live
    /// monitor (never started, or its pid is gone). The full list goes into
    /// the transcript to look back at; the toolbar carries a tally.
    pub(super) fn show_sessions(&mut self) {
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
    pub(super) fn session_command(&mut self, argument: &str) -> Vec<Effect> {
        let mut words = argument.split_whitespace();
        match words.next() {
            None => {
                self.notice(
                    "· usage: /session new [alias], or /session <uuid-or-alias> — /sessions lists them",
                    false,
                );
                Vec::new()
            }
            Some("rename") => {
                let name = words.collect::<Vec<_>>().join(" ");
                if !self.session_open {
                    self.notice(format!("! {NO_SESSION}"), true);
                    return Vec::new();
                }
                match crate::orch::session::rename_session(
                    self.fleet.root(),
                    self.orch_key.uuid,
                    &name,
                ) {
                    Ok(session) => {
                        self.orch_key = session.key();
                        let label = session_display_name(session.alias.as_deref(), session.uuid);
                        self.notice(format!("· this session is now {label}"), false);
                    }
                    Err(err) => self.notice(format!("! {err:#}"), true),
                }
                Vec::new()
            }
            Some("remove") => {
                let Some(target) = words.next() else {
                    self.notice(
                        "· usage: /session remove <uuid-or-alias> — /sessions lists them",
                        false,
                    );
                    return Vec::new();
                };
                match crate::orch::session::resolve_session_by_key(self.fleet.root(), target) {
                    Ok(session) => self.confirm_remove_session(&session.key()),
                    Err(err) => self.notice(format!("! {err:#}"), true),
                }
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

    /// Ask before removing session `key` outright, naming every worker that
    /// goes with it and what it is doing — this is the one command that
    /// throws unmerged work away on purpose.
    pub(super) fn confirm_remove_session(&mut self, key: &SessionKey) {
        let label = session_display_name(key.alias.as_deref(), key.uuid);
        let workers: Vec<String> =
            crate::fleet::run::list_runs_for_owner(self.fleet.root(), key.uuid)
                .into_iter()
                .filter_map(|summary| {
                    let state = crate::fleet::run::load_state(&summary.run_dir).ok()?;
                    let view = derive_view(&state, crate::fleet::run::is_alive, now_ms());
                    (view != crate::fleet::run::DerivedView::Archived).then(|| {
                        let diff = self
                            .diff_stats
                            .get(&summary.run_id)
                            .map(|stat| format!(" {stat}"))
                            .unwrap_or_default();
                        format!("  {} · {view}{diff}", state.name)
                    })
                })
                .collect();
        let mut message = format!(
            "Remove session {label} completely? Its orchestrator stops and its transcript \
and run records are deleted."
        );
        if workers.is_empty() {
            message.push_str(" It has no workers left.");
        } else {
            message.push_str(&format!(
                " Its {} {} stopped (killed if {} will not stop) and removed with {} worktree \
and branch; unmerged work is lost:\n{}",
                workers.len(),
                if workers.len() == 1 {
                    "worker is"
                } else {
                    "workers are"
                },
                if workers.len() == 1 { "it" } else { "they" },
                if workers.len() == 1 { "its" } else { "their" },
                workers.join("\n")
            ));
        }
        // leaving the session the console is on: go to the most recently
        // used other one, or to no session at all — never a fresh one, since
        // only the user starts sessions
        let leaving = key.uuid == self.orch_key.uuid;
        let next = leaving
            .then(|| {
                let mut others: Vec<_> = crate::orch::session::list_sessions(self.fleet.root())
                    .into_iter()
                    .filter(|session| session.uuid != key.uuid)
                    .collect();
                others.sort_by(|a, b| b.last_used_at.cmp(&a.last_used_at));
                others
                    .first()
                    .map(crate::orch::session::OrchestratorSession::key)
            })
            .flatten();
        if next.is_some() {
            message.push_str("\nThe console moves to another session afterwards.");
        } else if leaving {
            message.push_str("\nNo session is left open afterwards.");
        }
        self.overlay = Some(Overlay::Confirm(ConfirmState {
            message,
            action: ConfirmAction::RemoveSession {
                key: key.clone(),
                next,
            },
        }));
    }

    /// `/shutdown` with no argument stops the active session and closes the
    /// console (the classic `Q`); with a key it stops that session only —
    /// its workers aborted, its orchestrator told to stop — and the console
    /// stays open, so other sessions and their workers are untouched.
    pub(super) fn shutdown_command(&mut self, argument: &str) -> Vec<Effect> {
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
        self.session_open = true;
        self.orch_key = key.clone();
        self.orch_transcript = Transcript::new();
        self.worker_transcripts.clear();
        self.diff_stats.clear();
        self.runs.clear();
        self.rows.clear();
        self.selected = 0;
        self.scroll = None;
        self.search = None;
        self.scroll_base = 0;
        self.permission_answers.clear();
        self.raised_permissions.clear();
        self.pending_effort = None;
        self.pending_thinking.clear();
        self.composer.answering = None;
        self.flash = None;
    }

    /// No session open: the console forgets the one it was on, keeps its
    /// commands, and says how to start one.
    pub fn end_session(&mut self) {
        self.begin_session(&SessionKey::default());
        self.session_open = false;
        self.prefs.last_session_uuid = None;
        self.rows.clear();
        self.notice(format!("· {NO_SESSION}"), false);
    }

    pub(super) fn name_of(&self, run_id: &str) -> String {
        self.run_state(run_id)
            .map_or_else(|| run_id.to_string(), |s| s.name.clone())
    }

    pub(super) fn is_live(state: &RunState) -> bool {
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

    pub(super) fn shutdown_question(&self) -> String {
        let live = self.live_worker_count();
        format!(
            "Stop the orchestrator and {live} running {}? Worktrees and branches are kept.",
            if live == 1 { "worker" } else { "workers" }
        )
    }

    /// `a`: answer the selected session's pending question or dialog.
    /// Put the first pending request up. A question with no options is
    /// answered in your own words, so the overlay opens straight into its
    /// text field.
    pub(super) fn open_permission_overlay(&mut self) {
        let Some(request) = self.orch.pending_requests.first() else {
            return;
        };
        let custom = crate::orch::protocol::is_ask_user_question(&request.request)
            && questions_of(&request.request.input)
                .first()
                .is_some_and(|first| first.options.as_ref().is_none_or(Vec::is_empty));
        self.overlay = Some(Overlay::Permission(PermissionOverlay {
            at: 0,
            question: 0,
            selected: 0,
            denying: false,
            custom,
            input: String::new(),
        }));
    }

    /// Put the oldest waiting model question up, its first option — Jev's
    /// leaning — highlighted.
    pub(super) fn open_model_choice(&mut self) {
        let Some(question) = self.model_questions.first() else {
            return;
        };
        self.overlay = Some(Overlay::ModelChoice(super::ModelChoiceState {
            id: question.id.clone(),
            selected: 0,
        }));
    }

    pub(super) fn answer_selected(&mut self) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => {
                // a spawn waiting on a model choice is the orchestrator
                // waiting too, so both answer from its row
                if !self.orch.pending_requests.is_empty() {
                    self.open_permission_overlay();
                } else if !self.model_questions.is_empty() {
                    self.open_model_choice();
                } else {
                    self.toast("! the orchestrator has nothing waiting for an answer", true);
                }
                Vec::new()
            }
            SessionTarget::Worker { run_id } => {
                let Some(state) = self.run_state(&run_id).cloned() else {
                    self.toast("! that worker is gone", true);
                    return Vec::new();
                };
                if let Some(question) = &state.pending_question {
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
    pub(super) fn stop_selected(&mut self) -> Vec<Effect> {
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

    /// `x`: remove the selected worker, asking first — or, on the
    /// orchestrator's row, the whole session.
    pub(super) fn remove_selected(&mut self) -> Vec<Effect> {
        let SessionTarget::Worker { run_id } = self.selected_target() else {
            let key = self.orch_key.clone();
            self.confirm_remove_session(&key);
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
    pub(super) fn cycle_thinking(&mut self) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => {
                let current = self.effort().map(str::to_string);
                let next = next_level(&CLAUDE_EFFORT_LEVELS, current.as_deref());
                self.pending_effort = Some((next.clone(), now_ms()));
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
                    .map(|(level, _)| level.as_str())
                    .or(state.thinking_level.as_deref());
                let next = next_level(&worker_thinking_levels(&state), current);
                // optimistic, like the orchestrator's pending_effort: the
                // statusline reads it via the state overlay in set_runs, and
                // the next press advances from it instead of the stale state
                self.pending_thinking
                    .insert(run_id.clone(), (next.clone(), now_ms()));
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
    pub(super) fn toggle_mouse(&mut self) -> Vec<Effect> {
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
    pub(super) fn cycle_permission_mode(&mut self) -> Vec<Effect> {
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
    pub(super) fn model_effect(&self, model_id: &str, provider: Option<String>) -> Vec<Effect> {
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
        // with no session there is nobody to message; commands still run
        if !self.session_open && !text.starts_with('/') {
            self.notice(format!("! {NO_SESSION}"), true);
            return Vec::new();
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
            // with a worker selected, `/remove` is that worker's
            Some("/remove") if matches!(self.selected_target(), SessionTarget::Orchestrator(_)) => {
                return self.remove_selected();
            }
            Some("/mouse") => return self.toggle_mouse(),
            Some("/clear") => return self.clear_transcript(),
            Some("/verbose") => return self.toggle_verbose(),
            Some("/routing") => {
                self.overlay = Some(Overlay::Routing(super::RoutingPanel::default()));
                return vec![Effect::LoadRoutingStatus];
            }
            Some("/trim") => return vec![Effect::TrimTranscript],
            _ => {}
        }
        self.remember_history(&text);
        match self.selected_target() {
            SessionTarget::Worker { run_id } => self.submit_to_worker(&run_id, &text),
            SessionTarget::Orchestrator(_) => self.submit_to_orchestrator(&text),
        }
    }

    /// `/verbose` (or `ctrl-o`): show an old turn's reasoning and tool
    /// output in full, or fold each back to a summary row.
    pub(super) fn toggle_verbose(&mut self) -> Vec<Effect> {
        self.verbose = !self.verbose;
        self.notice(
            if self.verbose {
                "· showing every line of older turns"
            } else {
                "· older reasoning and tool output folded"
            },
            false,
        );
        Vec::new()
    }

    /// `/clear`: forget the open session's transcript here. The file on disk
    /// is the audit trail and is never touched — `/trim` is the one that
    /// shortens it.
    pub(super) fn clear_transcript(&mut self) -> Vec<Effect> {
        match self.selected_target() {
            SessionTarget::Orchestrator(_) => self.orch_transcript = Transcript::new(),
            SessionTarget::Worker { run_id } => {
                self.worker_transcripts.insert(run_id, Transcript::new());
            }
        }
        self.scroll = None;
        self.scroll_base = 0;
        self.search = None;
        self.notice("· transcript cleared from the console", false);
        Vec::new()
    }

    pub(super) fn remember_history(&mut self, text: &str) {
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
    pub(super) fn submit_to_worker(&mut self, run_id: &str, text: &str) -> Vec<Effect> {
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
                    self.pending_thinking
                        .insert(run_id.clone(), (level.clone(), now_ms()));
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
    pub(super) fn submit_to_orchestrator(&mut self, text: &str) -> Vec<Effect> {
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
                    self.pending_effort = Some((level.clone(), now_ms()));
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
        // Neither ours nor one claude offers: almost certainly a typo, and
        // sending it would put a question about a command in the log. Only
        // judged against a list claude actually gave — before the first
        // answer arrives (a monitor that has not been asked yet, or one from
        // an older build) the list is empty, and every agent command, `/compact`
        // included, must still get through.
        let asked = !self.caps.fetched_at.is_empty() && !self.caps.commands.is_empty();
        if text.starts_with('/') && asked {
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
                // the list may simply be out of date — a skill installed since
                // it was fetched — so ask again; the next try is judged fresh
                let refresh = self.refresh_orchestrator_capabilities_if_stale();
                self.notice(
                    format!(
                        "! unknown command {head}{}",
                        near.map_or_else(String::new, |n| format!(" — did you mean {n}?"))
                    ),
                    true,
                );
                return refresh;
            }
        }
        self.orch_transcript.push_sent(text);
        vec![Effect::SendToOrchestrator(text.to_string())]
    }
}
