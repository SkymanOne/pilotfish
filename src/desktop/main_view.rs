//! The main window: sessions, the open session's workers, the chat, and the
//! selected worker's changes, left to right. Every border drags; every
//! sidebar folds (the sessions sidebar to a rail of lights).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, ListAlignment, ListState, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, div, list,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::TitleBar;
use gpui_component::input::{
    Enter, Escape, IndentInline, InputEvent, MoveDown, MoveUp, Textarea, TextareaState,
};
use serde::{Deserialize, Serialize};

use super::backend::{Lane, Snapshot, UiCmd, WorkerItem};
use super::changes::{ChangesState, stat};
use super::chat::{self, Group, light, ring};
use super::theme::{MONO, Palette, SANS};
use super::{
    OpenFleet, OpenHelp, OpenPalette, OpenSearch, Shared, ToggleChanges, ToggleSessions,
    ToggleVerbose, ToggleWorkers,
};
use crate::orch::session::MonitorHealth;
use crate::tui::completions::{CompletionState, apply_suggestion, completions_for};
use crate::tui::keys::KeyAction;

const RAIL: f32 = 44.;
const GRIP: f32 = 5.;

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
            sessions: 212.,
            workers: 244.,
            changes: 380.,
            sessions_open: true,
            workers_open: true,
            changes_open: true,
        }
    }
}

impl Layout {
    const fn range(pane: Pane) -> (f32, f32) {
        match pane {
            Pane::Sessions => (160., 360.),
            Pane::Workers => (180., 420.),
            Pane::Changes => (280., 760.),
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

pub struct MainView {
    shared: Entity<Shared>,
    title: String,
    layout_path: std::path::PathBuf,
    layout: Layout,
    drag: Option<Drag>,
    composer: Entity<TextareaState>,
    completion: Option<CompletionState>,
    completion_index: usize,
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
    awaiting_sheet: Option<std::time::Instant>,
    /// Who the composer is talking to, for its placeholder.
    target: String,
    /// The Board popup, and the session its chips narrow it to.
    board_open: bool,
    board_filter: Option<uuid::Uuid>,
    board_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(
        shared: Entity<Shared>,
        title: String,
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
            title,
            layout_path,
            layout,
            drag: None,
            composer,
            completion: None,
            completion_index: 0,
            chat,
            groups: Arc::new(Vec::new()),
            changes: ChangesState::default(),
            changes_scroll: gpui::UniformListScrollHandle::new(),
            worker_tab: None,
            sheet_focus: cx.focus_handle(),
            had_overlay: false,
            awaiting_sheet: None,
            target: String::new(),
            board_open: false,
            board_filter: None,
            board_focus: cx.focus_handle(),
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
            .is_some_and(|at| at.elapsed() > std::time::Duration::from_secs(1));
        if open {
            self.awaiting_sheet = None;
        }
        if open && !self.had_overlay {
            window.focus(&self.sheet_focus, cx);
        } else if !open && (self.had_overlay || lapsed) {
            self.awaiting_sheet = None;
            self.composer
                .update(cx, |state, cx| state.focus(window, cx));
        }
        self.had_overlay = open;
        // the placeholder names whoever Enter will reach
        let target = if snap.selected == "orchestrator" {
            "the orchestrator".to_string()
        } else {
            snap.workers
                .iter()
                .find(|w| w.row.key == snap.selected)
                .map_or_else(|| "the worker".to_string(), |w| w.row.name.clone())
        };
        if target != self.target {
            let text = format!("Message {target}. / for commands, @ for workers and files");
            self.composer
                .update(cx, |state, cx| state.set_placeholder(text, window, cx));
            self.target = target;
        }
        cx.notify();
    }

    // -- composer ----------------------------------------------------------

    fn recompute_completion(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().to_string();
        let snap = self.snap(cx);
        self.completion = completions_for(&text, &snap.completion).filter(|c| !c.items.is_empty());
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
        self.send(UiCmd::Submit(text.to_string()), cx);
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.completion = None;
        cx.notify();
    }

    /// The blocks a running search matches.
    fn search_matches(&self, snap: &Snapshot) -> Vec<usize> {
        match &snap.overlay {
            Some(crate::tui::app::Overlay::Search(search)) => search.matches.clone(),
            _ => Vec::new(),
        }
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion.take().is_some() {
            cx.notify();
            return;
        }
        if !self.composer.read(cx).value().is_empty() {
            self.composer
                .update(cx, |state, cx| state.set_value("", window, cx));
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

    fn grip(&self, pane: Pane, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let dragging = self.drag.as_ref().is_some_and(|d| d.pane == pane);
        div()
            .id(SharedString::from(format!("grip-{pane:?}")))
            .w(px(GRIP))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .flex()
            .justify_center()
            .child(
                div()
                    .w(px(if dragging { 2. } else { 1. }))
                    .h_full()
                    .bg(if dragging { pal.accent } else { pal.line }),
            )
            .hover(|this| this.bg(super::theme::tint(pal.accent, 0.25)))
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

    // -- panes -------------------------------------------------------------

    fn title_bar(&self, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let toggle =
            |id: &'static str, open: bool, at: f32, action: Pane, cx: &mut Context<Self>| {
                div()
                    .id(id)
                    .w(px(26.))
                    .h(px(18.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(pal.line)
                    .when(open, |this| this.bg(pal.raised))
                    .cursor_pointer()
                    .relative()
                    .child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .bottom(px(2.))
                            .left(px(at))
                            .w(px(6.))
                            .rounded(px(1.))
                            .bg(if open { pal.text } else { pal.muted }),
                    )
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(action, cx)),
                    )
            };
        let button = |id: &'static str, text: &'static str, keys: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(9.))
                .py(px(2.))
                .rounded(px(6.))
                .border_1()
                .border_color(pal.line)
                .bg(pal.raised)
                .cursor_pointer()
                .text_size(px(12.))
                .child(text)
                .child(div().text_color(pal.muted).text_size(px(11.)).child(keys))
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
                        .gap(px(8.))
                        .items_baseline()
                        .text_size(px(13.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("pilotfish"))
                        .child(div().text_color(pal.muted).child(self.title.clone())),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
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
                            9.,
                            Pane::Workers,
                            cx,
                        ))
                        .child(toggle(
                            "t-changes",
                            self.layout.changes_open,
                            16.,
                            Pane::Changes,
                            cx,
                        ))
                        .child(div().w(px(6.)))
                        .child(button("b-commands", "Commands", "⌘K").on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| {
                                this.open_sheet(UiCmd::Action(KeyAction::OpenPalette), window, cx);
                            },
                        )))
                        .child(button("b-board", "Board", "⌘B").on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.toggle_board(window, cx),
                        ))),
                ),
        )
    }

    fn sessions(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if !self.layout.sessions_open {
            let mut rail = div()
                .w(px(RAIL))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(14.))
                .pt(px(14.))
                .bg(pal.panel);
            for (at, session) in snap.sessions.iter().enumerate() {
                let key = session.key.clone();
                let current = Some(session.key.uuid) == snap.current();
                rail = rail.child(
                    div()
                        .id(("rail", at))
                        .p(px(3.))
                        .rounded_full()
                        .cursor_pointer()
                        .when(current, |this| this.border_1().border_color(pal.accent))
                        .child(health_light(session.health, pal, 10.))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.open_session(key.clone(), cx);
                        })),
                );
            }
            return rail.into_any_element();
        }
        let mut list = div()
            .id("session-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(1.))
            .px(px(6.));
        for (at, session) in snap.sessions.iter().enumerate() {
            let current = Some(session.key.uuid) == snap.current();
            let key = session.key.clone();
            let meta = match session.lanes.len() {
                0 => format!("no workers, {}", ago(&session.last_used)),
                1 => format!("1 worker, {}", ago(&session.last_used)),
                n => format!("{n} workers, {}", ago(&session.last_used)),
            };
            let mut lights = div().flex().gap(px(4.)).pt(px(6.));
            for lane in session.lanes.iter().take(8) {
                lights = lights.child(lane_light(*lane, pal, 6.));
            }
            list = list.child(
                row(("session", at), current, pal)
                    .child(
                        div()
                            .pt(px(5.))
                            .child(health_light(session.health, pal, 9.)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(truncate(
                                div().font_weight(FontWeight::MEDIUM),
                                &session.name,
                            ))
                            .child(truncate(
                                div().text_size(px(12.)).text_color(pal.muted),
                                &meta,
                            )),
                    )
                    .child(lights)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_session(key.clone(), cx);
                    })),
            );
        }
        if snap.sessions.is_empty() {
            list = list.child(empty(pal, "No sessions yet. Send a message to start one."));
        }
        div()
            .w(px(self.layout.sessions))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(pal.panel)
            .child(
                pane_head(pal).child("Sessions").child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .child(icon_button("new-session", "+", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.send(UiCmd::Submit("/session new".into()), cx);
                            },
                        )))
                        .child(icon_button("fold-sessions", "‹", pal).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| this.toggle(Pane::Sessions, cx),
                        ))),
                ),
            )
            .child(list)
            .into_any_element()
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

    /// Ask the console for a sheet and hand it the keys straight away.
    fn open_sheet(&mut self, cmd: UiCmd, window: &mut Window, cx: &mut Context<Self>) {
        self.send(cmd, cx);
        self.awaiting_sheet = Some(std::time::Instant::now());
        window.focus(&self.sheet_focus, cx);
    }

    fn toggle_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.board_open = !self.board_open;
        if self.board_open {
            window.focus(&self.board_focus, cx);
        } else {
            self.composer
                .update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn board_handlers(&self, cx: &mut Context<Self>) -> super::board::Handlers {
        let entity = cx.entity();
        let filter = entity.clone();
        let open = entity.clone();
        let close = entity;
        super::board::Handlers {
            filter: std::rc::Rc::new(move |value, _, cx| {
                filter.update(cx, |this, cx| {
                    this.board_filter = value;
                    cx.notify();
                });
            }),
            open: std::rc::Rc::new(move |item: WorkerItem, window, cx| {
                open.update(cx, |this, cx| {
                    let snap = this.snap(cx);
                    if snap.current() != Some(item.session.uuid) {
                        this.send(UiCmd::Submit(format!("/session {}", item.session.uuid)), cx);
                    }
                    this.select_worker(&item.row.key, cx);
                    this.board_open = false;
                    this.composer
                        .update(cx, |state, cx| state.focus(window, cx));
                });
            }),
            close: std::rc::Rc::new(move |(), window, cx| {
                close.update(cx, |this, cx| {
                    if this.board_open {
                        this.toggle_board(window, cx);
                    }
                });
            }),
        }
    }

    fn select_worker(&mut self, run_id: &str, cx: &mut Context<Self>) {
        self.worker_tab = Some(run_id.to_string());
        self.send(UiCmd::Select(run_id.to_string()), cx);
        self.send(UiCmd::Diff(Some(run_id.to_string())), cx);
        cx.notify();
    }

    fn workers(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let name = snap
            .sessions
            .iter()
            .find(|s| Some(s.key.uuid) == snap.current())
            .map_or_else(|| "Session".to_string(), |s| s.name.clone());
        let short: String = snap
            .current()
            .map(|uuid| uuid.to_string().chars().take(7).collect())
            .unwrap_or_default();
        let mut list = div()
            .id("worker-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(1.))
            .px(px(6.));
        // the orchestrator first
        if let Some(orch) = snap.rows.first() {
            let color = if snap.facts.exited {
                pal.fail
            } else if !snap.requests.is_empty() {
                pal.wait
            } else if snap.facts.turn_active {
                pal.run
            } else {
                pal.muted
            };
            list = list.child(
                row("orchestrator", snap.selected == "orchestrator", pal)
                    .child(div().pt(px(5.)).child(light(color, pal.dark, 9.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(truncate(
                                div().font_weight(FontWeight::MEDIUM),
                                "Orchestrator",
                            ))
                            .child(truncate(
                                div().text_size(px(12.)).text_color(pal.muted),
                                if orch.detail.is_empty() {
                                    "idle"
                                } else {
                                    &orch.detail
                                },
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(pal.muted)
                            .child(snap.facts.model.clone().unwrap_or_default()),
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.send(UiCmd::Select("orchestrator".into()), cx);
                    })),
            );
            list = list.child(div().h(px(1.)).mx(px(6.)).my(px(6.)).bg(pal.line));
        }
        let mut workers: Vec<&WorkerItem> = snap.workers.iter().collect();
        workers.sort_by_key(|w| match w.lane {
            Lane::Waiting => 0,
            Lane::Failed => 1,
            Lane::Started => 2,
            Lane::Finished => 3,
        });
        for (at, worker) in workers.iter().enumerate() {
            let run_id = worker.row.key.clone();
            list = list.child(
                row(("worker", at), snap.selected == run_id, pal)
                    .child(div().pt(px(5.)).child(lane_light(worker.lane, pal, 9.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(truncate(
                                div().font_weight(FontWeight::MEDIUM),
                                &worker.row.name,
                            ))
                            .child(truncate(
                                div().text_size(px(12.)).text_color(pal.muted),
                                &worker.row.detail,
                            )),
                    )
                    .children(worker.row.diff_stat.as_deref().map(|s| diff_stat(s, pal)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select_worker(&run_id, cx);
                    })),
            );
        }
        if workers.is_empty() {
            list = list.child(empty(
                pal,
                "No workers yet. Ask the orchestrator to split the work.",
            ));
        }
        div()
            .w(px(self.layout.workers))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(pal.panel)
            .child(
                pane_head(pal)
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .items_baseline()
                            .min_w_0()
                            .child(truncate(div(), &name))
                            .child(
                                div()
                                    .text_color(pal.muted)
                                    .font_weight(FontWeight::NORMAL)
                                    .child(short),
                            ),
                    )
                    .child(icon_button("fold-workers", "‹", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(Pane::Workers, cx)),
                    )),
            )
            .child(list)
            .into_any_element()
    }

    fn chat(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let on_orch = snap.selected == "orchestrator";
        let tab = |id: &'static str, text: String, color: Hsla, on: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(7.))
                .px(px(12.))
                .h_full()
                .flex_none()
                .whitespace_nowrap()
                .cursor_pointer()
                .border_b_2()
                .border_color(if on {
                    pal.accent
                } else {
                    gpui::transparent_black()
                })
                .text_color(if on { pal.text } else { pal.muted })
                .child(light(color, pal.dark, 8.))
                .child(text)
        };
        let orch_color = if snap.facts.turn_active {
            pal.run
        } else {
            pal.muted
        };
        let mut tabs = div()
            .flex()
            .items_center()
            .h(px(40.))
            .flex_none()
            .px(px(12.))
            .gap(px(2.))
            .border_b_1()
            .border_color(pal.line)
            .child(
                tab("tab-orch", "Orchestrator".into(), orch_color, on_orch).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.send(UiCmd::Select("orchestrator".into()), cx);
                    },
                )),
            );
        if let Some(run_id) = &self.worker_tab
            && let Some(worker) = snap.workers.iter().find(|w| &w.row.key == run_id)
        {
            let id = run_id.clone();
            let close = run_id.clone();
            tabs = tabs.child(
                tab(
                    "tab-worker",
                    worker.row.name.clone(),
                    pal.lane(worker.lane),
                    !on_orch,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.send(UiCmd::Select(id.clone()), cx);
                }))
                .child(
                    div()
                        .id("tab-close")
                        .px(px(3.))
                        .text_color(pal.muted)
                        .child("×")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            if this.snap(cx).selected == close {
                                this.send(UiCmd::Select("orchestrator".into()), cx);
                            }
                            this.worker_tab = None;
                            cx.notify();
                        })),
                ),
            );
        }
        let mut facts = div()
            .ml_auto()
            .flex()
            .gap(px(6.))
            .min_w_0()
            .overflow_hidden();
        if let Some(model) = &snap.facts.model {
            let effort = snap
                .facts
                .effort
                .as_deref()
                .map(|e| format!(", {e}"))
                .unwrap_or_default();
            facts = facts.child(chip(pal, format!("{model}{effort}")));
        }
        if snap.facts.cost_usd > 0.0 {
            facts = facts.child(chip(pal, format!("${:.2}", snap.facts.cost_usd)));
        }
        tabs = tabs.child(facts);

        let groups = self.groups.clone();
        let palette = *pal;
        let matches = self.search_matches(snap);
        let body: AnyElement = if groups.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(empty(
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
                        chat::render(ix, group, matched, &palette, window, cx)
                    },
                )
            })
            .flex_1()
            .py(px(10.))
            .into_any_element()
        };
        let status = snap
            .flash
            .as_ref()
            .map(|flash| {
                (
                    flash.text.clone(),
                    if flash.error { pal.fail } else { pal.muted },
                )
            })
            .or_else(|| snap.activity.clone().map(|a| (a, pal.muted)));
        div()
            .flex_1()
            .min_w(px(360.))
            .h_full()
            .flex()
            .flex_col()
            .bg(pal.bg)
            .child(tabs)
            .child(body)
            .when_some(status, |this, (text, color)| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(22.))
                        .py(px(6.))
                        .text_size(px(12.))
                        .text_color(color)
                        .when(snap.facts.turn_active, |this| {
                            this.child(light(pal.run, pal.dark, 7.))
                        })
                        .child(truncate(div(), &text)),
                )
            })
            .child(self.composer(snap, pal, cx))
            .into_any_element()
    }

    fn composer(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let answering = snap.prompt.starts_with("answer");
        let placeholder_target = if snap.selected == "orchestrator" {
            None
        } else {
            snap.workers
                .iter()
                .find(|w| w.row.key == snap.selected)
                .map(|w| w.row.name.clone())
        };
        let mut wrap = div()
            .relative()
            .px(px(16.))
            .pb(px(16.))
            .pt(px(4.))
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
            let mut popup = div()
                .absolute()
                .left(px(16.))
                .right(px(16.))
                .bottom_full()
                .mb(px(-2.))
                .p(px(4.))
                .rounded(px(9.))
                .border_1()
                .border_color(pal.line)
                .bg(pal.raised)
                .shadow_lg()
                .flex()
                .flex_col();
            for (at, item) in completion.items.iter().enumerate().take(8) {
                popup = popup.child(
                    div()
                        .id(("suggestion", at))
                        .flex()
                        .gap(px(12.))
                        .px(px(10.))
                        .py(px(5.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(at == self.completion_index, |this| this.bg(pal.select))
                        .child(
                            div()
                                .w(px(120.))
                                .flex_none()
                                .font_weight(FontWeight::MEDIUM)
                                .child(item.label.clone()),
                        )
                        .child(truncate(
                            div().flex_1().min_w_0().text_color(pal.muted),
                            &item.detail,
                        ))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.completion_index = at;
                            this.accept_completion(window, cx);
                        })),
                );
            }
            wrap = wrap.child(popup);
        }
        let hint = match (&placeholder_target, answering) {
            (_, true) => format!("Answering {}", snap.prompt.trim_end_matches(['>', ' '])),
            (Some(name), false) => format!("Steering {name}"),
            (None, false) => "Tab completes, Enter sends, Shift-Enter adds a line".to_string(),
        };
        wrap.child(
            div()
                .rounded(px(9.))
                .border_1()
                .border_color(pal.accent)
                .bg(pal.raised)
                .px(px(12.))
                .pt(px(8.))
                .pb(px(6.))
                .child(Textarea::new(&self.composer).appearance(false))
                .child(div().text_size(px(12.)).text_color(pal.muted).child(hint)),
        )
    }

    fn changes(&self, snap: &Snapshot, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let pane = div()
            .w(px(self.layout.changes))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(pal.bg);
        let Some(view) = &snap.patch else {
            // nothing selected: a line per worker
            let mut summary = div().flex().flex_col();
            for (at, worker) in snap.workers.iter().enumerate() {
                let run_id = worker.row.key.clone();
                summary = summary.child(
                    div()
                        .id(("summary", at))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(14.))
                        .py(px(7.))
                        .border_b_1()
                        .border_color(pal.line)
                        .cursor_pointer()
                        .child(lane_light(worker.lane, pal, 8.))
                        .child(truncate(div().flex_1().min_w_0(), &worker.row.name))
                        .children(worker.row.diff_stat.as_deref().map(|s| diff_stat(s, pal)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.select_worker(&run_id, cx);
                        })),
                );
            }
            return pane
                .child(pane_head(pal).child("Changes").child(
                    icon_button("fold-changes", "›", pal).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(Pane::Changes, cx)),
                    ),
                ))
                .child(
                    div()
                        .px(px(14.))
                        .pb(px(8.))
                        .text_size(px(12.))
                        .text_color(pal.muted)
                        .child(if snap.workers.is_empty() {
                            "Changes appear here once a worker edits files."
                        } else {
                            "Select a worker to see its diff."
                        }),
                )
                .child(summary)
                .into_any_element();
        };
        let patch = &view.patch;
        let head = div()
            .px(px(14.))
            .pt(px(12.))
            .pb(px(10.))
            .border_b_1()
            .border_color(pal.line)
            .flex()
            .items_start()
            .justify_between()
            .child(
                div()
                    .min_w_0()
                    .child(truncate(
                        div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD),
                        &view.name,
                    ))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(pal.muted)
                            .child(stat(patch.added(), patch.removed(), pal))
                            .child(format!(
                                "in {} file{}, against {}",
                                patch.files.len(),
                                if patch.files.len() == 1 { "" } else { "s" },
                                view.base
                            )),
                    ),
            )
            .child(icon_button("fold-changes", "›", pal).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| this.toggle(Pane::Changes, cx)),
            ));
        let body: AnyElement = if let Some(error) = &view.error {
            empty(pal, error).into_any_element()
        } else if patch.files.is_empty() && patch.untracked.is_empty() {
            empty(pal, "No changes yet.").into_any_element()
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
            .child(self.sessions(&snap, &pal, cx));
        if self.layout.sessions_open {
            panes = panes.child(self.grip(Pane::Sessions, &pal, cx));
        } else {
            panes = panes.child(div().w(px(1.)).h_full().bg(pal.line));
        }
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
        div()
            .id("main")
            // keys for a sheet: the console's overlay reads them the way the
            // TUI's does, in the order they were typed
            .track_focus(&self.sheet_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
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
            .bg(pal.bg)
            .text_color(pal.text)
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
            .when_some(sheet, |this, sheet| this.child(sheet))
    }
}

// -- small pieces -----------------------------------------------------------

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

fn pane_head(pal: &Palette) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .px(px(12.))
        .pt(px(12.))
        .pb(px(8.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(13.))
        .text_color(pal.text)
}

fn row(id: impl Into<gpui::ElementId>, selected: bool, pal: &Palette) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_start()
        .gap(px(9.))
        .px(px(10.))
        .py(px(7.))
        .rounded(px(7.))
        .border_l_2()
        .cursor_pointer()
        .border_color(if selected {
            pal.accent
        } else {
            gpui::transparent_black()
        })
        .when(selected, |this| this.bg(pal.raised))
        .hover(|this| this.bg(pal.raised))
}

fn truncate(base: gpui::Div, text: &str) -> gpui::Div {
    base.overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.to_string())
}

fn icon_button(id: &'static str, text: &'static str, pal: &Palette) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(22.))
        .h(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .border_1()
        .border_color(pal.line)
        .cursor_pointer()
        .text_color(pal.text)
        .font_weight(FontWeight::NORMAL)
        .hover(|this| this.bg(pal.raised))
        .child(text)
}

fn chip(pal: &Palette, text: String) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(8.))
        .rounded_full()
        .border_1()
        .border_color(pal.line)
        .text_size(px(12.))
        .text_color(pal.muted)
        .whitespace_nowrap()
        .child(text)
}

fn empty(pal: &Palette, text: &str) -> gpui::Div {
    div()
        .p(px(18.))
        .max_w(px(420.))
        .text_color(pal.muted)
        .child(text.to_string())
}

fn lane_light(lane: Lane, pal: &Palette, size: f32) -> AnyElement {
    match lane {
        Lane::Finished => ring(pal.done, size, false).into_any_element(),
        lane => light(pal.lane(lane), pal.dark, size).into_any_element(),
    }
}

fn health_light(health: MonitorHealth, pal: &Palette, size: f32) -> AnyElement {
    match health {
        MonitorHealth::Running => light(pal.run, pal.dark, size).into_any_element(),
        MonitorHealth::Wedged => light(pal.wait, pal.dark, size).into_any_element(),
        MonitorHealth::Stopped => ring(pal.muted, size, true).into_any_element(),
    }
}

/// A row's `+12 −3`, coloured.
fn diff_stat(text: &str, pal: &Palette) -> AnyElement {
    let mut parts = text.split_whitespace();
    let added = parts.next().unwrap_or("").to_string();
    let removed = parts.next().unwrap_or("").to_string();
    div()
        .flex()
        .flex_none()
        .gap(px(4.))
        .text_size(px(12.))
        .font_family(MONO)
        .child(div().text_color(pal.run).child(added))
        .child(div().text_color(pal.fail).child(removed))
        .into_any_element()
}
