//! The chat pane: the open transcript's one-line blocks gathered into the
//! units a reader sees — a message, a markdown reply, a run of tool calls —
//! and drawn in the maritime look. Markdown goes through gpui-component's
//! `TextView`, which also highlights fenced code.

use gpui::{
    AnyElement, App, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_component::text::TextView;

use super::theme::{MONO, Palette};
use crate::tui::transcript::{Block, BlockKind};

/// One unit of the chat: consecutive blocks of one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub kind: BlockKind,
    pub text: String,
    /// The transcript blocks it gathers, for search matches.
    pub blocks: std::ops::Range<usize>,
}

/// Gather `blocks` (and the line still streaming) into groups. A blank
/// block is the transcript's own paragraph gap and always ends a group.
#[must_use]
pub fn groups(blocks: &[Block], partial: Option<&str>) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut open = false;
    for (at, block) in blocks.iter().enumerate() {
        if block.text.trim().is_empty() && block.kind != BlockKind::Text {
            open = false;
            continue;
        }
        match out.last_mut() {
            Some(last) if open && last.kind == block.kind => {
                last.text.push('\n');
                last.text.push_str(&block.text);
                last.blocks.end = at + 1;
            }
            _ => {
                let text = match block.kind {
                    BlockKind::User => block
                        .text
                        .strip_prefix("> ")
                        .unwrap_or(&block.text)
                        .to_string(),
                    _ => block.text.clone(),
                };
                out.push(Group {
                    kind: block.kind,
                    text,
                    blocks: at..at + 1,
                });
                open = true;
            }
        }
    }
    if let Some(partial) = partial.filter(|p| !p.is_empty()) {
        match out.last_mut() {
            Some(last) if open && last.kind == BlockKind::Text => {
                last.text.push('\n');
                last.text.push_str(partial);
            }
            _ => out.push(Group {
                kind: BlockKind::Text,
                text: partial.to_string(),
                blocks: blocks.len()..blocks.len(),
            }),
        }
    }
    // a reply's trailing blank lines are the gap before the next group
    for group in &mut out {
        let trimmed = group.text.trim_end().len();
        group.text.truncate(trimmed);
    }
    out.retain(|group| !group.text.is_empty());
    out
}

/// Draw one group; a search match is tinted, and the reply still streaming
/// fades in chunk by chunk.
pub fn render(
    ix: usize,
    group: &Group,
    matched: bool,
    streaming: bool,
    pal: &Palette,
    _: &mut Window,
    _: &mut App,
) -> AnyElement {
    // prose keeps a readable measure however wide the pane gets
    let body = div()
        .w_full()
        .max_w(px(760.))
        .px(px(28.))
        .py(px(7.))
        .when(matched, |this| this.bg(pal.select).rounded(px(10.)));
    let text: SharedString = group.text.clone().into();
    match group.kind {
        BlockKind::User => body
            .flex()
            .justify_end()
            .child(
                div()
                    .max_w(relative(0.78))
                    .px(px(14.))
                    .py(px(9.))
                    .rounded_tl(px(16.))
                    .rounded_tr(px(16.))
                    .rounded_bl(px(16.))
                    .rounded_br(px(4.))
                    .bg(pal.tint)
                    .border_1()
                    .border_color(pal.hair)
                    .text_color(pal.ink)
                    .child(text),
            )
            .into_any_element(),
        BlockKind::Text => body
            .child(
                TextView::markdown(("md", ix), text)
                    .selectable(true)
                    .stream_fade(streaming)
                    .text_color(pal.ink),
            )
            .into_any_element(),
        BlockKind::Thinking => body
            .child(
                div()
                    .pl(px(12.))
                    .border_l_2()
                    .border_color(pal.hair)
                    .italic()
                    .text_size(px(13.))
                    .text_color(pal.muted)
                    .child(text),
            )
            .into_any_element(),
        BlockKind::Tool | BlockKind::ToolResult => {
            let (head, rest) = group.text.split_once(' ').unwrap_or((&group.text, ""));
            let head = head.to_string();
            let rest = rest.to_string();
            body.py(px(1.))
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .text_color(pal.muted)
                        .flex()
                        .gap(px(6.))
                        .child(div().flex_none().child(head))
                        .child(div().min_w_0().child(rest)),
                )
                .into_any_element()
        }
        BlockKind::Fleet => body
            .py(px(4.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(6.))
                    .rounded_full()
                    .border_1()
                    .border_color(pal.hair)
                    .bg(pal.sheet)
                    .text_size(px(12.5))
                    .text_color(pal.ink)
                    .child(super::ui::dot(fleet_color(&group.text, pal), 7.))
                    .child(group.text.trim_start_matches(['⚑', ' ']).to_string()),
            )
            .into_any_element(),
        BlockKind::System => body
            .py(px(2.))
            .child(div().text_size(px(12.)).text_color(pal.muted).child(text))
            .into_any_element(),
        BlockKind::Error => body
            .py(px(2.))
            .child(div().text_size(px(12.5)).text_color(pal.port).child(text))
            .into_any_element(),
    }
}

/// A fleet event's light: a question is waiting on someone, a failure is
/// port red, a finish is quiet.
fn fleet_color(text: &str, pal: &Palette) -> gpui::Hsla {
    if text.contains("question") || text.contains("dialog") || text.contains("blocked") {
        pal.buoy
    } else if text.contains("failed") || text.contains("error") || text.contains("dead") {
        pal.port
    } else if text.contains("settled") || text.contains("stopped") {
        pal.muted
    } else {
        pal.ink
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_groups() {
        let block = |kind, text: &str| Block {
            kind,
            text: text.to_string(),
        };
        let blocks = vec![
            block(BlockKind::User, "> split the work"),
            block(BlockKind::User, "and keep docs in step"),
            block(BlockKind::System, ""),
            block(BlockKind::Text, "## Plan"),
            block(BlockKind::Text, "- one"),
            block(BlockKind::Tool, "⚙ spawn_worker a"),
            block(BlockKind::Tool, "⚙ spawn_worker b"),
            block(BlockKind::System, ""),
            block(BlockKind::Text, "Done so"),
        ];
        let gathered = groups(&blocks, Some("far."));
        let kinds: Vec<_> = gathered.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds,
            [
                BlockKind::User,
                BlockKind::Text,
                BlockKind::Tool,
                BlockKind::Text
            ]
        );
        assert_eq!(gathered[0].text, "split the work\nand keep docs in step");
        assert_eq!(gathered[1].text, "## Plan\n- one");
        assert_eq!(gathered[3].text, "Done so\nfar.");
        // a stream with nothing committed yet still shows
        assert_eq!(groups(&[], Some("hel"))[0].text, "hel");
    }
}
