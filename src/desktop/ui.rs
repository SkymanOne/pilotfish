//! The small pieces every surface is built from: sheets, pills, chips, the
//! logo, the wake bar and the two motions (a sheet rising into place, a
//! light breathing). GPUI drops every animation to its end state under
//! macOS's Reduce Motion.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Div, ElementId, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement, SharedString, Stateful, Styled, div, ease_out_quint,
    prelude::FluentBuilder as _, pulsating_between, px, svg,
};

use super::theme::{DISPLAY, Palette, SERIF};

/// A floating sheet: white on the canvas, generous radius, soft shadow.
pub fn sheet(pal: &Palette) -> Div {
    div()
        .bg(pal.sheet)
        .rounded(px(22.))
        .border_1()
        .border_color(pal.hair)
        .shadow(pal.sheet_shadow())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Ink,
    Buoy,
    Danger,
}

/// A pill button.
pub fn pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: Tone,
    pal: &Palette,
) -> Stateful<Div> {
    let (bg, fg, border) = match tone {
        Tone::Plain => (pal.sheet, pal.ink, pal.hair),
        Tone::Ink => (pal.ink, pal.sheet, pal.ink),
        Tone::Buoy => (pal.buoy, gpui::white(), pal.buoy),
        Tone::Danger => (pal.port, gpui::white(), pal.port),
    };
    let hover = match tone {
        Tone::Plain => pal.tint,
        _ => super::theme::tint(bg, 0.88),
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .h(px(28.))
        .px(px(12.))
        .rounded_full()
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(move |this| this.bg(hover))
        .child(label.into())
}

/// A quiet chip: a setting you can click to change.
pub fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    caret: bool,
    pal: &Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(24.))
        .px(px(10.))
        .rounded_full()
        .border_1()
        .border_color(pal.hair)
        .bg(pal.tint)
        .text_color(pal.ink)
        .text_size(px(12.))
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(|this| this.border_color(pal.muted))
        .child(label.into())
        .when(caret, |this| {
            this.child(div().text_size(px(9.)).text_color(pal.muted).child("▾"))
        })
}

/// A title in the display serif.
pub fn display(text: impl Into<SharedString>, size: f32) -> Div {
    div()
        .font_family(DISPLAY)
        .text_size(px(size))
        .line_height(px(size * 1.08))
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .child(text.into())
}

/// A name in the text serif.
pub fn serif(text: impl Into<SharedString>, size: f32) -> Div {
    div()
        .font_family(SERIF)
        .text_size(px(size))
        .line_height(px(size * 1.2))
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .child(text.into())
}

/// The mark: a pilot fish in goggles with a wing and a propeller.
pub fn logo(size: f32, pal: &Palette) -> Div {
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(
            svg()
                .path("logo/ink.svg")
                .absolute()
                .inset_0()
                .size(px(size))
                .text_color(pal.ink),
        )
        .child(
            svg()
                .path("logo/accent.svg")
                .absolute()
                .inset_0()
                .size(px(size))
                .text_color(pal.buoy),
        )
}

/// A light that breathes: something waiting on you, or a turn in flight.
pub fn pulse(id: impl Into<ElementId>, color: Hsla, size: f32) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .bg(color)
        .with_animation(
            id,
            Animation::new(Duration::from_millis(1600))
                .repeat()
                .with_easing(pulsating_between(0.3, 1.0)),
            |this, delta| this.opacity(delta),
        )
}

/// A still light.
pub fn dot(color: Hsla, size: f32) -> Div {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

/// A worker's wake: stripes that stream while it runs, a solid bar once it
/// stops (buoy waiting, port failed, faint when finished).
pub fn wake(
    id: impl Into<ElementId>,
    lane: super::backend::Lane,
    pal: &Palette,
) -> impl IntoElement {
    use super::backend::Lane;
    let track = div()
        .relative()
        .w_full()
        .h(px(5.))
        .rounded(px(3.))
        .overflow_hidden()
        .bg(pal.tint);
    match lane {
        Lane::Started => {
            let mut stripes = div().absolute().top_0().h_full().flex().gap(px(4.));
            for _ in 0..48 {
                stripes = stripes.child(div().w(px(10.)).h_full().rounded(px(2.)).bg(pal.ink));
            }
            track
                .child(stripes.with_animation(
                    id,
                    Animation::new(Duration::from_millis(1400)).repeat(),
                    |this, delta| this.left(px(-14. + 14. * delta)),
                ))
                .into_any_element()
        }
        Lane::Waiting => track
            .child(div().h_full().w_full().bg(pal.buoy))
            .into_any_element(),
        Lane::Failed => track
            .child(div().h_full().w_full().bg(pal.port))
            .into_any_element(),
        Lane::Finished => track
            .child(
                div()
                    .h_full()
                    .w_full()
                    .bg(super::theme::tint(pal.muted, 0.35)),
            )
            .into_any_element(),
    }
}

/// Fade in and settle 8 px up: dialogs, menus, the Board.
pub fn rise<E: IntoElement + Styled + 'static>(
    el: E,
    id: impl Into<ElementId>,
) -> impl IntoElement {
    el.with_animation(
        id,
        Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
        |this, delta| this.opacity(delta).mt(px(8. * (1. - delta))),
    )
}

/// Arrive a little after the one before: the Board's cards, lane by lane.
pub fn arrive<E: IntoElement + Styled + 'static>(
    el: E,
    id: impl Into<ElementId>,
    order: usize,
) -> impl IntoElement {
    let lead = 30. * order.min(20) as f32;
    let total = 220. + lead;
    el.with_animation(
        id,
        Animation::new(Duration::from_millis(total as u64)),
        move |this, delta| {
            let t = ((delta * total - lead) / 220.).clamp(0., 1.);
            let t = ease_out_quint()(t);
            this.opacity(t).mt(px(10. * (1. - t)))
        },
    )
}

/// A popover menu's card.
pub fn menu(pal: &Palette) -> Div {
    div()
        .min_w(px(220.))
        .p(px(6.))
        .rounded(px(14.))
        .border_1()
        .border_color(pal.hair)
        .bg(pal.sheet)
        .shadow(pal.sheet_shadow())
        .flex()
        .flex_col()
        .text_size(px(13.))
}

/// One menu row.
pub fn menu_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    detail: Option<String>,
    danger: bool,
    pal: &Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .px(px(10.))
        .py(px(7.))
        .rounded(px(9.))
        .cursor_pointer()
        .text_color(if danger { pal.port } else { pal.ink })
        .hover(|this| this.bg(pal.tint))
        .child(label.into())
        .when_some(detail, |this, detail| {
            this.child(
                div()
                    .text_size(px(11.5))
                    .text_color(pal.muted)
                    .max_w(px(300.))
                    .child(detail),
            )
        })
}

pub fn menu_separator(pal: &Palette) -> Div {
    div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(pal.hair)
}

/// `+12 −3` in the diff colours.
pub fn stat(added: usize, removed: usize, pal: &Palette) -> Div {
    div()
        .flex()
        .flex_none()
        .gap(px(4.))
        .text_size(px(12.))
        .child(div().text_color(pal.sea).child(format!("+{added}")))
        .child(div().text_color(pal.port).child(format!("−{removed}")))
}

/// A row's `+12 −3` string, coloured.
pub fn stat_text(text: &str, pal: &Palette) -> Div {
    let mut parts = text.split_whitespace();
    let added = parts.next().unwrap_or("").to_string();
    let removed = parts.next().unwrap_or("").to_string();
    div()
        .flex()
        .flex_none()
        .gap(px(4.))
        .text_size(px(12.))
        .child(div().text_color(pal.sea).child(added))
        .child(div().text_color(pal.port).child(removed))
}

/// Text cut to one line.
pub fn line(text: impl Into<SharedString>) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.into())
}
