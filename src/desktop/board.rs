//! The Board: every session's workers in four lanes — started, waiting on
//! you, failed, finished — as a popup over the main window (⌘B). A card
//! opens its worker; the chips narrow the Board to one session.

use std::rc::Rc;

use gpui::{
    AnyElement, App, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px,
};

use super::backend::{Lane, Snapshot, WorkerItem};
use super::chat::{light, ring};
use super::theme::{MONO, Palette};

const LANES: [(Lane, &str); 4] = [
    (Lane::Started, "Started"),
    (Lane::Waiting, "Waiting on you"),
    (Lane::Failed, "Failed"),
    (Lane::Finished, "Finished"),
];

type Handler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

/// What the Board's clicks do, supplied by the window.
pub struct Handlers {
    pub filter: Handler<Option<uuid::Uuid>>,
    pub open: Handler<WorkerItem>,
    pub close: Handler<()>,
}

pub fn render(
    snap: &Snapshot,
    pal: &Palette,
    filter: Option<uuid::Uuid>,
    handlers: &Handlers,
) -> AnyElement {
    let chip = |id: gpui::ElementId, text: String, on: bool, value: Option<uuid::Uuid>| {
        let set = handlers.filter.clone();
        div()
            .id(id)
            .px(px(10.))
            .py(px(2.))
            .rounded_full()
            .border_1()
            .cursor_pointer()
            .text_size(px(12.5))
            .map(|this| {
                if on {
                    this.bg(pal.accent)
                        .border_color(pal.accent)
                        .text_color(pal.accent_ink)
                } else {
                    this.border_color(pal.line).text_color(pal.text)
                }
            })
            .on_click(move |_, window, cx| set(value, window, cx))
            .child(text)
    };
    let mut chips = div().flex().gap(px(6.)).flex_wrap().child(chip(
        "all".into(),
        "All sessions".into(),
        filter.is_none(),
        None,
    ));
    for (at, session) in snap.sessions.iter().enumerate() {
        if session.lanes.is_empty() {
            continue;
        }
        let uuid = session.key.uuid;
        chips = chips.child(chip(
            ("chip", at).into(),
            session.name.clone(),
            filter == Some(uuid),
            Some(uuid),
        ));
    }
    let items: Vec<&WorkerItem> = snap
        .board
        .iter()
        .filter(|item| filter.is_none_or(|uuid| item.session.uuid == uuid))
        .collect();
    let mut lanes = div()
        .id("lanes")
        .flex()
        .flex_1()
        .min_h_0()
        .gap(px(12.))
        .overflow_x_scroll();
    for (lane, name) in LANES {
        let cards: Vec<&&WorkerItem> = items.iter().filter(|item| item.lane == lane).collect();
        let color = pal.lane(lane);
        let mut column = div()
            .id(name)
            .flex_1()
            .min_w(px(220.))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(10.))
            .rounded(px(10.))
            .border_t_2()
            .border_color(color)
            .bg(pal.panel)
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mb(px(2.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if lane == Lane::Finished {
                        ring(color, 9., false).into_any_element()
                    } else {
                        light(color, pal.dark, 9.).into_any_element()
                    })
                    .child(name)
                    .child(
                        div()
                            .text_color(pal.muted)
                            .font_weight(FontWeight::NORMAL)
                            .child(cards.len().to_string()),
                    ),
            );
        for (at, item) in cards.iter().enumerate() {
            column = column.child(card(lane as usize * 10_000 + at, item, pal, handlers));
        }
        if cards.is_empty() {
            column = column.child(div().text_size(px(12.5)).text_color(pal.muted).child(
                match lane {
                    Lane::Started => "Nothing running.",
                    Lane::Waiting => "Nobody is waiting on you.",
                    Lane::Failed => "No failures.",
                    Lane::Finished => "Nothing finished yet.",
                },
            ));
        }
        lanes = lanes.child(column);
    }
    let close = handlers.close.clone();
    let close_button = handlers.close.clone();
    div()
        .id("board-backdrop")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p(px(28.))
        .bg(super::theme::tint(
            gpui::black(),
            if pal.dark { 0.45 } else { 0.18 },
        ))
        .occlude()
        .on_click(move |_, window, cx| close((), window, cx))
        .child(
            div()
                .id("board")
                .size_full()
                .max_w(px(1400.))
                .flex()
                .flex_col()
                .gap(px(14.))
                .p(px(16.))
                .rounded(px(12.))
                .border_1()
                .border_color(pal.line)
                .bg(pal.bg)
                .shadow_lg()
                // clicks inside the Board stay inside it
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .items_baseline()
                                .child(
                                    div()
                                        .text_size(px(16.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Board"),
                                )
                                .child(
                                    div().text_color(pal.muted).child(match filter {
                                        None => "all sessions".to_string(),
                                        Some(uuid) => snap
                                            .sessions
                                            .iter()
                                            .find(|s| s.key.uuid == uuid)
                                            .map_or_else(String::new, |s| s.name.clone()),
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .id("board-close")
                                .flex()
                                .gap(px(6.))
                                .items_center()
                                .px(px(9.))
                                .py(px(2.))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(pal.line)
                                .cursor_pointer()
                                .text_size(px(12.))
                                .child("Close")
                                .child(div().text_color(pal.muted).child("⌘B"))
                                .on_click(move |_, window, cx| close_button((), window, cx)),
                        ),
                )
                .child(chips)
                .child(lanes),
        )
        .into_any_element()
}

fn card(id: usize, item: &WorkerItem, pal: &Palette, handlers: &Handlers) -> impl IntoElement {
    let open = handlers.open.clone();
    let target = item.clone();
    let mut card = div()
        .id(("card", id))
        .flex()
        .flex_col()
        .gap(px(3.))
        .p(px(11.))
        .rounded(px(8.))
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(pal.raised)
        .cursor_pointer()
        .hover(|this| this.border_color(pal.line))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            open(target.clone(), window, cx);
        })
        .child(
            div()
                .flex()
                .justify_between()
                .gap(px(8.))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_weight(FontWeight::MEDIUM)
                        .child(item.row.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(pal.muted)
                        .child(item.session_name.clone()),
                ),
        );
    card = match item.lane {
        Lane::Waiting => card.when_some(item.question.clone(), |this, question| {
            this.child(
                div()
                    .mt(px(4.))
                    .pl(px(8.))
                    .border_l_2()
                    .border_color(pal.wait)
                    .text_size(px(12.5))
                    .child(question),
            )
        }),
        Lane::Failed => card.child(
            div()
                .mt(px(4.))
                .font_family(MONO)
                .text_size(px(11.5))
                .text_color(pal.fail)
                .child(
                    item.error
                        .clone()
                        .unwrap_or_else(|| item.row.detail.clone()),
                ),
        ),
        _ => card.child(
            div()
                .text_size(px(12.5))
                .text_color(pal.muted)
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(item.row.detail.clone()),
        ),
    };
    let mut meta = vec![item.row.age.clone()];
    if let Some(model) = &item.model {
        meta.push(model.clone());
    }
    card.child(
        div()
            .text_size(px(12.))
            .text_color(pal.muted)
            .child(meta.join(", ")),
    )
}
