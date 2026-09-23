//! The overlay handlers: what a key means while the fleet panel, a confirm
//! prompt, a permission request, the palette, the search box or the brief
//! viewer is up.
//!
//! Split out of the state machine because they are the bulk of it and
//! nothing else reads them. A child module of `app`, so `Console`'s private
//! state is still private to the console; what the parent calls back into is
//! marked `pub(super)` and nothing else is reachable.

use serde_json::Value;

use crate::orch::protocol::PermissionRequest;
use crate::orch::records::PermissionDecisionRecord;
use crate::paths::SessionKey;
use crate::tui::completions::resolve_command;
use crate::tui::keys::KeyAction;
use crate::tui::palette::{PaletteAction, PaletteScope};

use crate::secrets::Secret;

use super::{
    AskQuestion, BriefState, ConfirmAction, ConfirmState, Console, Effect, KeyState, Overlay,
    PaletteState, PermissionOverlay, RoutingPanel, SearchState, SessionTarget, questions_of,
};

impl Console {
    pub(super) fn handle_overlay(&mut self, overlay: Overlay, action: KeyAction) -> Vec<Effect> {
        match overlay {
            Overlay::Help => {
                // whatever you reach for to dismiss a help panel closes it:
                // the key that opened it, esc, enter, and `q`
                let closes = matches!(action, KeyAction::Escape | KeyAction::Open)
                    || matches!(action, KeyAction::InsertChar('?' | 'q' | 'Q'));
                if closes {
                    self.overlay = None;
                }
                Vec::new()
            }
            Overlay::Fleet => self.handle_fleet(action),
            Overlay::Confirm(state) => self.handle_confirm(state, action),
            Overlay::Permission(state) => self.handle_permission(state, action),
            Overlay::Palette(state) => self.handle_palette(state, action),
            Overlay::Search(state) => self.handle_search(state, action),
            Overlay::Brief(state) => self.handle_brief(state, action),
            Overlay::Routing(panel) => self.handle_routing(panel, action),
        }
    }

    fn handle_confirm(&mut self, state: ConfirmState, action: KeyAction) -> Vec<Effect> {
        let yes = matches!(action, KeyAction::InsertChar('y' | 'Y'));
        // enter is deliberately not an answer: these prompts guard work that
        // cannot be undone, and the hint asks for y or n
        let no = matches!(action, KeyAction::InsertChar('n' | 'N') | KeyAction::Escape);
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
    pub(super) fn shutdown_effects(&self) -> Vec<Effect> {
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
    pub(super) fn shutdown_effects_for(&self, key: &SessionKey) -> Vec<Effect> {
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
        // the prompt raised itself a moment ago: a key this soon was typed at
        // the composer, and must not answer a question nobody has read yet
        if self.within_raise_grace() && !matches!(action, KeyAction::Escape) {
            self.overlay = Some(Overlay::Permission(state));
            return Vec::new();
        }
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
                KeyAction::Escape => {
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
                KeyAction::Escape => {
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
            KeyAction::Escape => {
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

    pub(super) fn handle_palette(
        &mut self,
        mut state: PaletteState,
        action: KeyAction,
    ) -> Vec<Effect> {
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
            KeyAction::Escape => {
                self.overlay = None;
                return Vec::new();
            }
            _ => {}
        }
        self.overlay = Some(Overlay::Palette(state));
        Vec::new()
    }

    /// What a chosen palette entry does.
    pub(super) fn run_palette_action(&mut self, action: PaletteAction) -> Vec<Effect> {
        match action {
            PaletteAction::ConsoleCommand(name) => match resolve_command(&name) {
                // commands that take an argument prefill the composer
                Some(spec) if spec.takes_argument => {
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
            // esc cancels: nothing is kept, so the next ctrl-r opens a fresh box
            KeyAction::Escape => {
                self.search = None;
                self.overlay = None;
                return Vec::new();
            }
            KeyAction::Send | KeyAction::Open => {
                self.apply_search(state.query.clone());
                self.overlay = None;
                if self.search.as_ref().is_some_and(|s| s.matches.is_empty()) {
                    self.toast(format!("· no match for \"{}\"", state.query), false);
                }
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

    /// The `/routing` panel. Single letters act while nothing is being typed;
    /// while a key is being entered, every key is part of it except enter
    /// (save) and esc (cancel).
    fn handle_routing(&mut self, mut panel: RoutingPanel, action: KeyAction) -> Vec<Effect> {
        if let Some(key) = panel.entering.as_mut() {
            match action {
                KeyAction::InsertChar(ch) => key.push(ch),
                KeyAction::InsertBackspace => key.pop(),
                KeyAction::Escape => panel.entering = None,
                KeyAction::Send => {
                    let key = key.finished();
                    panel.entering = None;
                    self.overlay = Some(Overlay::Routing(panel));
                    if key.is_empty() {
                        return Vec::new();
                    }
                    return vec![Effect::SaveTypesafeKey(key)];
                }
                _ => {}
            }
            self.overlay = Some(Overlay::Routing(panel));
            return Vec::new();
        }
        if panel.confirm_delete {
            panel.confirm_delete = false;
            let delete = matches!(action, KeyAction::InsertChar('y' | 'Y'));
            self.overlay = Some(Overlay::Routing(panel));
            return if delete {
                vec![Effect::DeleteTypesafeKey]
            } else {
                Vec::new()
            };
        }
        match action {
            KeyAction::Escape | KeyAction::Send => {
                self.overlay = None;
                Vec::new()
            }
            KeyAction::InsertChar('s') => {
                panel.entering = Some(Secret::default());
                self.overlay = Some(Overlay::Routing(panel));
                Vec::new()
            }
            KeyAction::InsertChar('r') => {
                let Some(status) = &panel.status else {
                    return Vec::new();
                };
                vec![Effect::SetRouting(!status.enabled)]
            }
            KeyAction::InsertChar('d') => {
                if matches!(
                    panel.status.as_ref().map(|s| &s.key),
                    Some(KeyState::Store { .. })
                ) {
                    panel.confirm_delete = true;
                    self.overlay = Some(Overlay::Routing(panel));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
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
            KeyAction::Open | KeyAction::Send | KeyAction::Escape => {
                self.overlay = None;
                return Vec::new();
            }
            _ => {}
        }
        self.overlay = Some(Overlay::Brief(state));
        Vec::new()
    }

    /// Case-insensitive matches over the open session's transcript blocks.
    pub(super) fn search_matches(&self, query: &str) -> Vec<usize> {
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

    /// Keep a search with its matches, pinning the view at the first. A
    /// search that found nothing is not kept: there is nothing to step
    /// through, and a kept empty search would make the next `ctrl-r` step
    /// through nothing instead of opening the box again.
    pub(super) fn apply_search(&mut self, query: String) {
        let matches = self.search_matches(&query);
        if matches.is_empty() {
            self.search = None;
            return;
        }
        let current = matches.first().copied();
        self.scroll = current;
        self.search = Some(SearchState {
            query,
            matches,
            current,
        });
    }

    #[allow(clippy::too_many_lines)]
    /// The fleet overlay: a list of every session, where single letters are
    /// commands because nothing is being typed.
    pub(super) fn handle_fleet(&mut self, action: KeyAction) -> Vec<Effect> {
        match action {
            KeyAction::Move(delta) => {
                self.move_selection(i64::from(delta));
                Vec::new()
            }
            KeyAction::First => {
                self.selected = 0;
                self.on_selection_changed();
                Vec::new()
            }
            KeyAction::Last => {
                self.selected = self.rows.len().saturating_sub(1);
                self.on_selection_changed();
                Vec::new()
            }
            KeyAction::JumpTo(index) => {
                if index < self.rows.len() {
                    self.selected = index;
                    self.on_selection_changed();
                }
                Vec::new()
            }
            // enter and esc both land back in the conversation; enter takes
            // the row it was on, esc leaves the selection where it was
            KeyAction::Send | KeyAction::Open | KeyAction::Escape => {
                self.overlay = None;
                Vec::new()
            }
            KeyAction::OpenPalette => self.open_palette(PaletteScope::All),
            KeyAction::ToggleMouse => self.toggle_mouse(),
            KeyAction::InsertChar(ch) => match ch {
                'a' => {
                    self.overlay = None;
                    self.answer_selected()
                }
                's' => self.stop_selected(),
                'x' => self.remove_selected(),
                't' => self.cycle_thinking(),
                'm' => self.open_palette(PaletteScope::Models),
                'p' => self.cycle_permission_mode(),
                'b' => self.open_brief(),
                '?' => {
                    self.overlay = Some(Overlay::Help);
                    Vec::new()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}
