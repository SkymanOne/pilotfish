//! The console's overlays as sheets over the main window. The console's
//! state machine owns every one of them — what is selected, what a key
//! does — so a sheet only draws the state and turns a click into the keys a
//! keyboard user would have pressed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gpui::{
    AnyElement, App, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use tokio::sync::mpsc::UnboundedSender;

use super::backend::{Snapshot, UiCmd};
use super::chat::light;
use super::theme::{MONO, Palette};
use crate::orch::protocol::is_ask_user_question;
use crate::tui::app::{KeyState, Overlay, questions_of};
use crate::tui::transcript::tool_args_text;

/// The open overlay as a sheet, or nothing.
pub fn render(
    snap: &Snapshot,
    pal: &Palette,
    cmds: &UnboundedSender<UiCmd>,
    _: &mut Window,
    _: &mut App,
) -> Option<AnyElement> {
    let overlay = snap.overlay.as_ref()?;
    let sheet = match overlay {
        Overlay::Help => help(pal),
        Overlay::Fleet => fleet(snap, pal, cmds),
        Overlay::Confirm(state) => card(pal, "Confirm", 480.)
            .child(
                div()
                    .text_color(pal.fail)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(state.message.clone()),
            )
            .child(
                buttons()
                    .child(danger_button(pal, "Confirm", cmds, keys(&['y'])))
                    .child(button(pal, "Cancel", false, cmds, keys(&['n']))),
            ),
        Overlay::Permission(state) => permission(snap, state, pal, cmds),
        Overlay::Palette(state) => {
            let mut sheet = card(pal, "Commands", 640.).child(field(pal, "", &state.query));
            let mut previous: Option<String> = None;
            // a window of rows around the selection, like the TUI's
            let shown = window(state.visible.len(), state.selected, 12);
            for (at, &index) in state
                .visible
                .iter()
                .enumerate()
                .skip(shown.start)
                .take(shown.len())
            {
                let Some(item) = state.items.get(index) else {
                    continue;
                };
                let group = item.group.label();
                if previous.as_deref() != Some(group.as_str()) {
                    sheet = sheet.child(label(pal, &group));
                    previous = Some(group);
                }
                sheet = sheet.child(option(
                    pal,
                    ("palette", at),
                    None,
                    &item.label,
                    &item.detail,
                    at == state.selected,
                    cmds,
                    moves(state.selected, at, Some(KeyCode::Enter)),
                ));
            }
            if state.visible.is_empty() {
                sheet = sheet.child(hint(pal, "No command or session matches."));
            }
            sheet.child(hint(
                pal,
                &format!(
                    "{} of {}. Enter runs it, Esc closes.",
                    (state.selected + 1).min(state.visible.len()),
                    state.visible.len()
                ),
            ))
        }
        Overlay::Search(state) => card(pal, "Search the transcript", 480.)
            .child(field(pal, "", &state.query))
            .child(hint(
                pal,
                &format!(
                    "Match {} of {}. Enter keeps the highlights, Esc closes.",
                    state
                        .current
                        .map_or_else(|| "–".to_string(), |c| (c + 1).to_string()),
                    state.matches.len()
                ),
            )),
        Overlay::Brief(state) => card(pal, "Brief", 680.)
            .child(
                div()
                    .id("brief")
                    .max_h(px(520.))
                    .overflow_y_scroll()
                    .font_family(MONO)
                    .text_size(px(12.5))
                    .text_color(if state.placeholder {
                        pal.muted
                    } else {
                        pal.text
                    })
                    .child(state.text.clone()),
            )
            .child(hint(pal, "Esc closes.")),
        Overlay::Routing(panel) => match &panel.shortlist {
            Some(editor) => shortlist(editor, pal, cmds),
            None => routing(panel, pal, cmds),
        },
        Overlay::ModelChoice(state) => model_choice(snap, state, pal, cmds)?,
    };
    Some(
        div()
            .id("sheet-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(96.))
            .bg(super::theme::tint(
                gpui::black(),
                if pal.dark { 0.45 } else { 0.18 },
            ))
            .occlude()
            .child(sheet)
            .into_any_element(),
    )
}

fn card(pal: &Palette, title: &str, width: f32) -> Div {
    div()
        .w(px(width))
        .max_h(px(640.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .p(px(18.))
        .overflow_hidden()
        .rounded(px(12.))
        .border_1()
        .border_color(pal.line)
        .bg(pal.raised)
        .shadow_lg()
        .text_size(px(13.5))
        .text_color(pal.text)
        .child(
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .mb(px(4.))
                .child(title.to_string()),
        )
}

fn hint(pal: &Palette, text: &str) -> Div {
    div()
        .mt(px(8.))
        .text_size(px(12.))
        .text_color(pal.muted)
        .child(text.to_string())
}

fn label(pal: &Palette, text: &str) -> Div {
    div()
        .mt(px(6.))
        .text_size(px(12.))
        .text_color(pal.muted)
        .child(text.to_string())
}

/// A text field the console is typing into (the keys reach it through the
/// sheet's own key handler).
fn field(pal: &Palette, prompt: &str, value: &str) -> Div {
    div()
        .flex()
        .items_center()
        .px(px(10.))
        .py(px(7.))
        .rounded(px(7.))
        .border_1()
        .border_color(pal.accent)
        .bg(pal.bg)
        .when(!prompt.is_empty(), |this| {
            this.child(
                div()
                    .text_color(pal.muted)
                    .mr(px(6.))
                    .child(prompt.to_string()),
            )
        })
        .child(value.to_string())
        .child(div().w(px(1.5)).h(px(15.)).bg(pal.accent))
}

fn buttons() -> Div {
    div().flex().gap(px(8.)).mt(px(10.))
}

fn button(
    pal: &Palette,
    text: &str,
    primary: bool,
    cmds: &UnboundedSender<UiCmd>,
    send: Vec<KeyEvent>,
) -> impl IntoElement {
    let cmds = cmds.clone();
    div()
        .id(SharedString::from(format!("button-{text}")))
        .px(px(12.))
        .py(px(4.))
        .rounded(px(6.))
        .border_1()
        .cursor_pointer()
        .text_size(px(12.5))
        .map(|this| {
            if primary {
                this.bg(pal.accent)
                    .border_color(pal.accent)
                    .text_color(pal.accent_ink)
            } else {
                this.border_color(pal.line).text_color(pal.text)
            }
        })
        .on_click(move |_, _, _| send_keys(&cmds, &send))
        .child(text.to_string())
}

/// The one button that destroys work: port red, never the accent.
fn danger_button(
    pal: &Palette,
    text: &str,
    cmds: &UnboundedSender<UiCmd>,
    send: Vec<KeyEvent>,
) -> impl IntoElement {
    let cmds = cmds.clone();
    div()
        .id(SharedString::from(format!("danger-{text}")))
        .px(px(12.))
        .py(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(12.5))
        .bg(pal.fail)
        .text_color(gpui::white())
        .on_click(move |_, _, _| send_keys(&cmds, &send))
        .child(text.to_string())
}

/// One pickable row.
#[allow(clippy::too_many_arguments)]
fn option(
    pal: &Palette,
    id: (&'static str, usize),
    marker: Option<AnyElement>,
    text: &str,
    detail: &str,
    selected: bool,
    cmds: &UnboundedSender<UiCmd>,
    send: Vec<KeyEvent>,
) -> impl IntoElement {
    let cmds = cmds.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(9.))
        .py(px(6.))
        .rounded(px(7.))
        .cursor_pointer()
        .when(selected, |this| this.bg(pal.select))
        .hover(|this| this.bg(pal.select))
        .on_click(move |_, _, _| send_keys(&cmds, &send))
        .children(marker)
        .child(
            div()
                .when(detail.is_empty(), |this| this.flex_1())
                .when(!detail.is_empty(), |this| this.flex_none().max_w(px(280.)))
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .when(selected, |this| this.font_weight(FontWeight::MEDIUM))
                .child(text.to_string()),
        )
        .when(!detail.is_empty(), |this| {
            this.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.))
                    .text_color(pal.muted)
                    .child(detail.to_string()),
            )
        })
}

/// The rows to show of `count` so `selected` stays in view.
fn window(count: usize, selected: usize, rows: usize) -> std::ops::Range<usize> {
    if count <= rows {
        return 0..count;
    }
    let start = selected.saturating_sub(rows / 2).min(count - rows);
    start..start + rows
}

fn send_keys(cmds: &UnboundedSender<UiCmd>, keys: &[KeyEvent]) {
    for key in keys {
        let _ = cmds.send(UiCmd::Key(*key));
    }
}

fn keys(chars: &[char]) -> Vec<KeyEvent> {
    chars
        .iter()
        .map(|c| KeyEvent::new(KeyCode::Char(*c), KeyModifiers::NONE))
        .collect()
}

/// The arrow presses from `from` to `to`, then `then`.
fn moves(from: usize, to: usize, then: Option<KeyCode>) -> Vec<KeyEvent> {
    let step = if to >= from {
        KeyCode::Down
    } else {
        KeyCode::Up
    };
    let mut out: Vec<KeyEvent> = (0..from.abs_diff(to))
        .map(|_| KeyEvent::new(step, KeyModifiers::NONE))
        .collect();
    out.extend(then.map(|code| KeyEvent::new(code, KeyModifiers::NONE)));
    out
}

fn help(pal: &Palette) -> Div {
    let mut commands = div().flex().flex_col().gap(px(2.));
    for spec in crate::tui::completions::COMMANDS {
        if matches!(spec.name, "/mouse") {
            continue;
        }
        commands = commands.child(
            div()
                .flex()
                .gap(px(14.))
                .text_size(px(12.5))
                .child(
                    div()
                        .w(px(120.))
                        .flex_none()
                        .font_family(MONO)
                        .text_color(pal.text)
                        .child(spec.name),
                )
                .child(
                    div()
                        .text_color(pal.muted)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(spec.detail),
                ),
        );
    }
    card(pal, "Keys and commands", 640.)
        .child(
            div()
                .id("help")
                .max_h(px(520.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(sheet_rows(pal))
                .child(label(pal, "Commands"))
                .child(commands),
        )
        .child(hint(pal, "Esc closes."))
}

fn sheet_rows(pal: &Palette) -> Div {
    let mut rows = div().flex().flex_col().gap(px(4.));
    for (keys, what) in [
        ("⌘K", "commands, sessions and workers"),
        ("⌘B", "the Board, every session's workers"),
        ("⌘1  ⌘2  ⌘3", "fold sessions, workers, changes"),
        ("⌘F", "search the transcript"),
        ("⌘O", "show older reasoning and tool output in full"),
        ("⇧⌘F", "the fleet list"),
        ("Enter  ⇧Enter", "send, new line"),
        ("Tab", "take the highlighted completion"),
        (
            "Esc",
            "close, clear the input, or stop the orchestrator's turn",
        ),
        ("/  @", "commands; workers and repository files"),
        ("⌘Q", "close the window; workers keep running"),
    ] {
        rows = rows.child(
            div()
                .flex()
                .gap(px(14.))
                .child(
                    div()
                        .w(px(120.))
                        .flex_none()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .text_color(pal.accent)
                        .child(keys),
                )
                .child(div().text_color(pal.text).child(what)),
        );
    }
    rows
}

fn fleet(snap: &Snapshot, pal: &Palette, cmds: &UnboundedSender<UiCmd>) -> Div {
    let current = snap
        .rows
        .iter()
        .position(|row| row.key == snap.selected)
        .unwrap_or(0);
    let mut sheet = card(pal, "Fleet", 720.);
    for (at, row) in snap.rows.iter().enumerate() {
        let lane = snap
            .workers
            .iter()
            .find(|w| w.row.key == row.key)
            .map(|w| pal.lane(w.lane))
            .unwrap_or(if snap.facts.turn_active {
                pal.run
            } else {
                pal.muted
            });
        sheet = sheet.child(option(
            pal,
            ("fleet", at),
            Some(light(lane, pal.dark, 8.).into_any_element()),
            &row.name,
            &row.detail,
            at == current,
            cmds,
            moves(current, at, None),
        ));
    }
    sheet.child(hint(
        pal,
        "Enter opens it. a answers, s stops, x removes, t thinking, m model. Esc closes.",
    ))
}

fn permission(
    snap: &Snapshot,
    state: &crate::tui::app::PermissionOverlay,
    pal: &Palette,
    cmds: &UnboundedSender<UiCmd>,
) -> Div {
    let Some(request) = snap.requests.get(state.at) else {
        return card(pal, "Waiting on the orchestrator", 480.);
    };
    let tool = &request.request;
    let queued = snap.requests.len().saturating_sub(state.at + 1);
    let mut title = tool
        .title
        .as_deref()
        .or(tool.display_name.as_deref())
        .map_or_else(
            || format!("The orchestrator wants to run {}", tool.tool_name),
            str::to_string,
        );
    if queued > 0 {
        title.push_str(&format!(" (+{queued} waiting)"));
    }
    let mut sheet = card(pal, &title, 620.);
    let questions = questions_of(&tool.input);
    if is_ask_user_question(tool) && !questions.is_empty() {
        let at = state.question.min(questions.len() - 1);
        let current = &questions[at];
        if questions.len() > 1 {
            sheet = sheet.child(label(
                pal,
                &format!("Question {} of {}", at + 1, questions.len()),
            ));
        }
        sheet = sheet.child(div().child(current.question.clone()));
        let options = current.options.clone().unwrap_or_default();
        for (i, text) in options.iter().enumerate() {
            sheet = sheet.child(option(
                pal,
                ("ask", i),
                None,
                text,
                "",
                state.selected == i,
                cmds,
                moves(state.selected, i, Some(KeyCode::Enter)),
            ));
        }
        sheet = sheet.child(option(
            pal,
            ("ask", options.len()),
            None,
            "Something else…",
            "",
            state.selected >= options.len(),
            cmds,
            moves(state.selected, options.len(), Some(KeyCode::Enter)),
        ));
        if state.custom {
            sheet = sheet.child(field(pal, "Answer", &state.input));
        }
        return sheet.child(hint(pal, "Arrows and Enter pick; Esc answers later."));
    }
    sheet = sheet.child(
        div()
            .px(px(11.))
            .py(px(9.))
            .rounded(px(7.))
            .border_1()
            .border_color(pal.line)
            .bg(pal.bg)
            .font_family(MONO)
            .text_size(px(12.))
            .child(format!(
                "{} {}",
                tool.tool_name,
                tool_args_text(&tool.input)
            )),
    );
    if let Some(description) = &tool.description {
        sheet = sheet.child(div().text_color(pal.muted).child(description.clone()));
    }
    if let Some(reason) = &tool.decision_reason {
        sheet = sheet.child(div().text_color(pal.fail).child(reason.clone()));
    }
    if state.denying {
        return sheet
            .child(field(pal, "Deny because", &state.input))
            .child(hint(pal, "Enter denies with this reason, Esc goes back."));
    }
    sheet.child(
        buttons()
            .child(button(pal, "Allow once", true, cmds, keys(&['y'])))
            .child(button(
                pal,
                "Allow for this session",
                false,
                cmds,
                keys(&['a']),
            ))
            .child(button(pal, "Deny…", false, cmds, keys(&['n']))),
    )
}

fn routing(
    panel: &crate::tui::app::RoutingPanel,
    pal: &Palette,
    cmds: &UnboundedSender<UiCmd>,
) -> Div {
    let mut sheet = card(pal, "Model routing", 600.).child(
        div()
            .text_color(pal.muted)
            .child("Jev picks each worker's model from the shortlist for value, asking you when it is unsure, then a thinking level that model has."),
    );
    let row = |name: &str, value: String, color| {
        div()
            .flex()
            .gap(px(12.))
            .py(px(5.))
            .border_b_1()
            .border_color(pal.line)
            .child(
                div()
                    .w(px(110.))
                    .flex_none()
                    .text_color(pal.muted)
                    .child(name.to_string()),
            )
            .child(div().text_color(color).child(value))
    };
    match &panel.status {
        None => sheet = sheet.child(hint(pal, "Checking…")),
        Some(status) => {
            sheet = sheet
                .child(row(
                    "Routing",
                    if status.enabled { "On" } else { "Off" }.into(),
                    if status.enabled {
                        pal.accent
                    } else {
                        pal.muted
                    },
                ))
                .child(row(
                    "TypeSafe key",
                    match &status.key {
                        KeyState::None => "None set".to_string(),
                        KeyState::Config { masked, path } => format!("{masked}, in {path}"),
                        KeyState::Env { masked } => format!(
                            "{masked}, from ${} (it wins over the config file)",
                            crate::secrets::KEY_VAR
                        ),
                    },
                    if status.key == KeyState::None {
                        pal.wait
                    } else {
                        pal.text
                    },
                ))
                .child(row(
                    "Choosing",
                    match &status.candidates {
                        Ok(n) => format!("between {n} model{}", if *n == 1 { "" } else { "s" }),
                        Err(why) => why.clone(),
                    },
                    if status.candidates.is_ok() {
                        pal.text
                    } else {
                        pal.wait
                    },
                ))
                .child(row(
                    "Shortlist",
                    match status.models.len() {
                        0 => "None: every model pi offers".to_string(),
                        1 => "1 model".to_string(),
                        n => format!("{n} models"),
                    },
                    pal.text,
                ))
                .child(row(
                    "Ask me below",
                    format!("{:.0}% confidence", status.threshold * 100.0),
                    pal.text,
                ));
        }
    }
    if let Some(key) = &panel.entering {
        return sheet
            .child(label(pal, "Paste or type your TypeSafe API key"))
            .child(field(pal, "", &"•".repeat(key.len().min(48))))
            .child(hint(
                pal,
                "Enter saves it to ~/.pilotfish/config.toml, readable only by you. Esc cancels.",
            ));
    }
    if panel.confirm_delete {
        return sheet.child(
            div()
                .mt(px(8.))
                .text_color(pal.fail)
                .child("Delete the stored key? y deletes it, any other key keeps it."),
        );
    }
    let stored = matches!(
        panel.status.as_ref().map(|s| &s.key),
        Some(KeyState::Config { .. })
    );
    let mut row_of_buttons = buttons()
        .child(button(pal, "Turn on or off", false, cmds, keys(&['r'])))
        .child(button(pal, "Set key", false, cmds, keys(&['s'])))
        .child(button(pal, "Shortlist", false, cmds, keys(&['m'])))
        .child(button(pal, "Ask less", false, cmds, keys(&['-'])))
        .child(button(pal, "Ask more", false, cmds, keys(&['+'])));
    if stored {
        row_of_buttons = row_of_buttons.child(button(pal, "Delete key", false, cmds, keys(&['d'])));
    }
    sheet.child(row_of_buttons).child(hint(pal, "Esc closes."))
}

fn shortlist(
    editor: &crate::tui::app::ShortlistEditor,
    pal: &Palette,
    cmds: &UnboundedSender<UiCmd>,
) -> Div {
    let mut list = div().flex().flex_col();
    let shown = window(editor.visible.len(), editor.selected, 14);
    for (at, &index) in editor
        .visible
        .iter()
        .enumerate()
        .skip(shown.start)
        .take(shown.len())
    {
        let Some(model) = editor.catalogue.get(index) else {
            continue;
        };
        let ticked = editor.chosen.contains(&model.key());
        let mut detail = Vec::new();
        if let Some(name) = &model.name
            && *name != model.id
        {
            detail.push(name.clone());
        }
        if let Some(cost) = model.cost {
            detail.push(format!("${:.2} / ${:.2}", cost.input, cost.output));
        }
        let tick = div()
            .size(px(14.))
            .flex_none()
            .rounded(px(3.))
            .border_1()
            .border_color(if ticked { pal.accent } else { pal.line })
            .when(ticked, |this| this.bg(pal.accent));
        list = list.child(option(
            pal,
            ("shortlist", at),
            Some(tick.into_any_element()),
            &model.key(),
            &detail.join(", "),
            at == editor.selected,
            cmds,
            moves(editor.selected, at, Some(KeyCode::Enter)),
        ));
    }
    card(pal, "Routing shortlist", 720.)
        .child(field(pal, "", &editor.query))
        .child(label(
            pal,
            &format!(
                "{} of {} shortlisted, at most {}",
                editor.count(),
                editor.catalogue.len(),
                crate::route::MAX_CHOICES
            ),
        ))
        .child(list)
        .when(editor.visible.is_empty(), |this| {
            this.child(hint(pal, "No model matches."))
        })
        .child(hint(
            pal,
            "Enter ticks, type to filter, Esc saves and closes.",
        ))
}

fn model_choice(
    snap: &Snapshot,
    state: &crate::tui::app::ModelChoiceState,
    pal: &Palette,
    cmds: &UnboundedSender<UiCmd>,
) -> Option<Div> {
    let question = snap.model_questions.iter().find(|q| q.id == state.id)?;
    let fallback = question
        .fallback
        .clone()
        .unwrap_or_else(|| "pi's default model".to_string());
    let left_ms = question.deadline_ms - crate::util::now_ms();
    let left = if left_ms >= 60_000 {
        format!("{} min", left_ms / 60_000)
    } else {
        format!("{} s", (left_ms / 1000).max(0))
    };
    let mut sheet = card(pal, &format!("Pick a model for {}", question.name), 680.)
        .child(div().child(format!(
            "Jev is {:.0}% sure; you are asked below {:.0}%. {fallback} is used in {left}.",
            question.confidence * 100.0,
            question.threshold * 100.0
        )))
        .child(
            div()
                .text_size(px(12.5))
                .text_color(pal.muted)
                .max_h(px(90.))
                .overflow_hidden()
                .child(question.brief.clone()),
        );
    let shown = window(question.options.len(), state.selected, 10);
    for (at, choice) in question
        .options
        .iter()
        .enumerate()
        .skip(shown.start)
        .take(shown.len())
    {
        let likely = choice
            .probability
            .map(|p| format!("{:.0}%", p * 100.0))
            .unwrap_or_default();
        sheet = sheet.child(option(
            pal,
            ("model", at),
            Some(
                div()
                    .w(px(36.))
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(pal.muted)
                    .child(likely)
                    .into_any_element(),
            ),
            &choice.key,
            &choice.detail,
            at == state.selected,
            cmds,
            moves(state.selected, at, Some(KeyCode::Enter)),
        ));
    }
    Some(
        sheet
            .child(buttons().child(button(
                pal,
                &format!("Keep {fallback}"),
                false,
                cmds,
                keys(&['d']),
            )))
            .child(hint(pal, "Enter chooses, Esc decides later.")),
    )
}
