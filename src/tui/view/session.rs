//! The session drill-down: a slim session list on the left, the selected
//! session's transcript filling the rest, the composer below it. The
//! transcript renders the blocks `transcript.rs` produces, blocks separated
//! by blank lines, in the colour language the console established — the
//! human's prompts in cyan, reasoning dimmed and abridged, the model's
//! answer as rendered markdown, tool calls in blue with their results dimmed
//! beneath, fleet events in yellow, errors red. Tool calls are shown as
//! written; tool output is a preview with a count of what was left out (the
//! transcript fold already bounded it). Scrolling follows the tail when
//! `scroll()` is `None` and pins at a block when it is `Some`; search
//! matches are highlighted, the current one distinctly.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::tui::app::Console;
use crate::tui::markdown;
use crate::tui::model::DashboardRow;
use crate::tui::theme::Palette;
use crate::tui::transcript::{Block, BlockKind};
use crate::tui::view::composer;
use crate::tui::view::{Feeds, clip_to};
use crate::util::now_ms;

/// The transcript never goes below this, even when the composer is tall.
const TRANSCRIPT_MIN_ROWS: u16 = 3;
/// The session list's automatic width: glyph + name + age, nothing more.
const RAIL_AUTO: u16 = 24;
/// Every non-text block opens with a one-column marker and a space (`> `,
/// `⚙ `, `↳ `), so its wrapped rows hang two columns in to line up under it.
const MARKER_WIDTH: usize = 2;

/// Width of the session list, from the remembered `/rail` mode. `full`
/// hides the list — the transcript is the pane; `/rail` brings it back.
#[must_use]
pub fn rail_width_for(mode: &str) -> u16 {
    match mode {
        "compact" => 14,
        "auto" => RAIL_AUTO,
        "wide" => 36,
        _ => 0,
    }
}

/// Draw the session view over `area` (the frame minus the status line).
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    console: &mut Console,
    _feeds: &Feeds<'_>,
    pal: &Palette,
) {
    let rail_width = rail_width_for(&console.prefs().rail_mode);
    let (rail, rest) = if rail_width > 0 {
        let [rail, rest] =
            Layout::horizontal([Constraint::Length(rail_width), Constraint::Min(1)]).areas(area);
        (Some(rail), rest)
    } else {
        (None, area)
    };

    // the box grows by the rows the text actually wraps to, not by its
    // newlines: a long unbroken sentence needs the room just as much
    let composer_lines = composer::layout(&console.composer().input, 0, composer::inner_width(rest))
        .rows
        .len()
        .clamp(1, composer::MAX_LINES) as u16;
    let composer_height = composer_lines + 2; // the borders
    let now = now_ms();
    let flash = u16::from(console.flash().is_some());
    let activity = u16::from(console.activity_line(now).is_some());
    let [transcript_area, chrome_area] = Layout::vertical([
        Constraint::Min(TRANSCRIPT_MIN_ROWS),
        Constraint::Length(composer_height + flash + activity),
    ])
    .areas(rest);

    // half/full-page scrolling keys are measured against this pane
    console.viewport_rows = transcript_area.height as usize;

    if let Some(rail) = rail {
        draw_rail(frame, rail, console, pal);
    }
    draw_transcript(frame, transcript_area, console, pal);
    composer::draw(frame, chrome_area, console, pal, now);
    composer::draw_popup(frame, transcript_area, chrome_area, console, pal);
}

// ---------------------------------------------------------------------------
// The session list

fn draw_rail(frame: &mut Frame, area: Rect, console: &Console, pal: &Palette) {
    let rows = console.rows();
    let selected = console.selected();
    let capacity = area.height as usize;
    let start = if rows.len() <= capacity {
        0
    } else {
        selected
            .saturating_sub(capacity.saturating_sub(1))
            .min(rows.len() - capacity)
    };
    let overflow = rows.len() - start > capacity;
    let room = if overflow { capacity - 1 } else { capacity };
    let visible = (rows.len() - start).min(room);
    for (at, i) in (start..start + visible).enumerate() {
        let line = rail_line(&rows[i], i == selected, area.width, pal);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y + at as u16, area.width, 1),
        );
    }
    if overflow {
        let hidden = rows.len() - start - visible;
        let at = area.y + capacity.saturating_sub(1) as u16;
        frame.render_widget(
            Paragraph::new(Line::styled(format!("… {hidden} more"), pal.dim())),
            Rect::new(area.x, at, area.width, 1),
        );
    }
}

/// `▸ ● db    2m` — one line per session; the age is always shown, the name
/// yields for it.
fn rail_line(row: &DashboardRow, selected: bool, width: u16, pal: &Palette) -> Line<'static> {
    let width = width as usize;
    // the orchestrator owns the conversation, so it keeps its accent in
    // every state: selection paints the background over the row's own
    // colour rather than replacing it, which also keeps a worker that wants
    // the human yellow while it is the selected row
    let base = if row.attention {
        pal.attention()
    } else if row.target.is_worker() {
        Style::default()
    } else {
        pal.accent().add_modifier(Modifier::BOLD)
    };
    let base = if selected {
        base.patch(pal.selected())
    } else {
        base
    };
    let marker = if selected { "▸ " } else { "  " };
    let age_room = usize::from(!row.age.is_empty()) * (row.age.width() + 1);
    let name_room = width
        .saturating_sub(marker.width() + row.glyph.width() + 1 + age_room)
        .max(3);
    let name = clip_to(&row.name, name_room);
    let mut spans = vec![
        Span::styled(marker.to_string(), base),
        Span::styled(format!("{} ", row.glyph), base),
        Span::styled(name.clone(), base),
    ];
    if !row.age.is_empty() {
        let used = marker.width() + row.glyph.width() + 1 + name.width();
        let gap = width.saturating_sub(used + age_room).max(1);
        spans.push(Span::styled(" ".repeat(gap), base));
        spans.push(Span::styled(row.age.clone(), base));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// The transcript

/// One visual unit: a range of blocks that renders together. A markdown run
/// of `Text` blocks is one unit; a tool call with its result and continuations
/// is one; anything else stands alone.
#[derive(Debug)]
struct Unit {
    start: usize,
    end: usize,
}

/// Does `cur` continue the unit `prev` opened? Blank gap blocks never
/// continue, and nothing continues across an empty block.
fn continues(prev: &Block, cur: &Block) -> bool {
    if prev.text.is_empty() {
        return false;
    }
    let indented = cur.text.starts_with(' ');
    match (prev.kind, cur.kind) {
        // one assistant message: its lines render as one markdown block,
        // and the result sits beneath its call, unmisted by a blank line
        (BlockKind::Text, BlockKind::Text)
        | (BlockKind::User, BlockKind::User)
        | (BlockKind::Fleet, BlockKind::Fleet)
        | (BlockKind::Tool, BlockKind::ToolResult) => true,
        (BlockKind::Tool, BlockKind::Tool)
        | (BlockKind::Thinking, BlockKind::Thinking)
        | (BlockKind::Error, BlockKind::Error) => indented,
        (BlockKind::ToolResult, BlockKind::ToolResult) => indented && !cur.text.starts_with("  ↳ "),
        _ => false,
    }
}

fn build_units(blocks: &[Block]) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    for i in 0..blocks.len() {
        let extends = i > 0
            && blocks
                .get(i - 1)
                .zip(blocks.get(i))
                .is_some_and(|(prev, cur)| continues(prev, cur));
        if extends && let Some(unit) = units.last_mut() {
            unit.end = i + 1;
            continue;
        }
        units.push(Unit {
            start: i,
            end: i + 1,
        });
    }
    units
}

/// What the search highlight needs, copied out so the transcript borrow can
/// end before rendering starts.
#[derive(Debug, Clone, Default)]
pub struct Highlight {
    pub matches: Vec<usize>,
    pub current: Option<usize>,
}

fn draw_transcript(frame: &mut Frame, area: Rect, console: &mut Console, pal: &Palette) {
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    let scroll = console.scroll();
    let search = console.search().map(|s| Highlight {
        matches: s.matches.clone(),
        current: s.current,
    });
    let lines = {
        let transcript = console.open_transcript();
        let partial = transcript.partial();
        render_rows(
            transcript.blocks(),
            partial.as_deref(),
            scroll,
            search.as_ref(),
            width,
            height,
            pal,
        )
    };
    frame.render_widget(Paragraph::new(lines), area);
}

/// The visible rows for the transcript pane: units rendered on demand, the
/// window either pinned at `scroll`'s block or sliding with the tail, a
/// counted notice for what is hidden above, and the streaming partial at the
/// very bottom.
fn render_rows(
    blocks: &[Block],
    partial: Option<&str>,
    scroll: Option<usize>,
    search: Option<&Highlight>,
    width: usize,
    height: usize,
    pal: &Palette,
) -> Vec<Line<'static>> {
    let units = build_units(blocks);
    let total_units = units.len();
    let render =
        |unit: &Unit| -> Vec<(usize, Line<'static>)> { render_unit(unit, blocks, width, pal) };

    let mut rows: Vec<(usize, Line<'static>)> = Vec::new();
    let mut more_below = false;
    if let Some(block) = scroll {
        // the unit containing the pinned block
        let start = units.partition_point(|u| u.end <= block);
        let mut consumed = 0;
        for unit in &units[start..] {
            let unit_rows = render(unit);
            push_separated(&mut rows, unit_rows, unit.start);
            consumed += 1;
            if rows.len() >= height {
                break;
            }
        }
        more_below = start + consumed < total_units;
        if !more_below {
            rows.extend(partial_rows(partial, width, pal, blocks.len()));
        }
    } else {
        // the partial is the tail's last word, so it renders first
        rows.extend(partial_rows(partial, width, pal, blocks.len()));
        for unit in units.iter().rev() {
            let mut unit_rows = render(unit);
            // prepending: the seam is between this unit's last row and
            // whatever currently sits at the top
            if !rows.is_empty()
                && rows.first().is_some_and(|(_, l)| !is_blank_line(l))
                && unit_rows.last().is_some_and(|(_, l)| !is_blank_line(l))
            {
                unit_rows.push((unit.start, Line::from(Span::raw(String::new()))));
            }
            rows.splice(0..0, unit_rows);
            if rows.len() >= height {
                break;
            }
        }
    }

    // a row for the notice, a row for the tail indicator: reserve them
    let will_hide = rows.first().is_some_and(|(block, _)| *block > 0);
    let budget = height
        .saturating_sub(usize::from(will_hide))
        .saturating_sub(usize::from(more_below))
        .max(1);
    if scroll.is_some() {
        // pinned: the window opens at the pinned block, extra falls below
        rows.truncate(budget);
    } else {
        // tail: the newest rows are the point, extra falls above
        let skip = rows.len().saturating_sub(budget);
        rows.drain(0..skip);
    }
    let hidden = rows.first().map_or(0, |(block, _)| *block);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if hidden > 0 {
        lines.push(Line::styled(
            format!(
                "… {hidden} earlier line{}",
                if hidden == 1 { "" } else { "s" }
            ),
            pal.dim(),
        ));
    }
    for (block, mut line) in rows {
        apply_highlight(&mut line, block, search, pal);
        lines.push(line);
    }
    if more_below {
        lines.push(Line::styled(
            "… more below — G follows the tail".to_string(),
            pal.dim(),
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "(no events captured yet)".to_string(),
            pal.dim(),
        ));
    }
    lines
}

/// One unit as rows: a `Text` run goes through the markdown renderer (each
/// rendered line maps back to its source block, so pinning and search stay
/// honest); every other block renders as its own coloured line.
fn render_unit(
    unit: &Unit,
    blocks: &[Block],
    width: usize,
    pal: &Palette,
) -> Vec<(usize, Line<'static>)> {
    let range = &blocks[unit.start..unit.end];
    if range[0].kind == BlockKind::Text {
        let joined = range
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let run = range.len();
        let rendered = markdown::render(&joined, width, pal).unwrap_or_default();
        let count = rendered.len().max(1);
        rendered
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let block = unit.start + (i * run / count).min(run - 1);
                (block, line)
            })
            .collect()
    } else {
        range
            .iter()
            .enumerate()
            .flat_map(|(i, block)| wrap_block(block, unit.start + i, width, pal))
            .collect()
    }
}

/// One non-text block as rows. A block is a single line of text, so anything
/// longer than the pane would be clipped at the right edge: wrap it, and hang
/// the continuation rows under the marker (`> `, `⚙ `, `↳ `) so a wrapped
/// prompt still reads as one prompt. Every row keeps the block's index, so
/// pinning and search stay per-block.
fn wrap_block(
    block: &Block,
    index: usize,
    width: usize,
    pal: &Palette,
) -> Vec<(usize, Line<'static>)> {
    if block.text.is_empty() {
        return vec![(index, Line::from(Span::raw(String::new())))];
    }
    // the style rides on the span, so the search highlight can patch it
    let style = pal.block(block.kind);
    let indent = leading_spaces(&block.text) + MARKER_WIDTH;
    // one column budget for every row, the first included: a wrap point that
    // moved with the row it lands on would be harder to read than a first row
    // a marker's width short of the edge
    let body = width.saturating_sub(indent).max(1);
    let hang = " ".repeat(indent);
    markdown::wrap_spans(&[Span::styled(block.text.clone(), style)], body)
        .into_iter()
        .enumerate()
        .map(|(row, mut spans)| {
            if row > 0 {
                spans.insert(0, Span::styled(hang.clone(), style));
            }
            (index, Line::from(spans))
        })
        .collect()
}

/// How many columns of leading whitespace a block opens with.
fn leading_spaces(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

/// One blank row between units — but never two: an existing gap block, or a
/// unit that starts or ends blank, does not earn another.
fn push_separated(
    rows: &mut Vec<(usize, Line<'static>)>,
    unit_rows: Vec<(usize, Line<'static>)>,
    unit_start: usize,
) {
    if !rows.is_empty()
        && rows.last().is_some_and(|(_, l)| !is_blank_line(l))
        && unit_rows.first().is_some_and(|(_, l)| !is_blank_line(l))
    {
        rows.push((unit_start, Line::from(Span::raw(String::new()))));
    }
    rows.extend(unit_rows);
}

fn is_blank_line(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// The streaming partial as rows, a dim caret at its end.
fn partial_rows(
    partial: Option<&str>,
    width: usize,
    pal: &Palette,
    block: usize,
) -> Vec<(usize, Line<'static>)> {
    let Some(text) = partial else {
        return Vec::new();
    };
    if text.is_empty() {
        return Vec::new();
    }
    let wrapped = markdown::wrap_spans(&[Span::raw(text.to_string())], width.max(2));
    let last = wrapped.len().saturating_sub(1);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, mut spans)| {
            if i == last {
                spans.push(Span::styled("▍".to_string(), pal.dim()));
            }
            (block, Line::from(spans))
        })
        .collect()
}

/// Paint matched rows; the match under the caret stands out from the rest.
fn apply_highlight(
    line: &mut Line<'static>,
    block: usize,
    search: Option<&Highlight>,
    pal: &Palette,
) {
    let Some(search) = search else {
        return;
    };
    if search.matches.is_empty() {
        return;
    }
    let Some(at) = search.matches.iter().position(|&m| m == block) else {
        return;
    };
    let style = if search.current == Some(at) {
        pal.current_match()
    } else {
        pal.other_match()
    };
    for span in &mut line.spans {
        span.style = span.style.patch(style);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tui::model::SessionTarget;

    fn plain(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn block(kind: BlockKind, text: &str) -> Block {
        Block {
            kind,
            text: text.to_string(),
        }
    }

    #[test]
    fn tool_call_and_its_result_share_a_unit_and_results_start_new_ones() {
        let blocks = vec![
            block(BlockKind::Tool, "⚙ bash cargo test"),
            block(BlockKind::ToolResult, "  ↳ bash: running"),
            block(BlockKind::ToolResult, "      output line"),
            block(BlockKind::ToolResult, "  ↳ bash: second"),
        ];
        let units = build_units(&blocks);
        assert_eq!(
            units.len(),
            2,
            "call+result+continuation, then the next result"
        );
        assert_eq!((units[0].start, units[0].end), (0, 3));
        assert_eq!((units[1].start, units[1].end), (3, 4));
    }

    #[test]
    fn the_orchestrator_row_keeps_its_accent_when_selected() {
        let pal = Palette::colored();
        let orch = DashboardRow {
            key: "orchestrator".into(),
            glyph: "●",
            name: "orchestrator · docs".into(),
            detail: "idle".into(),
            age: String::new(),
            target: SessionTarget::Orchestrator(uuid::Uuid::nil()),
            attention: false,
            branch: None,
            diff_stat: None,
        };
        let worker = DashboardRow {
            key: "auth-1f2e3d4".into(),
            name: "auth".into(),
            target: SessionTarget::Worker {
                run_id: "auth-1f2e3d4".into(),
            },
            ..orch.clone()
        };
        let accent = pal.accent().fg;
        for selected in [false, true] {
            let line = rail_line(&orch, selected, 30, &pal);
            assert!(
                line.spans.iter().all(|s| s.style.fg == accent),
                "the session that owns the conversation keeps its colour                  (selected: {selected}): {line:?}"
            );
            let line = rail_line(&worker, selected, 30, &pal);
            assert!(
                line.spans.iter().all(|s| s.style.fg != accent),
                "a worker never borrows it (selected: {selected}): {line:?}"
            );
        }
        // selection still paints its background over both
        let line = rail_line(&orch, true, 30, &pal);
        assert!(
            line.spans.iter().all(|s| s.style.bg == pal.selected().bg),
            "{line:?}"
        );
    }

    #[test]
    fn a_selected_worker_that_wants_the_human_stays_yellow() {
        let pal = Palette::colored();
        let row = DashboardRow {
            key: "auth-1f2e3d4".into(),
            glyph: "●",
            name: "auth".into(),
            detail: "blocked".into(),
            age: "2m".into(),
            target: SessionTarget::Worker {
                run_id: "auth-1f2e3d4".into(),
            },
            attention: true,
            branch: None,
            diff_stat: None,
        };
        let line = rail_line(&row, true, 30, &pal);
        assert!(
            line.spans.iter().all(|s| s.style.fg == pal.attention().fg),
            "selection must not hide a pending question: {line:?}"
        );
    }

    #[test]
    fn a_long_prompt_wraps_and_hangs_under_its_marker() {
        let blocks = vec![block(
            BlockKind::User,
            "> plan the development of the thing and present it to me",
        )];
        let rows = render_rows(&blocks, None, None, None, 24, 20, &Palette::plain());
        let rows = plain(&rows);
        assert!(rows.len() > 1, "the prompt wrapped: {rows:?}");
        assert!(
            rows.iter()
                .all(|r| UnicodeWidthStr::width(r.as_str()) <= 24),
            "no row overflows the pane: {rows:?}"
        );
        assert!(rows[0].starts_with("> "), "the marker leads: {rows:?}");
        assert!(
            rows[1..].iter().all(|r| r.starts_with("  ")),
            "continuations hang under it: {rows:?}"
        );
        assert_eq!(
            rows.join(" ").split_whitespace().collect::<Vec<_>>(),
            blocks[0].text.split_whitespace().collect::<Vec<_>>(),
            "nothing was lost to the wrap"
        );
    }

    #[test]
    fn an_indented_tool_result_keeps_its_indent_when_it_wraps() {
        let blocks = vec![block(
            BlockKind::ToolResult,
            "  \u{21b3} bash: a b c d e f g h",
        )];
        let rows = plain(&render_rows(
            &blocks,
            None,
            None,
            None,
            16,
            20,
            &Palette::plain(),
        ));
        assert!(rows.len() > 1, "wrapped: {rows:?}");
        assert!(
            rows[1..].iter().all(|r| r.starts_with("    ")),
            "the block's own indent plus the marker: {rows:?}"
        );
    }

    #[test]
    fn text_runs_group_and_gaps_break_them() {
        let blocks = vec![
            block(BlockKind::Text, "line one"),
            block(BlockKind::Text, "line two"),
            block(BlockKind::System, ""),
            block(BlockKind::Text, "next message"),
        ];
        let units = build_units(&blocks);
        assert_eq!(units.len(), 3, "run, gap, run: {units:?}");
    }

    #[test]
    fn user_prompt_with_newlines_stays_one_unit() {
        let blocks = vec![
            block(BlockKind::User, "> first"),
            block(BlockKind::User, "> second"),
        ];
        let units = build_units(&blocks);
        assert_eq!(units.len(), 1);
    }

    #[test]
    fn blank_lines_separate_units_but_never_double() {
        let blocks = vec![
            block(BlockKind::User, "> hi"),
            block(BlockKind::System, ""), // an existing gap
            block(BlockKind::Fleet, "⚑ settled db"),
            block(BlockKind::Tool, "⚙ bash ls"),
        ];
        let pal = Palette::plain();
        let rows = render_rows(&blocks, None, None, None, 80, 20, &pal);
        let texts = plain(&rows);
        assert_eq!(
            texts,
            vec!["> hi", "", "⚑ settled db", "", "⚙ bash ls",],
            "one blank between units, no doubling: {texts:?}"
        );
    }

    #[test]
    fn the_tail_is_followed_with_a_counted_notice() {
        let blocks: Vec<Block> = (0..30)
            .map(|i| block(BlockKind::System, &format!("note {i}")))
            .collect();
        let pal = Palette::plain();
        let rows = render_rows(&blocks, None, None, None, 80, 6, &pal);
        let texts = plain(&rows);
        assert_eq!(texts.len(), 6, "{texts:?}");
        // the notice counts every line hidden above the window
        assert_eq!(texts[0], "… 27 earlier lines", "{texts:?}");
        assert_eq!(texts.last().unwrap(), "note 29");
    }

    #[test]
    fn pinned_scroll_starts_at_the_block_and_counts_the_tail() {
        let blocks: Vec<Block> = (0..30)
            .map(|i| block(BlockKind::System, &format!("note {i}")))
            .collect();
        let pal = Palette::plain();
        let rows = render_rows(&blocks, None, Some(10), None, 80, 4, &pal);
        let texts = plain(&rows);
        assert_eq!(texts[0], "… 10 earlier lines", "{texts:?}");
        assert_eq!(texts[1], "note 10");
        assert_eq!(texts.last().unwrap(), "… more below — G follows the tail");
    }

    #[test]
    fn search_matches_are_highlighted_and_the_current_one_distinctly() {
        let blocks = vec![
            block(BlockKind::System, "the quick fox"),
            block(BlockKind::System, "another quick fox"),
        ];
        let pal = Palette::plain();
        let search = Highlight {
            matches: vec![0, 1],
            current: Some(1),
        };
        let rows = render_rows(&blocks, None, None, Some(&search), 80, 10, &pal);
        // blank separators sit between blocks: rows[0] block 0, rows[2] block 1
        // the highlight patches over the block's own style
        assert!(
            rows[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            rows[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn markdown_text_units_render_styled_lines() {
        let blocks = vec![
            block(BlockKind::Text, "# The plan"),
            block(BlockKind::Text, "with **bold**"),
        ];
        let pal = Palette::plain();
        let rows = render_rows(&blocks, None, None, None, 80, 10, &pal);
        let texts = plain(&rows);
        assert_eq!(texts[0], "The plan", "the heading marker is gone");
        assert!(rows[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        let bold = rows
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("bold"))
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn block_kinds_take_their_theme_colors() {
        let blocks = vec![
            block(BlockKind::User, "> hello"),
            block(BlockKind::Fleet, "⚑ settled"),
            block(BlockKind::Tool, "⚙ bash"),
            block(BlockKind::Error, "✖ boom"),
        ];
        let pal = Palette::colored();
        let rows = render_rows(&blocks, None, None, None, 80, 10, &pal);
        // blank separators sit between the four blocks
        assert_eq!(rows[0].spans[0].style.fg, Some(ratatui::style::Color::Cyan));
        assert_eq!(
            rows[2].spans[0].style.fg,
            Some(ratatui::style::Color::Yellow)
        );
        assert_eq!(rows[4].spans[0].style.fg, Some(ratatui::style::Color::Blue));
        assert_eq!(rows[6].spans[0].style.fg, Some(ratatui::style::Color::Red));
    }

    #[test]
    fn partial_streams_at_the_bottom_with_a_caret() {
        let blocks = vec![block(BlockKind::System, "note")];
        let pal = Palette::plain();
        let rows = render_rows(&blocks, Some("streaming text"), None, None, 80, 10, &pal);
        let texts = plain(&rows);
        assert_eq!(texts[0], "note");
        // the partial is its own block: a blank, then the stream with a caret
        assert_eq!(texts[1], "", "{texts:?}");
        assert!(texts[2].starts_with("streaming text"), "{texts:?}");
        assert!(texts[2].ends_with("▍"), "{texts:?}");
    }

    #[test]
    fn an_empty_transcript_says_so() {
        let pal = Palette::plain();
        let rows = render_rows(&[], None, None, None, 80, 10, &pal);
        assert_eq!(plain(&rows), vec!["(no events captured yet)"]);
    }
}
