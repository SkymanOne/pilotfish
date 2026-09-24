//! The Board: every session's workers in four lanes — started, waiting on
//! you, failed, finished — as a popup over the main window (⌘B). A card
//! opens its worker; the chips narrow the Board to one session.

use std::rc::Rc;

use gpui::{
    AnyElement, App, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px,
};

use super::backend::{Lane, Snapshot, WorkerItem};
use super::theme::{MONO, Palette, tint};
use super::ui::{self, Tone};

const LANES: [(Lane, &str); 4] = [
    (Lane::Started, "Started"),
    (Lane::Waiting, "Waiting on you"),
    (Lane::Failed, "Failed"),
    (Lane::Finished, "Finished"),
];

type Handler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

/// A card's own buttons.
#[derive(Debug, Clone, Copy)]
pub enum CardAction {
    Answer,
    Merge,
}

/// What the Board's clicks do, supplied by the window.
pub struct Handlers {
    pub filter: Handler<Option<uuid::Uuid>>,
    pub open: Handler<WorkerItem>,
    pub close: Handler<()>,
    pub card: Handler<(WorkerItem, CardAction)>,
}

pub fn render(
    snap: &Snapshot,
    pal: &Palette,
    filter: Option<uuid::Uuid>,
    handlers: &Handlers,
) -> AnyElement {
    let chip = |id: gpui::ElementId, text: String, on: bool, value: Option<uuid::Uuid>| {
        let set = handlers.filter.clone();
        ui::pill(id, text, if on { Tone::Ink } else { Tone::Plain }, pal)
            .h(px(26.))
            .on_click(move |_, window, cx| set(value, window, cx))
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
    let mut order = 0;
    for (lane, name) in LANES {
        let cards: Vec<&&WorkerItem> = items.iter().filter(|item| item.lane == lane).collect();
        let calling = lane == Lane::Waiting && !cards.is_empty();
        let mut column = div()
            .id(name)
            .flex_1()
            .min_w(px(240.))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(10.))
            .rounded(px(18.))
            .bg(if calling {
                tint(pal.buoy, 0.08)
            } else {
                pal.tint
            })
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(6.))
                    .py(px(4.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if calling {
                        ui::pulse("lane-wait", pal.buoy, 8.).into_any_element()
                    } else {
                        ui::dot(pal.lane(lane), 8.).into_any_element()
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
            let id = lane as usize * 10_000 + at;
            column = column.child(ui::arrive(
                card(id, item, pal, handlers),
                ("arrive", id),
                order,
            ));
            order += 1;
        }
        if cards.is_empty() {
            column = column.child(
                div()
                    .px(px(6.))
                    .text_size(px(12.5))
                    .text_color(pal.muted)
                    .child(match lane {
                        Lane::Started => "Nothing running.",
                        Lane::Waiting => "Nobody is waiting on you.",
                        Lane::Failed => "No failures.",
                        Lane::Finished => "Nothing finished yet.",
                    }),
            );
        }
        lanes = lanes.child(column);
    }
    let close = handlers.close.clone();
    let close_button = handlers.close.clone();
    let scope = match filter {
        None => "Every session's workers".to_string(),
        Some(uuid) => snap
            .sessions
            .iter()
            .find(|s| s.key.uuid == uuid)
            .map_or_else(String::new, |s| format!("The workers of {}", s.name)),
    };
    let board = ui::sheet(pal)
        .id("board")
        .size_full()
        .max_w(px(1400.))
        .flex()
        .flex_col()
        .gap(px(16.))
        .p(px(22.))
        // clicks inside the Board stay inside it
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .child(
                    div()
                        .child(ui::display("Board", 36.))
                        .child(div().mt(px(2.)).text_color(pal.muted).child(scope)),
                )
                .child(
                    ui::pill("board-close", "Close", Tone::Plain, pal)
                        .child(
                            div()
                                .text_size(px(11.5))
                                .font_weight(FontWeight::NORMAL)
                                .text_color(pal.muted)
                                .child("⌘B"),
                        )
                        .on_click(move |_, window, cx| close_button((), window, cx)),
                ),
        )
        .child(chips)
        .child(lanes);
    div()
        .id("board-backdrop")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p(px(28.))
        .bg(tint(gpui::black(), if pal.dark { 0.5 } else { 0.16 }))
        .occlude()
        .on_click(move |_, window, cx| close((), window, cx))
        .child(ui::rise(board, "board-rise"))
        .into_any_element()
}

fn card(
    id: usize,
    item: &WorkerItem,
    pal: &Palette,
    handlers: &Handlers,
) -> gpui::Stateful<gpui::Div> {
    let open = handlers.open.clone();
    let target = item.clone();
    let hair = pal.hair;
    let mut card = div()
        .id(("card", id))
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(12.))
        .rounded(px(14.))
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(pal.sheet)
        .shadow(pal.sheet_shadow())
        .cursor_pointer()
        .hover(move |this| this.border_color(hair))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            open(target.clone(), window, cx);
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .child(ui::serif(item.row.name.clone(), 16.).min_w_0().text_color(
                    if item.lane == Lane::Finished {
                        pal.muted
                    } else {
                        pal.ink
                    },
                ))
                .children(item.row.diff_stat.as_deref().map(|s| ui::stat_text(s, pal))),
        )
        .child(
            div()
                .text_size(px(11.5))
                .text_color(pal.muted)
                .child(item.session_name.clone()),
        );
    let button = |label: &'static str, tone: Tone, action: CardAction| {
        let act = handlers.card.clone();
        let target = item.clone();
        ui::pill(("card-act", id), label, tone, pal)
            .h(px(26.))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                act((target.clone(), action), window, cx);
            })
    };
    card = match item.lane {
        Lane::Waiting => {
            card.when_some(item.question.clone(), |this, question| {
                this.child(
                    div()
                        .mt(px(4.))
                        .text_size(px(12.5))
                        .line_height(px(17.))
                        .child(question),
                )
            })
            .child(div().flex().mt(px(6.)).child(button(
                "Answer",
                Tone::Buoy,
                CardAction::Answer,
            )))
        }
        Lane::Failed => card.child(
            div()
                .mt(px(4.))
                .font_family(MONO)
                .text_size(px(11.5))
                .text_color(pal.port)
                .child(
                    item.error
                        .clone()
                        .unwrap_or_else(|| item.row.detail.clone()),
                ),
        ),
        lane => card
            .child(
                ui::line(item.row.detail.clone())
                    .text_size(px(12.5))
                    .text_color(pal.muted),
            )
            .child(
                div()
                    .mt(px(6.))
                    .child(ui::wake(("card-wake", id), lane, pal)),
            )
            .when(item.mergeable, |this| {
                this.child(div().flex().mt(px(6.)).child(button(
                    "Merge",
                    Tone::Plain,
                    CardAction::Merge,
                )))
            }),
    };
    let mut meta = Vec::new();
    if !item.row.age.is_empty() {
        meta.push(item.row.age.clone());
    }
    if let Some(model) = &item.model {
        meta.push(model.clone());
    }
    card.when(!meta.is_empty(), |this| {
        this.child(
            div()
                .mt(px(2.))
                .text_size(px(11.5))
                .text_color(pal.muted)
                .child(meta.join(", ")),
        )
    })
}
