//! The main window: sessions on the canvas, then the open session's workers,
//! the conversation and the selected worker's changes, each on a floating
//! sheet. Every gap between sheets drags; every side pane folds. Anything
//! you can type as a command has a place to click too: the session's title
//! and ⋯ menu, the chips under it, the composer's buttons, the worker cards
//! and the changes header.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FocusHandle,
    FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, ListAlignment, ListState,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Point, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Window, anchored,
    deferred, div, list, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::TitleBar;
use gpui_component::input::{
    Enter, Escape, IndentInline, Input, InputEvent, InputState, MoveDown, MoveUp, Textarea,
    TextareaState,
};
use serde::{Deserialize, Serialize};

use super::backend::{Lane, Snapshot, UiCmd, WorkerItem};
use super::changes::ChangesState;
use super::chat::{self, Group};
use super::theme::{DISPLAY, MONO, Palette, SANS, SERIF, tint};
use super::ui::{self, Tone};
use super::{
    OpenFleet, OpenHelp, OpenPalette, OpenSearch, Shared, ToggleChanges, ToggleSessions,
    ToggleVerbose, ToggleWorkers,
};
use crate::orch::args::{PERMISSION_MODES, describe_permission_mode};
use crate::orch::session::MonitorHealth;
use crate::tui::app::CLAUDE_EFFORT_LEVELS;
use crate::tui::completions::{CompletionState, apply_suggestion, completions_for};
use crate::tui::keys::KeyAction;
use crate::tui::palette::ORCHESTRATOR_MODEL_ALIASES;

const RAIL: f32 = 56.;
/// The gap between sheets, which is also where a border is grabbed.
const GAP: f32 = 10.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Sessions,
    Workers,
    Changes,
}

/// Widths and folds, remembered per repo in `.pilotfish/desktop.json`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
struct Layout {
    sessions: f32,
    workers: f32,
    changes: f32,
    sessions_open: bool,
    workers_open: bool,
    changes_open: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            sessions: 224.,
            workers: 272.,
            changes: 400.,
            sessions_open: true,
            workers_open: true,
            changes_open: true,
        }
    }
}

impl Layout {
    const fn range(pane: Pane) -> (f32, f32) {
        match pane {
            Pane::Sessions => (180., 360.),
            Pane::Workers => (220., 420.),
            Pane::Changes => (300., 780.),
        }
    }

    fn width_mut(&mut self, pane: Pane) -> &mut f32 {
        match pane {
            Pane::Sessions => &mut self.sessions,
            Pane::Workers => &mut self.workers,
            Pane::Changes => &mut self.changes,
        }
    }
}

struct Drag {
    pane: Pane,
    start_x: f32,
    start_width: f32,
}

/// How Enter reaches a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compose {
    /// Steer it after its current tool call.
    Now,
    /// Queue a follow-up for after its current work.
    After,
    /// Answer its pending question.
    Answer,
}

/// What a popover menu is about.
#[derive(Debug, Clone)]
enum MenuKind {
    Session { uuid: uuid::Uuid, name: String },
    Model,
    Effort(Vec<String>),
    Permissions,
    Worker(Box<WorkerItem>),
}

struct Menu {
    kind: MenuKind,
    at: Point<Pixels>,
}

/// What a menu row or button does.
#[derive(Debug, Clone)]
enum Act {
    Rename(uuid::Uuid, String),
    Shutdown(uuid::Uuid),
    RemoveSession(uuid::Uuid),
    Command(String),
    Verbose,
    /// Select this row (`orchestrator` or a run id), then show its brief.
    Brief(String),
    Models,
    Reveal(std::path::PathBuf),
    Copy(String),
    StopWorker(String),
    RemoveWorker(String),
    Merge(String, String),
    FoldAll,
}

#[derive(Debug, Clone)]
enum Dialog {
    NewSession,
    Rename(uuid::Uuid),
    Merge { run_id: String, name: String },
}

pub struct MainView {
    shared: Entity<Shared>,
    repo: String,
    layout_path: std::path::PathBuf,
    layout: Layout,
    drag: Option<Drag>,
    composer: Entity<TextareaState>,
    completion: Option<CompletionState>,
    completion_index: usize,
    compose: Compose,
    /// The worker `compose` was set for, and the selection last seen: a
    /// different selection puts Enter back to sending.
    compose_for: Option<String>,
    last_selected: String,
    chat: ListState,
    groups: Arc<Vec<Group>>,
    changes: ChangesState,
    changes_scroll: gpui::UniformListScrollHandle,
    /// The worker whose tab sits beside the orchestrator's.
    worker_tab: Option<String>,
    sheet_focus: FocusHandle,
    had_overlay: bool,
    /// A sheet was asked for and has not shown yet: its keys are already
    /// going to it, so none are typed into the composer meanwhile.
    awaiting_sheet: Option<Instant>,
    /// The composer's placeholder, as last set.
    placeholder: String,
    /// The Board popup, and the session its chips narrow it to.
    board_open: bool,
    board_filter: Option<uuid::Uuid>,
    board_focus: FocusHandle,
    menu: Option<Menu>,
    dialog: Option<Dialog>,
    dialog_input: Entity<InputState>,
    /// Holds the keys while a dialog with no field is up.
    dialog_focus: FocusHandle,
    /// Holds the keys while no session is open (there is no composer), so
    /// the window's shortcuts still reach it.
    idle_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(
        shared: Entity<Shared>,
        repo: String,
        fleet_root: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 10)
                .submit_on_enter(true)
                .placeholder("Message the orchestrator. / for commands, @ for workers and files")
        });
        let dialog_input = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.recompute_completion(cx),
                    InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &dialog_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.confirm_dialog(window, cx);
                    }
                },
            ),
            cx.observe_in(&shared, window, |this, _, window, cx| {
                this.on_snapshot(window, cx);
            }),
        ];
        super::follow_appearance(window, cx);
        let chat = ListState::new(0, ListAlignment::Bottom, px(1200.));
        chat.set_follow_mode(gpui::FollowMode::Tail);
        let layout_path = fleet_root.join("desktop.json");
        let layout = std::fs::read_to_string(&layout_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        composer.update(cx, |state, cx| state.focus(window, cx));
        Self {
            shared,
            repo,
            layout_path,
            layout,
            drag: None,
            composer,
            completion: None,
            completion_index: 0,
            compose: Compose::Now,
            compose_for: None,
            last_selected: String::new(),
            chat,
            groups: Arc::new(Vec::new()),
            changes: ChangesState::default(),
            changes_scroll: gpui::UniformListScrollHandle::new(),
            worker_tab: None,
            sheet_focus: cx.focus_handle(),
            had_overlay: false,
            awaiting_sheet: None,
            placeholder: String::new(),
            board_open: false,
            board_filter: None,
            board_focus: cx.focus_handle(),
            menu: None,
            dialog: None,
            dialog_input,
            dialog_focus: cx.focus_handle(),
            idle_focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn snap(&self, cx: &App) -> Arc<Snapshot> {
        self.shared.read(cx).snap.clone()
    }

    fn send(&self, cmd: UiCmd, cx: &App) {
        self.shared.read(cx).send(cmd);
    }

    fn on_snapshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snap = self.snap(cx);
        // the chat: splice from the first group that changed
        let groups = chat::groups(&snap.blocks, snap.partial.as_deref());
        let first_change = self
            .groups
            .iter()
            .zip(groups.iter())
            .position(|(old, new)| old != new)
            .unwrap_or(self.groups.len().min(groups.len()));
        if first_change < self.groups.len() || groups.len() != self.groups.len() {
            self.chat
                .splice(first_change..self.groups.len(), groups.len() - first_change);
        }
        self.groups = Arc::new(groups);
        // the current search match comes into view
        if let Some(crate::tui::app::Overlay::Search(search)) = &snap.overlay
            && let Some(block) = search.current.and_then(|c| search.matches.get(c))
            && let Some(ix) = self.groups.iter().position(|g| g.blocks.contains(block))
        {
            self.chat.scroll_to_reveal_item(ix);
        }
        // the changes pane follows the patch it was pointed at
        match &snap.patch {
            Some(view) => self.changes.update(&view.run_id, &view.patch, cx),
            None => self.changes.clear(),
        }
        // a worker that is gone takes its tab with it
        if let Some(run_id) = &self.worker_tab
            && !snap.workers.iter().any(|w| &w.row.key == run_id)
        {
            self.worker_tab = None;
        }
        // a sheet takes the keys while it is up, and gives them back
        let open = snap.overlay.is_some();
        let lapsed = self
            .awaiting_sheet
            .is_some_and(|at| at.elapsed() > Duration::from_secs(1));
        if open {
            self.awaiting_sheet = None;
        }
        if open && !self.had_overlay {
            self.menu = None;
            window.focus(&self.sheet_focus, cx);
        } else if !open && (self.had_overlay || lapsed) {
            self.awaiting_sheet = None;
            self.restore_focus(window, cx);
        }
        self.had_overlay = open;
        if snap.key.is_none() && !open && self.dialog.is_none() && !self.board_open {
            window.focus(&self.idle_focus, cx);
        }
        // another selection, and Enter just sends again
        if snap.selected != self.last_selected {
            if self.compose_for.as_deref() != Some(snap.selected.as_str()) {
                self.compose = Compose::Now;
            }
            self.last_selected.clone_from(&snap.selected);
        }
        // the placeholder names whoever Enter will reach, and how
        let target = target_name(&snap);
        let placeholder = match self.compose {
            Compose::Answer => format!("Answer {target}"),
            Compose::After => format!("Queue a follow-up for {target}"),
            Compose::Now => format!("Message {target}. / for commands, @ for workers and files"),
        };
        if placeholder != self.placeholder {
            self.composer.update(cx, |state, cx| {
                state.set_placeholder(placeholder.clone(), window, cx);
            });
            self.placeholder = placeholder;
        }
        cx.notify();
    }

    /// Hand the keys back to whatever should hold them: an open dialog,
    /// else the composer, else (no session, so no composer) the window.
    fn restore_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.dialog {
            Some(Dialog::Merge { .. }) => window.focus(&self.dialog_focus, cx),
            Some(_) => self
                .dialog_input
                .update(cx, |state, cx| state.focus(window, cx)),
            None if self.snap(cx).key.is_none() => window.focus(&self.idle_focus, cx),
            None => self
                .composer
                .update(cx, |state, cx| state.focus(window, cx)),
        }
    }

    // -- composer ----------------------------------------------------------

    fn recompute_completion(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().to_string();
        let snap = self.snap(cx);
        self.completion = completions_for(&text, &snap.completion)
            .map(|mut c| {
                // the terminal's mouse capture means nothing here
                c.items.retain(|item| item.label != "/mouse");
                c
            })
            .filter(|c| !c.items.is_empty());
        self.completion_index = 0;
        cx.notify();
    }

    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(completion) = self.completion.take() else {
            return false;
        };
        let Some(suggestion) = completion
            .items
            .get(self.completion_index.min(completion.items.len() - 1))
        else {
            return false;
        };
        let text = self.composer.read(cx).value().to_string();
        let next = apply_suggestion(&text, &completion, suggestion);
        let exact = next.trim_end() == text.trim_end();
        self.composer.update(cx, |state, cx| {
            state.set_value(next.clone(), window, cx);
            state.set_cursor_position(end_of(&next), window, cx);
        });
        self.completion = None;
        cx.notify();
        !exact
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion.is_some() && self.accept_completion(window, cx) {
            return;
        }
        let text = self.composer.read(cx).value().to_string();
        let text = text.trim_end_matches('\n');
        if text.trim().is_empty() {
            return;
        }
        let to_worker = self.snap(cx).selected != "orchestrator";
        let line = match self.compose {
            Compose::After if to_worker && !text.starts_with('/') => format!("/followup {text}"),
            Compose::Answer if to_worker && !text.starts_with('/') => format!("/answer {text}"),
            _ => text.to_string(),
        };
        self.send(UiCmd::Submit(line), cx);
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.completion = None;
        if self.compose == Compose::Answer {
            self.set_compose(Compose::Now, window, cx);
        }
        cx.notify();
    }

    fn set_compose(&mut self, compose: Compose, window: &mut Window, cx: &mut Context<Self>) {
        self.compose = compose;
        self.compose_for.clone_from(&self.worker_tab);
        self.on_snapshot(window, cx);
        self.restore_focus(window, cx);
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
            return;
        }
        if self.completion.take().is_some() {
            cx.notify();
            return;
        }
        if !self.composer.read(cx).value().is_empty() {
            self.composer
                .update(cx, |state, cx| state.set_value("", window, cx));
            return;
        }
        if self.compose == Compose::Answer {
            self.set_compose(Compose::Now, window, cx);
            return;
        }
        // an empty composer: the console's esc (stop the turn)
        self.send(
            UiCmd::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            cx,
        );
    }

    // -- layout ------------------------------------------------------------

    fn save_layout(&self) {
        if let Ok(raw) = serde_json::to_string_pretty(&self.layout) {
            let _ = std::fs::write(&self.layout_path, raw);
        }
    }

    fn toggle(&mut self, pane: Pane, cx: &mut Context<Self>) {
        match pane {
            Pane::Sessions => self.layout.sessions_open = !self.layout.sessions_open,
            Pane::Workers => self.layout.workers_open = !self.layout.workers_open,
            Pane::Changes => self.layout.changes_open = !self.layout.changes_open,
        }
        self.save_layout();
        cx.notify();
    }

    /// The gap beside a pane, which is where its border is grabbed.
    fn grip(&self, pane: Pane, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let dragging = self.drag.as_ref().is_some_and(|d| d.pane == pane);
        let hover = tint(pal.ink, 0.3);
        div()
            .id(SharedString::from(format!("grip-{pane:?}")))
            .group("grip")
            .w(px(GAP))
            .h_full()
            .flex_none()
            .flex()
            .justify_center()
            .py(px(32.))
            .cursor_col_resize()
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .rounded_full()
                    .when(dragging, |this| this.bg(pal.ink))
                    .group_hover("grip", move |this| this.bg(hover)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        let default = *Layout::default().width_mut(pane);
                        *this.layout.width_mut(pane) = default;
                        this.save_layout();
                    } else {
                        this.drag = Some(Drag {
                            pane,
                            start_x: event.position.x.as_f32(),
                            start_width: *this.layout.width_mut(pane),
                        });
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
    }

    fn on_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            self.save_layout();
            cx.notify();
            return;
        }
        let dx = event.position.x.as_f32() - drag.start_x;
        let (min, max) = Layout::range(drag.pane);
        let width = match drag.pane {
            Pane::Changes => drag.start_width - dx,
            _ => drag.start_width + dx,
        };
        let pane = drag.pane;
        *self.layout.width_mut(pane) = width.clamp(min, max);
        cx.notify();
    }

    // -- menus, dialogs and what their rows do ------------------------------

    fn open_menu(&mut self, kind: MenuKind, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.menu = Some(Menu { kind, at });
        cx.stop_propagation();
        cx.notify();
    }

    fn act(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        match act {
            Act::Rename(uuid, current) => {
                self.dialog_input.update(cx, |state, cx| {
                    state.set_value(current, window, cx);
                    state.focus(window, cx);
                });
                self.dialog = Some(Dialog::Rename(uuid));
            }
            Act::Shutdown(uuid) => {
                self.open_sheet(UiCmd::Submit(format!("/shutdown {uuid}")), window, cx);
            }
            Act::RemoveSession(uuid) => {
                self.open_sheet(UiCmd::Submit(format!("/session remove {uuid}")), window, cx);
            }
            Act::Command(line) => self.send(UiCmd::Submit(line), cx),
            Act::Verbose => self.send(UiCmd::Action(KeyAction::ToggleVerbose), cx),
            Act::Brief(target) => {
                self.send(UiCmd::Select(target), cx);
                self.open_sheet(UiCmd::OpenBrief, window, cx);
            }
            Act::Models => self.open_sheet(UiCmd::OpenModels, window, cx),
            Act::Reveal(path) => self.send(UiCmd::Reveal(path), cx),
            Act::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            Act::StopWorker(run_id) => {
                self.send(UiCmd::Select(run_id), cx);
                self.send(UiCmd::Submit("/stop".into()), cx);
            }
            Act::RemoveWorker(run_id) => {
                self.send(UiCmd::Select(run_id), cx);
                self.open_sheet(UiCmd::Submit("/remove".into()), window, cx);
            }
            Act::Merge(run_id, name) => {
                self.dialog = Some(Dialog::Merge { run_id, name });
                window.focus(&self.dialog_focus, cx);
            }
            Act::FoldAll => self.changes.toggle_all(),
        }
        cx.notify();
    }

    fn open_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_placeholder("Name it, or leave it empty", window, cx);
            state.focus(window, cx);
        });
        self.dialog = Some(Dialog::NewSession);
        cx.notify();
    }

    fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let value = self.dialog_input.read(cx).value().trim().to_string();
        match dialog {
            Dialog::NewSession => {
                self.worker_tab = None;
                self.send(UiCmd::Diff(None), cx);
                let line = format!("/session new {value}");
                self.send(UiCmd::Submit(line.trim_end().to_string()), cx);
            }
            Dialog::Rename(uuid) => self.send(UiCmd::RenameSession(uuid, value), cx),
            Dialog::Merge { run_id, .. } => self.send(UiCmd::Merge(run_id), cx),
        }
        self.restore_focus(window, cx);
        cx.notify();
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        self.restore_focus(window, cx);
        cx.notify();
    }

    /// Ask the console for a sheet and hand it the keys straight away.
    /// A dialog keeps the keys until it closes, so nothing opens over it.
    fn open_sheet(&mut self, cmd: UiCmd, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        self.menu = None;
        self.send(cmd, cx);
        self.awaiting_sheet = Some(Instant::now());
        window.focus(&self.sheet_focus, cx);
    }

    fn toggle_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        self.menu = None;
        self.board_open = !self.board_open;
        if self.board_open {
            window.focus(&self.board_focus, cx);
        } else {
            self.restore_focus(window, cx);
        }
        cx.notify();
    }

    fn board_handlers(&self, cx: &mut Context<Self>) -> super::board::Handlers {
        use super::board::CardAction;
        let entity = cx.entity();
        let filter = entity.clone();
        let open = entity.clone();
        let close = entity.clone();
        let card = entity;
        super::board::Handlers {
            filter: std::rc::Rc::new(move |value, _, cx| {
                filter.update(cx, |this, cx| {
                    this.board_filter = value;
                    cx.notify();
                });
            }),
            open: std::rc::Rc::new(move |item: WorkerItem, window, cx| {
                open.update(cx, |this, cx| {
                    this.board_open = false;
                    this.open_worker(&item, window, cx);
                });
            }),
            close: std::rc::Rc::new(move |(), window, cx| {
                close.update(cx, |this, cx| {
                    if this.board_open {
                        this.toggle_board(window, cx);
                    }
                });
            }),
            card: std::rc::Rc::new(
                move |(item, action): (WorkerItem, CardAction), window, cx| {
                    card.update(cx, |this, cx| {
                        this.board_open = false;
                        this.open_worker(&item, window, cx);
                        match action {
                            CardAction::Answer => this.set_compose(Compose::Answer, window, cx),
                            CardAction::Merge => {
                                this.act(Act::Merge(item.row.key, item.row.name), window, cx);
                            }
                        }
                    });
                },
            ),
        }
    }

    /// Show a worker wherever it lives: its session, its tab, its diff.
    fn open_worker(&mut self, item: &WorkerItem, window: &mut Window, cx: &mut Context<Self>) {
        if self.snap(cx).current() != Some(item.session.uuid) {
            self.send(UiCmd::Submit(format!("/session {}", item.session.uuid)), cx);
        }
        self.select_worker(&item.row.key, cx);
        self.restore_focus(window, cx);
    }

    fn open_session(&mut self, key: crate::paths::SessionKey, cx: &mut Context<Self>) {
        let snap = self.snap(cx);
        if Some(key.uuid) == snap.current() {
            self.layout.workers_open = !self.layout.workers_open;
            self.save_layout();
        } else {
            self.layout.workers_open = true;
            self.worker_tab = None;
            self.send(UiCmd::Diff(None), cx);
            // the console's own `/session`: it remembers the session too
            self.send(UiCmd::Submit(format!("/session {}", key.uuid)), cx);
        }
        cx.notify();
    }

    fn select_worker(&mut self, run_id: &str, cx: &mut Context<Self>) {
        self.worker_tab = Some(run_id.to_string());
        self.send(UiCmd::Select(run_id.to_string()), cx);
        self.send(UiCmd::Diff(Some(run_id.to_string())), cx);
        cx.notify();
    }

    // -- panes -------------------------------------------------------------

    fn title_bar(&self, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let toggle = |id: &'static str, open: bool, at: f32, pane: Pane, cx: &mut Context<Self>| {
            let color = if open { pal.ink } else { pal.muted };
            div()
                .id(id)
                .w(px(24.))
                .h(px(17.))
                .rounded(px(5.))
                .border_1()
                .border_color(color)
                .cursor_pointer()
                .relative()
                .child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .bottom(px(2.))
                        .left(px(at))
                        .w(px(5.))
                        .rounded(px(1.5))
                        .when(open, |this| this.bg(color)),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(pane, cx)))
        };
        let key_hint = |keys: &'static str, color| {
            div()
                .text_size(px(11.5))
                .font_weight(FontWeight::NORMAL)
                .text_color(color)
                .child(keys)
        };
        TitleBar::new().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .pr(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(ui::logo(26., pal))
                        .child(
                            div()
                                .font_family(DISPLAY)
                                .italic()
                                .text_size(px(20.))
                                .text_color(pal.ink)
                                .child("pilotfish"),
                        )
                        .child(
                            div()
                                .ml(px(4.))
                                .text_size(px(12.5))
                                .text_color(pal.muted)
                                .child(self.repo.clone()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(toggle(
                            "t-sessions",
                            self.layout.sessions_open,
                            2.,
                            Pane::Sessions,
                            cx,
                        ))
                        .child(toggle(
                            "t-workers",
                            self.layout.workers_open,
                            8.5,
                            Pane::Workers,
                            cx,
                        ))
                        .child(toggle(
                            "t-changes",
                            self.layout.changes_open,
                            15.,
                            Pane::Changes,
                            cx,
                        ))
                        .child(div().w(px(6.)))
                        .child(ui::pill("b-routing", "Routing", Tone::Plain, pal).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_sheet(UiCmd::Submit("/routing".into()), window, cx);
                            }),
                        ))
                        .child(
                            ui::pill("b-commands", "Commands", Tone::Plain, pal)
                                .child(key_hint("⌘K", pal.muted))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.open_sheet(
                                        UiCmd::Action(KeyAction::OpenPalette),
                                        window,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            ui::pill("b-board", "Board", Tone::Ink, pal)
                                .child(key_hint("⌘B", tint(pal.sheet, 0.6)))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.toggle_board(window, cx);
                                })),
                        ),
                ),
        )
    }

    fn sessions(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if !self.layout.sessions_open {
            return self.rail(snap, pal, cx);
        }
        let mut list = div()
            .id("session-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.));
        for (at, session) in snap.sessions.iter().enumerate() {
            let current = Some(session.key.uuid) == snap.current();
            let key = session.key.clone();
            let stopped = session.health == MonitorHealth::Stopped;
            let when = ago(&session.last_used);
            let meta = match (session.lanes.len(), stopped) {
                (0, true) => format!("Stopped, {when}"),
                (0, false) => format!("No workers, {when}"),
                (1, _) => format!("1 worker, {when}"),
                (n, _) => format!("{n} workers, {when}"),
            };
            // the formation: one bar per worker, in its lane's colour
            let mut formation = div().flex().flex_wrap().gap(px(3.)).mt(px(9.));
            for lane in session.lanes.iter().take(12) {
                let color = match lane {
                    Lane::Finished => pal.hair,
                    lane => pal.lane(*lane),
                };
                formation = formation.child(div().w(px(14.)).h(px(4.)).rounded(px(2.)).bg(color));
            }
            let menu = MenuKind::Session {
                uuid: session.key.uuid,
                name: session.name.clone(),
            };
            let hover = tint(pal.sheet, 0.55);
            list = list.child(
                div()
                    .id(("session", at))
                    .group("session")
                    .relative()
                    .px(px(14.))
                    .py(px(11.))
                    .rounded(px(16.))
                    .cursor_pointer()
                    .map(|this| {
                        if current {
                            this.bg(pal.sheet).shadow(pal.sheet_shadow())
                        } else {
                            this.hover(move |this| this.bg(hover))
                        }
                    })
                    .child(ui::serif(session.name.clone(), 17.).pr(px(20.)).text_color(
                        if stopped && !current {
                            pal.muted
                        } else {
                            pal.ink
                        },
                    ))
                    .child(
                        div()
                            .mt(px(2.))
                            .text_size(px(12.))
                            .text_color(pal.muted)
                            .child(meta),
                    )
                    .when(!session.lanes.is_empty(), |this| this.child(formation))
                    .child(
                        more_button(("session-more", at), pal)
                            .absolute()
                            .top(px(8.))
                            .right(px(8.))
                            .when(!current, |this| {
                                this.invisible()
                                    .group_hover("session", |this| this.visible())
                            })
                            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                this.open_menu(menu.clone(), event.position(), cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_session(key.clone(), cx);
                    })),
            );
        }
        div()
            .w(px(self.layout.sessions))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(14.))
                    .pr(px(4.))
                    .pb(px(8.))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(pal.muted)
                            .child("Sessions"),
                    )
                    .child(fold_button("fold-sessions", "‹", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(Pane::Sessions, cx)),
                    )),
            )
            .child(list)
            .child(
                div().pt(px(8.)).child(
                    ui::pill("new-session", "New session", Tone::Plain, pal)
                        .w_full()
                        .h(px(36.))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.open_new_session(window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    /// The sessions pane folded: an initial per session, and a new one.
    fn rail(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut rail = div()
            .w(px(RAIL))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.))
            .child(fold_button("unfold-sessions", "›", pal).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(Pane::Sessions, cx)),
            ));
        for (at, session) in snap.sessions.iter().enumerate() {
            let key = session.key.clone();
            let current = Some(session.key.uuid) == snap.current();
            let waiting = session.lanes.contains(&Lane::Waiting);
            let initial: String = session
                .name
                .chars()
                .next()
                .unwrap_or('·')
                .to_uppercase()
                .collect();
            rail = rail.child(
                div()
                    .id(("rail", at))
                    .relative()
                    .size(px(38.))
                    .flex_none()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .font_family(SERIF)
                    .text_size(px(17.))
                    .map(|this| {
                        if current {
                            this.bg(pal.ink).text_color(pal.sheet)
                        } else {
                            this.bg(pal.sheet)
                                .text_color(pal.ink)
                                .border_1()
                                .border_color(pal.hair)
                        }
                    })
                    .child(initial)
                    .when(waiting, |this| {
                        this.child(div().absolute().top_0().right_0().child(ui::pulse(
                            ("rail-wait", at),
                            pal.buoy,
                            10.,
                        )))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_session(key.clone(), cx);
                    })),
            );
        }
        rail.child(
            div()
                .id("rail-new")
                .size(px(38.))
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(pal.hair)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_size(px(18.))
                .text_color(pal.muted)
                .hover(|this| this.text_color(pal.ink).border_color(pal.ink))
                .child("+")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.open_new_session(window, cx);
                })),
        )
        .into_any_element()
    }

    fn workers(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("worker-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(10.))
            .pb(px(10.));
        list = list.child(self.orchestrator_card(snap, pal, cx));
        let mut workers: Vec<&WorkerItem> = snap.workers.iter().collect();
        workers.sort_by_key(|w| match w.lane {
            Lane::Waiting => 0,
            Lane::Failed => 1,
            Lane::Started => 2,
            Lane::Finished => 3,
        });
        for (at, worker) in workers.iter().enumerate() {
            list = list.child(self.worker_card(at, worker, snap, pal, cx));
        }
        if workers.is_empty() {
            list = list.child(note(
                pal,
                "No workers yet. Ask the orchestrator to split the work and they appear here.",
            ));
        }
        let count = match workers.len() {
            1 => "1 worker".to_string(),
            n => format!("{n} workers"),
        };
        ui::sheet(pal)
            .w(px(self.layout.workers))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(18.))
                    .pr(px(10.))
                    .pt(px(14.))
                    .pb(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Workers"),
                            )
                            .child(div().text_size(px(12.)).text_color(pal.muted).child(count)),
                    )
                    .child(fold_button("fold-workers", "‹", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(Pane::Workers, cx)),
                    )),
            )
            .child(list)
            .into_any_element()
    }

    /// The orchestrator, set apart in ink.
    fn orchestrator_card(
        &self,
        snap: &Snapshot,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let detail = snap
            .rows
            .first()
            .map(|row| row.detail.clone())
            .filter(|detail| !detail.is_empty())
            .unwrap_or_else(|| {
                if snap.facts.exited {
                    "exited".into()
                } else {
                    "idle".into()
                }
            });
        // inked while it is the one you are talking to
        let selected = snap.selected == "orchestrator";
        let (bg, fg, border) = if selected {
            (pal.ink, pal.sheet, pal.ink)
        } else {
            (pal.tint, pal.ink, pal.hair)
        };
        let soft = tint(fg, 0.62);
        div()
            .id("orchestrator")
            .p(px(14.))
            .rounded(px(16.))
            .bg(bg)
            .text_color(fg)
            .cursor_pointer()
            .border_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(ui::serif("Orchestrator", 17.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(soft)
                            .when(snap.facts.turn_active, |this| {
                                this.child(ui::pulse("orch-live", pal.buoy, 7.))
                            })
                            .child(snap.facts.model.clone().unwrap_or_default()),
                    ),
            )
            .child(
                ui::line(detail)
                    .mt(px(3.))
                    .text_size(px(12.))
                    .text_color(soft),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.send(UiCmd::Select("orchestrator".into()), cx);
            }))
    }

    fn worker_card(
        &self,
        at: usize,
        worker: &WorkerItem,
        snap: &Snapshot,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = snap.selected == worker.row.key;
        let waiting = worker.lane == Lane::Waiting;
        let finished = worker.lane == Lane::Finished;
        let item = worker.clone();
        let hover = pal.tint;
        let mut card = div()
            .id(("worker", at))
            .p(px(12.))
            .rounded(px(14.))
            .border_1()
            .cursor_pointer()
            .map(|this| {
                if waiting {
                    this.bg(pal.buoy_soft).border_color(tint(pal.buoy, 0.4))
                } else if selected {
                    this.bg(pal.tint).border_color(pal.hair)
                } else {
                    this.border_color(gpui::transparent_black())
                        .hover(move |this| this.bg(hover))
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        ui::serif(worker.row.name.clone(), 16.)
                            .min_w_0()
                            .text_color(if finished { pal.muted } else { pal.ink }),
                    )
                    .map(|this| {
                        if waiting {
                            this.child(ui::pulse(("wait", at), pal.buoy, 8.))
                        } else {
                            this.children(
                                worker
                                    .row
                                    .diff_stat
                                    .as_deref()
                                    .map(|s| ui::stat_text(s, pal)),
                            )
                        }
                    }),
            );
        if waiting {
            let answer = item.clone();
            card = card
                .when_some(worker.question.clone(), |this, question| {
                    this.child(
                        div()
                            .mt(px(6.))
                            .text_size(px(12.5))
                            .line_height(px(17.))
                            .child(question),
                    )
                })
                .child(
                    div().flex().mt(px(10.)).child(
                        ui::pill(("answer", at), "Answer", Tone::Buoy, pal)
                            .h(px(26.))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.open_worker(&answer, window, cx);
                                this.set_compose(Compose::Answer, window, cx);
                            })),
                    ),
                );
        } else {
            let detail = match (&worker.error, worker.lane) {
                (Some(error), Lane::Failed) => error.clone(),
                _ => worker.row.detail.clone(),
            };
            card = card
                .child(ui::line(detail).mt(px(2.)).text_size(px(12.)).text_color(
                    if worker.lane == Lane::Failed {
                        pal.port
                    } else {
                        pal.muted
                    },
                ))
                .child(
                    div()
                        .mt(px(10.))
                        .child(ui::wake(("wake", at), worker.lane, pal)),
                );
            if worker.mergeable {
                let merge = (item.row.key.clone(), item.row.name.clone());
                card = card.child(
                    div().flex().mt(px(10.)).child(
                        ui::pill(("merge", at), "Merge", Tone::Plain, pal)
                            .h(px(26.))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.act(Act::Merge(merge.0.clone(), merge.1.clone()), window, cx);
                            })),
                    ),
                );
            }
        }
        card.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.open_worker(&item, window, cx);
        }))
    }

    fn chat(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let on_orch = snap.selected == "orchestrator";
        let name = snap
            .sessions
            .iter()
            .find(|s| Some(s.key.uuid) == snap.current())
            .map_or_else(|| "Session".to_string(), |s| s.name.clone());
        let uuid = snap.current();
        let worker = self
            .worker_tab
            .as_ref()
            .and_then(|run_id| snap.workers.iter().find(|w| &w.row.key == run_id))
            .cloned();
        let rename = name.clone();
        let menu_name = name.clone();
        let header = div()
            .px(px(28.))
            .pt(px(20.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .child(
                        div()
                            .id("session-title")
                            .group("title")
                            .min_w_0()
                            .flex()
                            .items_baseline()
                            .gap(px(10.))
                            .cursor_pointer()
                            .child(ui::display(name, 34.).min_w_0())
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.))
                                    .text_color(pal.muted)
                                    .invisible()
                                    .group_hover("title", |this| this.visible())
                                    .child("Rename"),
                            )
                            .when_some(uuid, |this, uuid| {
                                this.on_click(cx.listener(
                                    move |this, _: &ClickEvent, window, cx| {
                                        this.act(Act::Rename(uuid, rename.clone()), window, cx);
                                    },
                                ))
                            }),
                    )
                    .when_some(uuid, |this, uuid| {
                        this.child(more_button("session-menu", pal).size(px(32.)).on_click(
                            cx.listener(move |this, event: &ClickEvent, _, cx| {
                                let kind = MenuKind::Session {
                                    uuid,
                                    name: menu_name.clone(),
                                };
                                this.open_menu(kind, event.position(), cx);
                            }),
                        ))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .when_some(worker.as_ref(), |this, worker| {
                        this.child(self.tabs(snap, worker, pal, cx))
                    })
                    .child(chips(snap, worker.as_ref(), pal, cx)),
            );

        let groups = self.groups.clone();
        let palette = *pal;
        let matches = search_matches(snap);
        let streaming = snap.facts.turn_active && snap.partial.is_some();
        let last = groups.len().saturating_sub(1);
        let body: AnyElement = if groups.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(note(
                    pal,
                    if on_orch {
                        "Tell the orchestrator what you want done. It splits the work between workers and reports back here."
                    } else {
                        "This worker has not said anything yet."
                    },
                ))
                .into_any_element()
        } else {
            list(self.chat.clone(), move |ix, window, cx| {
                groups.get(ix).map_or_else(
                    || div().into_any_element(),
                    |group| {
                        let matched = matches.iter().any(|m| group.blocks.contains(m));
                        let live = streaming && ix == last;
                        chat::render(ix, group, matched, live, &palette, window, cx)
                    },
                )
            })
            .flex_1()
            .pb(px(10.))
            .into_any_element()
        };
        let status = snap
            .flash
            .as_ref()
            .map(|flash| {
                (
                    flash.text.clone(),
                    if flash.error { pal.port } else { pal.muted },
                )
            })
            .or_else(|| snap.activity.clone().map(|a| (a, pal.muted)));
        ui::sheet(pal)
            .flex_1()
            .min_w(px(380.))
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(self.waiting_strip(snap, pal, cx))
            .child(body)
            .when_some(status, |this, (text, color)| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(30.))
                        .pb(px(6.))
                        .text_size(px(12.))
                        .text_color(color)
                        .when(snap.facts.turn_active, |this| {
                            this.child(ui::pulse("activity", pal.ink, 7.))
                        })
                        .child(ui::line(text)),
                )
            })
            .child(self.composer(snap, pal, cx))
            .into_any_element()
    }

    /// Orchestrator | the worker last opened, as a segmented control.
    fn tabs(
        &self,
        snap: &Snapshot,
        worker: &WorkerItem,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_orch = snap.selected == "orchestrator";
        let tab = |id: &'static str, on: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(12.))
                .h(px(26.))
                .rounded_full()
                .whitespace_nowrap()
                .cursor_pointer()
                .text_size(px(12.5))
                .map(|this| {
                    if on {
                        this.bg(pal.sheet)
                            .text_color(pal.ink)
                            .shadow(pal.sheet_shadow())
                    } else {
                        this.text_color(pal.muted)
                    }
                })
        };
        let mut tabs = div()
            .flex()
            .items_center()
            .p(px(3.))
            .rounded_full()
            .bg(pal.tint)
            .border_1()
            .border_color(pal.hair)
            .child(
                tab("tab-orch", on_orch)
                    .child("Orchestrator")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.send(UiCmd::Select("orchestrator".into()), cx);
                    })),
            );
        {
            let id = worker.row.key.clone();
            let close = worker.row.key.clone();
            tabs = tabs.child(
                tab("tab-worker", !on_orch)
                    .child(ui::dot(pal.lane(worker.lane), 6.))
                    .child(worker.row.name.clone())
                    .child(
                        div()
                            .id("tab-close")
                            .pl(px(2.))
                            .text_color(pal.muted)
                            .hover(|this| this.text_color(pal.ink))
                            .child("×")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if this.snap(cx).selected == close {
                                    this.send(UiCmd::Select("orchestrator".into()), cx);
                                }
                                this.worker_tab = None;
                                cx.notify();
                            })),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.send(UiCmd::Select(id.clone()), cx);
                    })),
            );
        }
        tabs
    }

    /// Everything waiting on you, pinned above the conversation.
    fn waiting_strip(
        &self,
        snap: &Snapshot,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row = |id: SharedString, text: AnyElement, button: gpui::Stateful<gpui::Div>| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .pl(px(14.))
                .pr(px(8.))
                .py(px(8.))
                .rounded(px(16.))
                .bg(pal.buoy_soft)
                .border_1()
                .border_color(tint(pal.buoy, 0.35))
                .child(ui::pulse(id, pal.buoy, 8.))
                .child(div().flex_1().min_w_0().text_size(px(13.)).child(text))
                .child(button)
                .into_any_element()
        };
        let mut items: Vec<AnyElement> = Vec::new();
        if snap.overlay.is_none() && !snap.requests.is_empty() {
            let text = match snap.requests.len() {
                1 => "The orchestrator is waiting for your approval.".to_string(),
                n => format!("The orchestrator is waiting for {n} approvals."),
            };
            items.push(row(
                "wait-approval".into(),
                div().child(text).into_any_element(),
                ui::pill("review", "Review", Tone::Buoy, pal).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.open_sheet(UiCmd::ReviewWaiting, window, cx);
                    },
                )),
            ));
        }
        if snap.overlay.is_none()
            && snap.requests.is_empty()
            && let Some(question) = snap.model_questions.first()
        {
            items.push(row(
                "wait-model".into(),
                div()
                    .child(format!(
                        "Jev is not sure which model should run {}.",
                        question.name
                    ))
                    .into_any_element(),
                ui::pill("choose-model", "Choose", Tone::Buoy, pal).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| {
                        this.open_sheet(UiCmd::ReviewWaiting, window, cx);
                    },
                )),
            ));
        }
        let asking = snap
            .workers
            .iter()
            .filter(|w| w.lane == Lane::Waiting && snap.selected != w.row.key);
        for (at, worker) in asking.take(2).enumerate() {
            let item = worker.clone();
            let question = worker
                .question
                .clone()
                .unwrap_or_else(|| "is waiting on you".into());
            items.push(row(
                SharedString::from(format!("wait-worker-{at}")),
                div()
                    .flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(worker.row.name.clone()),
                    )
                    .child(ui::line(question))
                    .into_any_element(),
                ui::pill(("wait-answer", at), "Answer", Tone::Buoy, pal).on_click(cx.listener(
                    move |this, _: &ClickEvent, window, cx| {
                        this.open_worker(&item, window, cx);
                        this.set_compose(Compose::Answer, window, cx);
                    },
                )),
            ));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(20.))
            .when(!items.is_empty(), |this| this.pb(px(8.)))
            .children(items)
    }

    fn composer(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let on_orch = snap.selected == "orchestrator";
        let answering = self.compose == Compose::Answer && !on_orch;
        let mut wrap = div()
            .relative()
            .mx(px(16.))
            .mb(px(16.))
            .key_context("Composer")
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if let Some(completion) = &this.completion {
                    let n = completion.items.len();
                    this.completion_index = (this.completion_index + n - 1) % n;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if let Some(completion) = &this.completion {
                    this.completion_index = (this.completion_index + 1) % completion.items.len();
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.completion.is_some() {
                    this.accept_completion(window, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                if !action.shift && this.completion.is_some() && this.accept_completion(window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                this.escape(window, cx);
                cx.stop_propagation();
            }));
        if let Some(completion) = &self.completion {
            let mut popup = ui::menu(pal)
                .absolute()
                .left_0()
                .right_0()
                .bottom_full()
                .mb(px(8.));
            for (at, item) in completion.items.iter().enumerate().take(8) {
                popup = popup.child(
                    div()
                        .id(("suggestion", at))
                        .flex()
                        .gap(px(12.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(9.))
                        .cursor_pointer()
                        .when(at == self.completion_index, |this| this.bg(pal.tint))
                        .child(
                            div()
                                .w(px(132.))
                                .flex_none()
                                .font_family(MONO)
                                .text_size(px(12.5))
                                .child(item.label.clone()),
                        )
                        .child(ui::line(item.detail.clone()).flex_1().text_color(pal.muted))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.completion_index = at;
                            this.accept_completion(window, cx);
                        })),
                );
            }
            wrap = wrap.child(popup);
        }
        // the toolbar: who Enter reaches and how, then stop and send
        let target = target_name(snap);
        let target_label = if answering {
            format!("Answering {target}")
        } else {
            format!("To {target}")
        };
        let mut tools = div().flex().items_center().gap(px(6.)).mt(px(10.)).child(
            div()
                .flex()
                .items_center()
                .h(px(24.))
                .px(px(10.))
                .rounded_full()
                .bg(if answering { pal.buoy_soft } else { pal.tint })
                .text_size(px(12.))
                .child(target_label),
        );
        if !on_orch && !answering {
            let seg = |id: &'static str, label: &'static str, on: bool| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .px(px(10.))
                    .h(px(20.))
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .map(|this| {
                        if on {
                            this.bg(pal.sheet)
                                .text_color(pal.ink)
                                .shadow(pal.sheet_shadow())
                        } else {
                            this.text_color(pal.muted)
                        }
                    })
                    .child(label)
            };
            tools = tools.child(
                div()
                    .flex()
                    .p(px(2.))
                    .rounded_full()
                    .bg(pal.tint)
                    .border_1()
                    .border_color(pal.hair)
                    .child(
                        seg("mode-now", "Now", self.compose == Compose::Now).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.set_compose(Compose::Now, window, cx);
                            },
                        )),
                    )
                    .child(
                        seg(
                            "mode-after",
                            "After this step",
                            self.compose == Compose::After,
                        )
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.set_compose(Compose::After, window, cx);
                            },
                        )),
                    ),
            );
        }
        tools = tools.child(div().flex_1());
        if on_orch && snap.facts.turn_active {
            tools = tools.child(
                div()
                    .id("stop-turn")
                    .size(px(32.))
                    .rounded_full()
                    .border_1()
                    .border_color(pal.hair)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|this| this.bg(pal.tint))
                    .child(div().size(px(10.)).rounded(px(2.)).bg(pal.ink))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.send(
                            UiCmd::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                            cx,
                        );
                    })),
            );
        }
        tools = tools.child(
            div()
                .id("send")
                .size(px(32.))
                .rounded_full()
                .bg(if answering { pal.buoy } else { pal.ink })
                .text_color(if answering { gpui::white() } else { pal.sheet })
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .child("↑")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit(window, cx))),
        );
        wrap.child(
            div()
                .rounded(px(18.))
                .border_1()
                .border_color(if answering { pal.buoy } else { pal.hair })
                .bg(pal.sheet)
                .shadow(pal.sheet_shadow())
                .px(px(14.))
                .pt(px(12.))
                .pb(px(10.))
                .child(Textarea::new(&self.composer).appearance(false))
                .child(tools),
        )
    }

    fn changes(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let pane = ui::sheet(pal)
            .w(px(self.layout.changes))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden();
        let fold = fold_button("fold-changes", "›", pal)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(Pane::Changes, cx)));
        let Some(view) = &snap.patch else {
            // nothing selected: a line per worker
            let mut summary = div().flex().flex_col().px(px(10.));
            for (at, worker) in snap.workers.iter().enumerate() {
                let run_id = worker.row.key.clone();
                summary = summary.child(
                    div()
                        .id(("summary", at))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(10.))
                        .py(px(9.))
                        .rounded(px(12.))
                        .cursor_pointer()
                        .hover(|this| this.bg(pal.tint))
                        .child(ui::dot(pal.lane(worker.lane), 7.))
                        .child(ui::serif(worker.row.name.clone(), 15.).flex_1().min_w_0())
                        .children(
                            worker
                                .row
                                .diff_stat
                                .as_deref()
                                .map(|s| ui::stat_text(s, pal)),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.select_worker(&run_id, cx);
                        })),
                );
            }
            return pane
                .child(
                    div()
                        .flex()
                        .items_start()
                        .justify_between()
                        .pl(px(20.))
                        .pr(px(10.))
                        .pt(px(20.))
                        .pb(px(12.))
                        .child(
                            div().child(ui::display("Changes", 26.)).child(
                                div()
                                    .mt(px(4.))
                                    .text_size(px(12.5))
                                    .text_color(pal.muted)
                                    .child(if snap.workers.is_empty() {
                                        "A worker's diff shows here once it edits files."
                                    } else {
                                        "Pick a worker to see its diff."
                                    }),
                            ),
                        )
                        .child(fold),
                )
                .child(summary)
                .into_any_element();
        };
        let patch = &view.patch;
        let worker = snap
            .board
            .iter()
            .find(|w| w.row.key == view.run_id)
            .cloned();
        let mut actions = div().flex().items_center().gap(px(6.)).mt(px(12.));
        if let Some(worker) = worker {
            if let Some(path) = &worker.worktree {
                let path = std::path::PathBuf::from(path);
                actions = actions.child(
                    ui::pill("reveal", "Show in Finder", Tone::Plain, pal).on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.act(Act::Reveal(path.clone()), window, cx);
                        },
                    )),
                );
            }
            if worker.mergeable {
                let (id, name) = (worker.row.key.clone(), worker.row.name.clone());
                actions = actions.child(ui::pill("merge", "Merge", Tone::Ink, pal).on_click(
                    cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.act(Act::Merge(id.clone(), name.clone()), window, cx);
                    }),
                ));
            } else if matches!(worker.lane, Lane::Started | Lane::Waiting) {
                let id = worker.row.key.clone();
                actions =
                    actions.child(ui::pill("stop-worker", "Stop", Tone::Plain, pal).on_click(
                        cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.act(Act::StopWorker(id.clone()), window, cx);
                        }),
                    ));
            }
            let kind = MenuKind::Worker(Box::new(worker));
            actions = actions.child(more_button("worker-menu", pal).size(px(28.)).on_click(
                cx.listener(move |this, event: &ClickEvent, _, cx| {
                    this.open_menu(kind.clone(), event.position(), cx);
                }),
            ));
        }
        let files = patch.files.len();
        let head = div()
            .pl(px(20.))
            .pr(px(10.))
            .pt(px(18.))
            .pb(px(14.))
            .border_b_1()
            .border_color(pal.hair)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(8.))
                    .child(ui::serif(view.name.clone(), 22.).min_w_0())
                    .child(fold),
            )
            .child(
                div()
                    .mt(px(4.))
                    .flex()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(pal.muted)
                    .child(ui::stat(patch.added(), patch.removed(), pal))
                    .child(format!(
                        "in {files} file{}, against {}",
                        if files == 1 { "" } else { "s" },
                        view.base
                    )),
            )
            .child(actions);
        let body: AnyElement = if let Some(error) = &view.error {
            note(pal, error).into_any_element()
        } else if patch.files.is_empty() && patch.untracked.is_empty() {
            note(pal, "No changes yet.").into_any_element()
        } else {
            let count = self.changes.rows.len();
            let palette = *pal;
            uniform_list(
                "diff",
                count,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    let entity = cx.entity();
                    range
                        .map(|ix| {
                            let entity = entity.clone();
                            this.changes.render_row(ix, &palette, move |path, cx| {
                                entity.update(cx, |this: &mut Self, cx| {
                                    this.changes.toggle(&path);
                                    cx.notify();
                                });
                            })
                        })
                        .collect()
                }),
            )
            .track_scroll(&self.changes_scroll)
            // long lines scroll sideways instead of being cut off
            .with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(Some(self.changes.widest))
            .flex_1()
            .into_any_element()
        };
        pane.child(head).child(body).into_any_element()
    }

    /// No session is open: say so, and offer the one way to start.
    fn empty(&self, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let lead = if self.layout.sessions_open {
            "Start one, or pick one on the left. A session is one conversation with an orchestrator: it plans the work, runs the workers and reports back."
        } else {
            "A session is one conversation with an orchestrator: it plans the work, runs the workers and reports back."
        };
        ui::sheet(pal)
            .track_focus(&self.idle_focus)
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .child(ui::logo(96., pal))
            .child(ui::display("No session open", 44.))
            .child(
                div()
                    .max_w(px(440.))
                    .text_center()
                    .text_size(px(14.5))
                    .line_height(px(21.))
                    .text_color(pal.muted)
                    .child(lead),
            )
            .child(
                ui::pill("hero-new", "New session", Tone::Ink, pal)
                    .h(px(40.))
                    .px(px(22.))
                    .mt(px(8.))
                    .text_size(px(14.))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_new_session(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn menu_view(
        &self,
        menu: &Menu,
        snap: &Snapshot,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let item = |id: (&'static str, usize),
                    label: String,
                    detail: Option<String>,
                    danger: bool,
                    act: Act,
                    cx: &mut Context<Self>| {
            ui::menu_item(id, label, detail, danger, pal).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| this.act(act.clone(), window, cx),
            ))
        };
        let mut card = ui::menu(pal);
        match &menu.kind {
            MenuKind::Session { uuid, name } => {
                let uuid = *uuid;
                card = card.child(item(
                    ("m-rename", 0),
                    "Rename".into(),
                    None,
                    false,
                    Act::Rename(uuid, name.clone()),
                    cx,
                ));
                if snap.current() == Some(uuid) {
                    card = card
                        .child(item(
                            ("m-brief", 0),
                            "Show the orchestrator's brief".into(),
                            None,
                            false,
                            Act::Brief("orchestrator".into()),
                            cx,
                        ))
                        .child(item(
                            ("m-verbose", 0),
                            "Expand reasoning and tool output".into(),
                            Some("⌘O".into()),
                            false,
                            Act::Verbose,
                            cx,
                        ))
                        .child(item(
                            ("m-clear", 0),
                            "Clear the view".into(),
                            Some("The transcript file is kept".into()),
                            false,
                            Act::Command("/clear".into()),
                            cx,
                        ))
                        .child(item(
                            ("m-trim", 0),
                            "Trim the transcript".into(),
                            Some("Cut the file down to its recent tail".into()),
                            false,
                            Act::Command("/trim".into()),
                            cx,
                        ));
                }
                card = card
                    .child(ui::menu_separator(pal))
                    .child(item(
                        ("m-shutdown", 0),
                        "Shut down the orchestrator".into(),
                        Some("Its workers stop too; the session stays".into()),
                        false,
                        Act::Shutdown(uuid),
                        cx,
                    ))
                    .child(item(
                        ("m-remove", 0),
                        "Remove session…".into(),
                        Some("With its workers, worktrees and branches".into()),
                        true,
                        Act::RemoveSession(uuid),
                        cx,
                    ));
            }
            MenuKind::Model => {
                for (n, alias) in ORCHESTRATOR_MODEL_ALIASES.iter().enumerate() {
                    card = card.child(item(
                        ("m-model", n),
                        (*alias).to_string(),
                        None,
                        false,
                        Act::Command(format!("/model {alias}")),
                        cx,
                    ));
                }
                card = card.child(ui::menu_separator(pal)).child(item(
                    ("m-models", 0),
                    "Any model…".into(),
                    None,
                    false,
                    Act::Models,
                    cx,
                ));
            }
            MenuKind::Effort(levels) => {
                for (n, level) in levels.iter().enumerate() {
                    card = card.child(item(
                        ("m-effort", n),
                        capitalise(level),
                        None,
                        false,
                        Act::Command(format!("/thinking {level}")),
                        cx,
                    ));
                }
            }
            MenuKind::Permissions => {
                for (n, mode) in PERMISSION_MODES.iter().enumerate() {
                    card = card.child(item(
                        ("m-perm", n),
                        permission_label(mode),
                        Some(capitalise(describe_permission_mode(mode))),
                        false,
                        Act::Command(format!("/permissions {mode}")),
                        cx,
                    ));
                }
            }
            MenuKind::Worker(worker) => {
                let run_id = worker.row.key.clone();
                card = card.child(item(
                    ("w-brief", 0),
                    "Show its brief".into(),
                    None,
                    false,
                    Act::Brief(run_id.clone()),
                    cx,
                ));
                if let Some(branch) = &worker.row.branch {
                    card = card.child(item(
                        ("w-copy", 0),
                        "Copy branch name".into(),
                        Some(branch.clone()),
                        false,
                        Act::Copy(branch.clone()),
                        cx,
                    ));
                }
                card = card.child(item(
                    ("w-fold", 0),
                    "Fold or unfold every file".into(),
                    None,
                    false,
                    Act::FoldAll,
                    cx,
                ));
                card = card.child(ui::menu_separator(pal)).child(item(
                    ("w-remove", 0),
                    "Remove worker…".into(),
                    Some("Its worktree and branch go too".into()),
                    true,
                    Act::RemoveWorker(run_id),
                    cx,
                ));
            }
        }
        deferred(
            anchored()
                .position(menu.at)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .id("menu")
                        .occlude()
                        .mt(px(6.))
                        .child(ui::rise(card, "menu-rise")),
                ),
        )
        .with_priority(2)
        .into_any_element()
    }

    fn dialog_view(&self, dialog: &Dialog, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (title, body, confirm, input) = match dialog {
            Dialog::NewSession => (
                "New session".to_string(),
                "A new conversation with its own orchestrator and workers. Without a name it goes by its short id until you rename it.",
                "Start session",
                true,
            ),
            Dialog::Rename(_) => (
                "Rename session".to_string(),
                "The name shows here and in /session. The session's files stay where they are.",
                "Rename",
                true,
            ),
            Dialog::Merge { name, .. } => (
                format!("Merge {name}?"),
                "Its branch merges into the checkout it was cut from. If the merge conflicts it is abandoned and the checkout is left clean.",
                "Merge",
                false,
            ),
        };
        let card = ui::sheet(pal)
            .w(px(460.))
            .p(px(26.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(ui::display(title, 30.))
            .child(
                div()
                    .text_size(px(13.5))
                    .line_height(px(20.))
                    .text_color(pal.muted)
                    .child(body),
            )
            .when(input, |this| {
                this.child(
                    div()
                        .mt(px(10.))
                        .px(px(12.))
                        .py(px(6.))
                        .rounded(px(12.))
                        .border_1()
                        .border_color(pal.ink)
                        .child(Input::new(&self.dialog_input).appearance(false)),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .mt(px(14.))
                    .child(
                        ui::pill("dialog-ok", confirm, Tone::Ink, pal)
                            .h(px(36.))
                            .px(px(18.))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.confirm_dialog(window, cx);
                            })),
                    )
                    .child(
                        ui::pill("dialog-cancel", "Cancel", Tone::Plain, pal)
                            .h(px(36.))
                            .px(px(18.))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.close_dialog(window, cx);
                            })),
                    ),
            );
        div()
            .id("dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(140.))
            .bg(tint(gpui::black(), if pal.dark { 0.5 } else { 0.16 }))
            .occlude()
            .track_focus(&self.dialog_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.close_dialog(window, cx),
                    "enter" => this.confirm_dialog(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                this.close_dialog(window, cx);
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_dialog(window, cx)),
            )
            .child(
                div()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(ui::rise(card, "dialog-rise")),
            )
            .into_any_element()
    }
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = super::palette(window);
        let snap = self.snap(cx);
        let sheet =
            super::sheets::render(&snap, &pal, &self.shared.read(cx).cmds.clone(), window, cx);
        let mut panes = div()
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .px(px(GAP))
            .pb(px(GAP))
            .child(self.sessions(&snap, &pal, cx));
        if self.layout.sessions_open {
            panes = panes.child(self.grip(Pane::Sessions, &pal, cx));
        } else {
            panes = panes.child(div().w(px(GAP)).flex_none());
        }
        if snap.key.is_none() {
            panes = panes.child(self.empty(&pal, cx));
        } else {
            if self.layout.workers_open {
                panes = panes.child(self.workers(&snap, &pal, cx)).child(self.grip(
                    Pane::Workers,
                    &pal,
                    cx,
                ));
            }
            panes = panes.child(self.chat(&snap, &pal, cx));
            if self.layout.changes_open {
                panes = panes
                    .child(self.grip(Pane::Changes, &pal, cx))
                    .child(self.changes(&snap, &pal, cx));
            }
        }
        let menu = self
            .menu
            .as_ref()
            .map(|menu| self.menu_view(menu, &snap, &pal, cx));
        let dialog = self
            .dialog
            .clone()
            .map(|dialog| self.dialog_view(&dialog, &pal, cx));
        div()
            .id("main")
            // keys for a sheet: the console's overlay reads them the way the
            // TUI's does, in the order they were typed
            .track_focus(&self.sheet_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.menu.is_some() && event.keystroke.key == "escape" {
                    this.menu = None;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if !this.sheet_focus.is_focused(window) {
                    return;
                }
                let keystroke = &event.keystroke;
                if keystroke.modifiers.platform && keystroke.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        this.send(UiCmd::Paste(text), cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                if let Some(key) = super::keys::to_crossterm(keystroke) {
                    this.send(UiCmd::Key(key), cx);
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .bg(pal.canvas)
            .text_color(pal.ink)
            .font_family(SANS)
            .text_size(px(13.5))
            .on_action(cx.listener(|this, _: &super::ToggleBoard, window, cx| {
                this.toggle_board(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ToggleSessions, _, cx| this.toggle(Pane::Sessions, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleWorkers, _, cx| this.toggle(Pane::Workers, cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| this.toggle(Pane::Changes, cx)))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.open_sheet(UiCmd::Action(KeyAction::OpenPalette), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSearch, window, cx| {
                this.open_sheet(UiCmd::Action(KeyAction::Search), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenFleet, window, cx| {
                this.open_sheet(UiCmd::Action(KeyAction::OpenFleet), window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleVerbose, _, cx| {
                this.send(UiCmd::Action(KeyAction::ToggleVerbose), cx);
            }))
            .on_action(cx.listener(|this, _: &OpenHelp, window, cx| {
                this.open_sheet(UiCmd::Submit("/help".into()), window, cx);
            }))
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, cx| this.on_drag(event, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.drag.take().is_some() {
                        this.save_layout();
                        cx.notify();
                    }
                }),
            )
            .child(self.title_bar(&pal, cx))
            .child(panes)
            .when(self.board_open, |this| {
                let handlers = self.board_handlers(cx);
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .track_focus(&self.board_focus)
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                this.toggle_board(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(super::board::render(
                            &snap,
                            &pal,
                            self.board_filter,
                            &handlers,
                        )),
                )
            })
            // a sheet the console raises by itself sits over a dialog
            .when_some(dialog, |this, dialog| this.child(dialog))
            .when_some(sheet, |this, sheet| this.child(sheet))
            .when(menu.is_some(), |this| {
                this.child(
                    div()
                        .id("menu-catcher")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.menu = None;
                                cx.notify();
                            }),
                        ),
                )
            })
            .children(menu)
    }
}

/// The chips under the title: what the selected row runs on, each a menu.
fn chips(
    snap: &Snapshot,
    worker: Option<&WorkerItem>,
    pal: &Palette,
    cx: &mut Context<MainView>,
) -> impl IntoElement {
    let mut chips = div().flex().flex_wrap().items_center().gap(px(6.));
    if snap.selected == "orchestrator" {
        let model = snap.facts.model.clone().unwrap_or_else(|| "Model".into());
        let effort = snap.facts.effort.as_deref().map_or_else(
            || "Thinking".to_string(),
            |effort| format!("{} thinking", capitalise(effort)),
        );
        chips = chips
            .child(
                ui::chip("chip-model", model, true, pal).on_click(cx.listener(
                    |this, event: &ClickEvent, _, cx| {
                        this.open_menu(MenuKind::Model, event.position(), cx);
                    },
                )),
            )
            .child(
                ui::chip("chip-effort", effort, true, pal).on_click(cx.listener(
                    |this, event: &ClickEvent, _, cx| {
                        let levels = CLAUDE_EFFORT_LEVELS
                            .iter()
                            .map(|l| (*l).to_string())
                            .collect();
                        this.open_menu(MenuKind::Effort(levels), event.position(), cx);
                    },
                )),
            )
            .child(
                ui::chip(
                    "chip-perms",
                    permission_label(&snap.facts.permission_mode),
                    true,
                    pal,
                )
                .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                    this.open_menu(MenuKind::Permissions, event.position(), cx);
                })),
            );
        if snap.facts.cost_usd > 0.0 {
            chips = chips.child(
                div()
                    .px(px(6.))
                    .text_size(px(12.))
                    .text_color(pal.muted)
                    .child(format!("${:.2} so far", snap.facts.cost_usd)),
            );
        }
    } else if let Some(worker) = worker {
        let levels = worker.thinking_levels.clone();
        let thinking = worker.thinking.as_deref().map_or_else(
            || "Thinking".to_string(),
            |level| format!("{} thinking", capitalise(level)),
        );
        chips = chips
            .child(
                ui::chip(
                    "chip-wmodel",
                    worker.model.clone().unwrap_or_else(|| "Model".into()),
                    true,
                    pal,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.act(Act::Models, window, cx);
                })),
            )
            .when(!levels.is_empty(), |this| {
                this.child(
                    ui::chip("chip-wthinking", thinking, true, pal).on_click(cx.listener(
                        move |this, event: &ClickEvent, _, cx| {
                            this.open_menu(MenuKind::Effort(levels.clone()), event.position(), cx);
                        },
                    )),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(6.))
                    .text_size(px(12.))
                    .text_color(pal.muted)
                    .child(ui::dot(pal.lane(worker.lane), 7.))
                    .child(lane_label(worker.lane)),
            );
    }
    chips
}

// -- small pieces -----------------------------------------------------------

fn target_name(snap: &Snapshot) -> String {
    if snap.selected == "orchestrator" {
        return "the orchestrator".to_string();
    }
    snap.workers
        .iter()
        .find(|w| w.row.key == snap.selected)
        .map_or_else(|| "the worker".to_string(), |w| w.row.name.clone())
}

/// The blocks a running search matches.
fn search_matches(snap: &Snapshot) -> Vec<usize> {
    match &snap.overlay {
        Some(crate::tui::app::Overlay::Search(search)) => search.matches.clone(),
        _ => Vec::new(),
    }
}

fn end_of(text: &str) -> gpui_component::input::Position {
    let line = text.lines().count().saturating_sub(1);
    let last = text.rsplit('\n').next().unwrap_or("");
    gpui_component::input::Position::new(
        u32::try_from(line).unwrap_or(0),
        u32::try_from(last.chars().count()).unwrap_or(0),
    )
}

fn ago(age: &str) -> String {
    if age.is_empty() {
        "just now".into()
    } else {
        format!("{age} ago")
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

/// A permission mode in words.
fn permission_label(mode: &str) -> String {
    match mode {
        "default" => "Ask before acting",
        "auto" => "Auto-approve routine",
        "acceptEdits" => "Accept edits",
        "dontAsk" => "Never ask",
        "plan" => "Plan only",
        other => other,
    }
    .to_string()
}

const fn lane_label(lane: Lane) -> &'static str {
    match lane {
        Lane::Started => "Running",
        Lane::Waiting => "Waiting on you",
        Lane::Failed => "Failed",
        Lane::Finished => "Finished",
    }
}

fn fold_button(id: &'static str, text: &'static str, pal: &Palette) -> gpui::Stateful<gpui::Div> {
    let ink = pal.ink;
    div()
        .id(id)
        .size(px(26.))
        .flex_none()
        .rounded(px(8.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_size(px(15.))
        .text_color(pal.muted)
        .hover(move |this| this.bg(tint(ink, 0.06)).text_color(ink))
        .child(text)
}

fn more_button(id: impl Into<gpui::ElementId>, pal: &Palette) -> gpui::Stateful<gpui::Div> {
    let ink = pal.ink;
    div()
        .id(id)
        .size(px(24.))
        .flex_none()
        .rounded(px(8.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(pal.muted)
        .hover(move |this| this.bg(tint(ink, 0.06)).text_color(ink))
        .child("⋯")
}

fn note(pal: &Palette, text: &str) -> gpui::Div {
    div()
        .p(px(20.))
        .max_w(px(440.))
        .text_size(px(13.5))
        .line_height(px(20.))
        .text_color(pal.muted)
        .child(text.to_string())
}
