//! The composer: the input box the console talks through, the activity line
//! above it (`✻ thinking… 12s`, `✎ replying…`, the tool in flight), the
//! flash note, and the suggestion popup floating above the box. The text
//! itself is owned by the state machine (`Composer.input` + a char cursor);
//! it is mirrored into a `tui-textarea` each frame so the widget handles
//! multi-line layout, growth to a cap, and scrolling under the cursor.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use tui_textarea::TextArea;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::app::Console;
use crate::tui::theme::Palette;

/// The composer grows with the message, up to this many text rows.
pub const MAX_LINES: usize = 5;

/// The text width inside the box: the area less its two border columns.
#[must_use]
pub fn inner_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

/// Draw the chrome under the transcript: flash note, activity line, composer.
pub fn draw(frame: &mut Frame, area: Rect, console: &Console, pal: &Palette, now: i64) {
    let flash = console.chrome_flash();
    let activity = console.activity_line(now);
    let mut parts: Vec<Constraint> = Vec::new();
    if flash.is_some() {
        parts.push(Constraint::Length(1));
    }
    if activity.is_some() {
        parts.push(Constraint::Length(1));
    }
    parts.push(Constraint::Min(1));
    let chunks = Layout::vertical(parts).split(area);

    let mut at = 0;
    if let Some(flash) = flash {
        let style = if flash.error { pal.error() } else { pal.dim() };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::styled(flash.text.clone(), style)),
            chunks[at],
        );
        at += 1;
    }
    if let Some(line) = activity {
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::styled(line, pal.dim())),
            chunks[at],
        );
        at += 1;
    }
    draw_box(frame, chunks[at], console, pal);
}

/// The input box itself: what it is aimed at as the title, an amber title
/// while answering a question. The composer always has focus, so the border
/// is only ever accent or answering-amber.
fn draw_box(frame: &mut Frame, area: Rect, console: &Console, pal: &Palette) {
    let composer = console.composer();
    let answering = composer.answering.is_some();
    let style = if answering {
        pal.attention()
    } else {
        pal.accent()
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style)
        .title(Span::styled(
            format!(" {}", console.composer_prompt()),
            style,
        ));

    // the box wraps rather than scrolling sideways: the whole message stays
    // readable while it is being written
    let laid = layout(&composer.input, composer.cursor, inner_width(area));
    let mut textarea = TextArea::new(laid.rows);
    textarea.set_block(block);
    textarea.set_style(Style::default());
    textarea.set_cursor_line_style(Style::default());
    // the composer always has focus, so the caret is always drawn
    textarea.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    textarea.move_cursor(tui_textarea::CursorMove::Jump(
        laid.row as u16,
        laid.col as u16,
    ));
    // a &TextArea renders as a widget directly (0.7 deprecated .widget())
    frame.render_widget(&textarea, area);
}

/// The suggestion popup: a floating list above the composer, the highlighted
/// entry marked, kind-coloured labels with their details dimmed.
pub fn draw_popup(
    frame: &mut Frame,
    transcript_area: Rect,
    composer_area: Rect,
    console: &Console,
    pal: &Palette,
) {
    let composer = console.composer();
    let Some(completion) = composer.completion.clone() else {
        return;
    };
    if composer.dismissed || completion.items.is_empty() {
        return;
    }
    let selected = composer.completion_index.min(completion.items.len() - 1);

    // the widest row decides the width; it never leaves the pane
    let wanted = completion
        .items
        .iter()
        .map(|s| s.label.width() + s.detail.width() + 8)
        .max()
        .unwrap_or(20) as u16;
    let x = composer_area.x.saturating_add(1);
    let max_width = transcript_area
        .width
        .saturating_sub(x.saturating_sub(transcript_area.x));
    let width = wanted.min(max_width).max(12);
    let height = (completion.items.len() as u16 + 3).min(transcript_area.height.max(3));
    let y = composer_area.y.saturating_sub(height);
    if y < transcript_area.y {
        return; // no room above the composer
    }
    let area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(pal.accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // one row for each suggestion plus the hint line; when they outnumber
    // the rows, the window slides so the highlighted one is always drawn
    let rows = inner.height.saturating_sub(1) as usize;
    let first = selected.saturating_sub(rows.saturating_sub(1));
    for (row, (i, item)) in completion
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .enumerate()
    {
        let is_selected = i == selected;
        let marker = if is_selected { "▸ " } else { "  " };
        let label_style = pal.suggestion(item.kind);
        let label_style = if is_selected {
            label_style.add_modifier(Modifier::REVERSED)
        } else {
            label_style
        };
        let detail_style = if is_selected {
            pal.dim().add_modifier(Modifier::REVERSED)
        } else {
            pal.dim()
        };
        let mut spans = vec![
            Span::styled(marker.to_string(), label_style),
            Span::styled(item.label.clone(), label_style),
        ];
        if !item.detail.is_empty() {
            spans.push(Span::styled(format!("  {}", item.detail), detail_style));
        }
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
        );
    }
    let hidden = completion.items.len().saturating_sub(rows);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::styled(
            if hidden > 0 {
                format!("tab accept · esc dismiss · {hidden} more, up/down")
            } else {
                "tab accept · esc dismiss".to_string()
            },
            pal.dim(),
        )),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
}

/// The char cursor as a (row, col) pair, which is what the textarea jumps to.
/// The composer's text as the rows the box actually shows, and where the
/// caret sits in them.
///
/// The wrapping is visual only: `Composer.input` and its character cursor
/// are untouched, so every editing key still works on the real string and a
/// wrapped message is sent exactly as typed. Every character belongs to
/// exactly one row — a break keeps the space it broke on — so the caret maps
/// back with no ambiguity.
pub struct Wrapped {
    pub rows: Vec<String>,
    /// The caret's row, and its character offset within that row (which is
    /// what `tui-textarea` means by a column).
    pub row: usize,
    pub col: usize,
}

#[must_use]
pub fn layout(input: &str, cursor: usize, width: usize) -> Wrapped {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    // the character offset each row starts at, so the caret can be found
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    for line in input.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        let n = chars.len();
        let mut start = 0usize;
        loop {
            let mut end = start;
            let mut used = 0usize;
            while end < n {
                let w = chars[end].width().unwrap_or(0);
                if used + w > width {
                    break;
                }
                used += w;
                end += 1;
            }
            if end >= n {
                rows.push(chars[start..n].iter().collect());
                starts.push(offset + start);
                break;
            }
            // break after the last space that fits, so words stay whole; a
            // word with no space in it is broken rather than left to run out
            // of the box, and the row always advances
            let end = chars[start..end]
                .iter()
                .rposition(|c| *c == ' ')
                .map(|i| start + i + 1)
                .filter(|e| *e > start)
                .unwrap_or_else(|| end.max(start + 1));
            rows.push(chars[start..end].iter().collect());
            starts.push(offset + start);
            start = end;
        }
        offset += n + 1; // the newline itself
    }

    let total = input.chars().count();
    // typing that exactly fills the last row puts the caret one past the
    // right edge; it belongs on a fresh row, the way any editor shows it
    if cursor >= total
        && rows
            .last()
            .is_some_and(|r| UnicodeWidthStr::width(r.as_str()) >= width)
    {
        rows.push(String::new());
        starts.push(total);
    }

    let row = starts
        .iter()
        .rposition(|start| *start <= cursor)
        .unwrap_or(0);
    let col = cursor.saturating_sub(starts.get(row).copied().unwrap_or(0));
    Wrapped { rows, row, col }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The caret as (row, col), the way `layout` hands it to the textarea.
    fn caret(input: &str, cursor: usize, width: usize) -> (usize, usize) {
        let laid = layout(input, cursor, width);
        (laid.row, laid.col)
    }

    #[test]
    fn the_cursor_lands_after_newlines() {
        assert_eq!(caret("hello", 3, 40), (0, 3));
        assert_eq!(caret("hello", 5, 40), (0, 5));
        assert_eq!(
            caret("a\nb\nc", 2, 40),
            (1, 0),
            "the char after the newline"
        );
        assert_eq!(caret("a\nb\nc", 4, 40), (2, 0), "'c' opens row 2");
        assert_eq!(caret("a\nb\nc", 5, 40), (2, 1));
        assert_eq!(caret("a\nb", 3, 40), (1, 1), "one past the end");
        assert_eq!(caret("", 0, 40), (0, 0));
    }

    #[test]
    fn a_long_sentence_wraps_inside_the_box() {
        let text = "the quick brown fox jumps over the lazy dog";
        let laid = layout(text, 0, 12);
        assert!(laid.rows.len() > 1, "it wrapped: {:?}", laid.rows);
        assert!(
            laid.rows
                .iter()
                .all(|r| UnicodeWidthStr::width(r.as_str()) <= 12),
            "no row leaves the box: {:?}",
            laid.rows
        );
        assert_eq!(
            laid.rows.concat(),
            text,
            "wrapping is visual only — every character is still there, in order"
        );
    }

    #[test]
    fn a_word_too_long_for_the_box_is_broken_rather_than_lost() {
        let laid = layout("supercalifragilistic", 0, 6);
        assert_eq!(
            laid.rows,
            vec!["superc", "alifra", "gilist", "ic"],
            "it always advances"
        );
        assert_eq!(laid.rows.concat(), "supercalifragilistic");
    }

    #[test]
    fn the_caret_follows_the_text_onto_its_wrapped_row() {
        let text = "aaa bbb ccc";
        // "aaa " then "bbb " then "ccc" at width 4
        assert_eq!(layout(text, 0, 4).rows, vec!["aaa ", "bbb ", "ccc"]);
        assert_eq!(caret(text, 0, 4), (0, 0));
        assert_eq!(caret(text, 3, 4), (0, 3), "before the break");
        assert_eq!(caret(text, 4, 4), (1, 0), "the boundary opens the next row");
        assert_eq!(caret(text, 9, 4), (2, 1));
        assert_eq!(caret(text, 11, 4), (2, 3), "the end of the text");
    }

    #[test]
    fn a_full_last_row_puts_the_caret_on_a_fresh_one() {
        // typing exactly to the edge would otherwise draw the caret one
        // column outside the border
        let laid = layout("abcd", 4, 4);
        assert_eq!(laid.rows, vec!["abcd", ""]);
        assert_eq!((laid.row, laid.col), (1, 0));
        // but only when the caret is actually at the end
        let laid = layout("abcd", 2, 4);
        assert_eq!(laid.rows, vec!["abcd"]);
        assert_eq!((laid.row, laid.col), (0, 2));
    }

    #[test]
    fn empty_lines_and_a_zero_width_box_still_have_a_row() {
        assert_eq!(layout("", 0, 20).rows, vec![""]);
        assert_eq!(layout("a\n\nb", 0, 20).rows, vec!["a", "", "b"]);
        assert_eq!(layout("hi", 0, 0).rows.concat(), "hi", "never loses text");
    }

    #[test]
    fn composer_height_grows_to_the_cap() {
        let rows =
            |input: &str, width: usize| layout(input, 0, width).rows.len().clamp(1, MAX_LINES);
        assert_eq!(rows("one", 40), 1);
        assert_eq!(rows("one\ntwo\nthree", 40), 3);
        let tall = "x\n".repeat(10);
        assert_eq!(rows(tall.trim_end(), 40), MAX_LINES, "capped");
        // and a single long sentence earns the same room a newline would
        assert_eq!(
            rows("the quick brown fox jumps over the lazy dog", 10),
            MAX_LINES
        );
    }
}
