//! Markdown -> ratatui `Text`: headings, emphasis, inline code, links, lists,
//! block quotes, fenced code and tables laid out in columns. Parsed with
//! `pulldown-cmark` (so the syntax rules are CommonMark's, not a regex's) and
//! wrapped to the pane width with `unicode-width`, so CJK and emoji do not
//! break the layout.
//!
//! Ported from the TypeScript `src/tui/markdown.ts`, reshaped for styled
//! spans: the old `parseInline` regex becomes pulldown's inline events, and
//! `wrapSpans` keeps its word-boundary wrapping because ratatui's own
//! wrapping cannot carry per-span styles across a wrap.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme::Palette;

/// How wide a table cell may be before it wraps, so one huge cell cannot push
/// the columns off the pane.
const MAX_CELL: usize = 40;

/// Below this per-cell width a table layout is not readable; the rows render
/// as plain wrapped lines instead of shredding cells into fragments.
const MIN_CELL: usize = 8;

/// Render markdown into styled lines no wider than `width`.
///
/// # Errors
///
/// Never fails today; the `Result` keeps the door open for a hard failure in
/// a future block kind without changing the API.
pub fn render(markdown: &str, width: usize, pal: &Palette) -> anyhow::Result<Vec<Line<'static>>> {
    let width = width.max(8);
    let mut r = Renderer::new(pal, width);
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        r.event(event);
    }
    r.finish();
    Ok(r.lines)
}

/// Kinds that read as one group; a change between groups earns a blank line,
/// and a heading always earns one after it, so a block is not a wall. Quotes
/// ride with prose (their bar prefix already sets them apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Prose,
    Heading,
    List,
    Code,
    Table,
    Rule,
}

struct TableState {
    /// One row = one cell = one styled span run; all owned, so the finished
    /// table outlives the parse.
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    in_head: bool,
    /// The markdown had a header row (a `| --- |` rule under the first).
    saw_header: bool,
}

impl TableState {
    const fn new() -> Self {
        Self {
            rows: Vec::new(),
            row: Vec::new(),
            in_head: false,
            saw_header: false,
        }
    }
}

struct Renderer<'a> {
    pal: &'a Palette,
    width: usize,
    lines: Vec<Line<'static>>,
    group: Option<Group>,
    para: Vec<Span<'static>>,
    para_group: Group,
    heading: bool,
    bold: usize,
    italic: usize,
    strike: usize,
    link: usize,
    quote_depth: usize,
    in_code: bool,
    code: String,
    list_stack: Vec<Option<u64>>,
    table: TableState,
    in_html: bool,
}

impl<'a> Renderer<'a> {
    const fn new(pal: &'a Palette, width: usize) -> Self {
        Self {
            pal,
            width,
            lines: Vec::new(),
            group: None,
            para: Vec::new(),
            para_group: Group::Prose,
            heading: false,
            bold: 0,
            italic: 0,
            strike: 0,
            link: 0,
            quote_depth: 0,
            in_code: false,
            code: String::new(),
            list_stack: Vec::new(),
            table: TableState::new(),
            in_html: false,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(&tag),
            Event::End(end) => self.end(end),
            Event::Text(text) => {
                if self.in_code {
                    self.code.push_str(&text);
                } else if !self.in_html {
                    self.push_inline(text.into_string());
                }
            }
            Event::Code(code) => {
                let style = self.pal.code();
                self.para.push(Span::styled(code.into_string(), style));
            }
            Event::SoftBreak => self.push_inline(" "),
            Event::HardBreak => self.flush_para(),
            Event::Rule => {
                self.flush_para();
                let rule = "─".repeat(24.min(self.width));
                self.push_grouped(Group::Rule, Line::styled(rule, self.pal.dim()));
            }
            Event::TaskListMarker(done) => {
                let style = self.pal.dim();
                let mark = if done { "[x] " } else { "[ ] " };
                self.para.push(Span::styled(mark.to_string(), style));
            }
            // html and the rarely-seen reference/math kinds pass through
            // unpainted: they are chrome, not content
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_para();
                self.para_group = Group::Prose;
            }
            Tag::Heading { .. } => {
                self.flush_para();
                self.heading = true;
                self.para_group = Group::Heading;
            }
            Tag::BlockQuote(_) => {
                self.flush_para();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush_para();
                self.in_code = true;
                self.code.clear();
            }
            Tag::List(start) => {
                self.flush_para();
                self.list_stack.push(*start);
            }
            Tag::Item => {
                self.flush_para();
                self.para_group = Group::List;
                // the marker rides in the paragraph, so wrapped lines continue
                // under it only by luck — the old console did the same
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        // the start number is the first item's number
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                self.para.push(Span::styled(indent, Style::default()));
                self.para.push(Span::styled(marker, self.inline_style()));
            }
            Tag::Table(_) => {
                self.flush_para();
                self.table = TableState::new();
            }
            Tag::TableHead => self.table.in_head = true,
            Tag::TableRow => {
                self.flush_para();
                self.table.row.clear();
            }
            Tag::TableCell => {
                // cell content flows through the same inline machinery; the
                // cell boundary just claims what accumulated
                self.flush_para();
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } | Tag::Image { .. } => self.link += 1,
            Tag::HtmlBlock => self.in_html = true,
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph | TagEnd::Item => self.flush_para(),
            TagEnd::Heading(_) => {
                self.flush_para();
                self.heading = false;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_para();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.flush_code();
                self.in_code = false;
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            TagEnd::Table => {
                let rows = std::mem::take(&mut self.table.rows);
                let header = self.table.saw_header;
                for line in render_table(&rows, header, self.width, self.pal) {
                    self.push_grouped(Group::Table, line);
                }
            }
            TagEnd::TableHead => {
                // pulldown closes the head without a TableRow end: the head
                // row lands here, and the rule goes under it in render_table
                let row = std::mem::take(&mut self.table.row);
                self.table.rows.push(row);
                self.table.saw_header = true;
                self.table.in_head = false;
            }
            TagEnd::TableRow => {
                let row = std::mem::take(&mut self.table.row);
                self.table.rows.push(row);
            }
            TagEnd::TableCell => {
                let mut cell = std::mem::take(&mut self.para);
                if self.table.in_head {
                    for span in &mut cell {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                self.table.row.push(cell);
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => self.link = self.link.saturating_sub(1),
            TagEnd::HtmlBlock => self.in_html = false,
            _ => {}
        }
    }

    /// The style an inline span gets right now, from the open containers.
    fn inline_style(&self) -> Style {
        let mut style = if self.quote_depth > 0 {
            self.pal.quote()
        } else {
            Style::default()
        };
        if self.heading {
            style = style.patch(self.pal.heading());
        }
        if self.bold > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link > 0 {
            style = style.patch(self.pal.link());
        }
        style
    }

    fn push_inline(&mut self, text: impl Into<String>) {
        let style = self.inline_style();
        self.para.push(Span::styled(text.into(), style));
    }

    /// Land the paragraph: quote prefix, wrap, one group.
    fn flush_para(&mut self) {
        if self.para.is_empty() {
            return;
        }
        let group = self.para_group;
        let mut spans: Vec<Span> = Vec::new();
        if self.quote_depth > 0 {
            spans.push(Span::styled("│ ".to_string(), self.pal.quote()));
        }
        spans.append(&mut self.para);
        for line in wrap_spans(&spans, self.width) {
            self.push_grouped(group, Line::from(line));
        }
    }

    /// Land the code block: every source line stays a line, hard-wrapped.
    fn flush_code(&mut self) {
        let code = std::mem::take(&mut self.code);
        if code.is_empty() {
            return;
        }
        let style = self.pal.code();
        for source in code.trim_end_matches('\n').split('\n') {
            for line in wrap_plain(source, self.width) {
                self.push_grouped(Group::Code, Line::from(Span::styled(line, style)));
            }
        }
    }

    /// Push one line with the blank-line rule: a group change earns a blank,
    /// so does anything after a heading, but blank separators never double.
    fn push_grouped(&mut self, group: Group, line: Line<'static>) {
        if is_blank(&line) {
            self.lines.push(line);
            return;
        }
        let needs_blank = self
            .group
            .is_some_and(|previous| previous != group || previous == Group::Heading);
        if needs_blank && self.lines.last().is_some_and(|l| !is_blank(l)) {
            self.lines.push(Line::from(Span::raw(String::new())));
        }
        self.lines.push(line);
        self.group = Some(group);
    }

    fn finish(&mut self) {
        self.flush_para();
        if self.in_code {
            self.flush_code();
        }
    }
}

fn is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Wrapping

/// Split into words that carry their trailing whitespace, so a wrap point is
/// where the space ends — the same tokenising the TypeScript relied on.
fn words_of(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            in_word = false;
        } else {
            if !in_word && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            in_word = true;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn width_of(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Break a styled span run into lines of at most `width` printed columns, on
/// word boundaries; one word longer than the line is broken rather than lost.
pub fn wrap_spans(spans: &[Span<'_>], width: usize) -> Vec<Vec<Span<'static>>> {
    // A zero-column line can hold nothing; return before the long-word
    // breaker below fails to advance and loops forever pushing empty pieces.
    if width == 0 {
        return Vec::new();
    }
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        for word in words_of(&span.content) {
            let word_width = width_of(word.trim_end());
            if used > 0 && used + word_width > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if word_width > width {
                // a single word longer than the column: break it
                let mut rest = word.as_str();
                while width_of(rest) > width {
                    let mut piece = String::new();
                    let mut piece_width = 0usize;
                    let mut piece_end = 0usize;
                    for (byte_idx, ch) in rest.char_indices() {
                        let ch_width = width_of(ch.encode_utf8(&mut [0; 4]));
                        // the first char always goes in: a width-2 char at
                        // width 1 would otherwise leave the piece empty and
                        // the breaker spinning forever
                        if !piece.is_empty() && piece_width + ch_width > width {
                            break;
                        }
                        piece.push(ch);
                        piece_width += ch_width;
                        piece_end = byte_idx + ch.len_utf8();
                    }
                    out.push(vec![Span::styled(piece, span.style)]);
                    rest = &rest[piece_end..];
                }
                if !rest.is_empty() {
                    line.push(Span::styled(rest.to_string(), span.style));
                    used = width_of(rest);
                }
                continue;
            }
            // after a wrap, a whitespace run would sit at the head of a line:
            // drop it. At the very start of the paragraph it is indent, keep it.
            if used == 0 && !out.is_empty() && word.trim().is_empty() {
                continue;
            }
            used += width_of(&word);
            line.push(Span::styled(word, span.style));
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    // trailing spaces are padding's business, not content's
    out.into_iter()
        .map(|mut l| {
            if let Some(last) = l.last_mut() {
                let trimmed = last.content.trim_end().to_string();
                last.content = trimmed.into();
            }
            l
        })
        .collect()
}

/// Hard-wrap unstyled text at exactly `width` columns (code blocks keep
/// their shape; only overlong lines are cut across).
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = width_of(ch.encode_utf8(&mut [0; 4]));
        if used + w > width {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += w;
    }
    out.push(current);
    out
}

// ---------------------------------------------------------------------------
// Tables

fn printed(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width_of(&s.content)).sum()
}

/// Fold a row that carries more cells than the table's agreed column count
/// (a stray `|` inside inline code or a regex splits cells at the block
/// level) into the last column: nothing is lost, and the phantom columns
/// never reach the layout. Table cells are always owned (built from the
/// parse into `String`s), so this works on `'static` spans.
fn normalize_row(row: &[Vec<Span<'static>>], columns: usize) -> Vec<Vec<Span<'static>>> {
    if row.len() <= columns {
        return row.to_vec();
    }
    let mut cells: Vec<Vec<Span<'static>>> = row[..columns - 1].to_vec();
    let mut last = row[columns - 1].clone();
    for extra in &row[columns..] {
        last.extend(extra.iter().cloned());
    }
    cells.push(last);
    cells
}

/// The table's real column count: the width most rows actually carry. The
/// header row declares the author's intent, so a tie goes to it; a single
/// over-split row must never inflate the whole table's layout.
fn table_columns(rows: &[Vec<Vec<Span>>], header: bool) -> usize {
    let mut widths: Vec<usize> = rows.iter().map(Vec::len).collect();
    widths.sort_unstable();
    let mut best = widths[0];
    let mut best_n = 1usize;
    let mut i = 0;
    while i < widths.len() {
        let width = widths[i];
        let mut n = 0;
        while i < widths.len() && widths[i] == width {
            n += 1;
            i += 1;
        }
        if n > best_n || (n == best_n && header && width == rows[0].len()) {
            best = width;
            best_n = n;
        }
    }
    best.max(1)
}

/// A table with more columns than the pane can show readably, rendered as
/// plain wrapped rows: cells joined by the separator, wrapped at the full
/// width. The header keeps its bold; there is no column rule.
fn render_table_plain(
    rows: &[Vec<Vec<Span<'static>>>],
    width: usize,
    pal: &Palette,
) -> Vec<Line<'static>> {
    let separator = Span::styled(" │ ".to_string(), pal.dim());
    let mut out: Vec<Line<'static>> = Vec::new();
    for row in rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (c, cell) in row.iter().enumerate() {
            spans.extend(cell.iter().cloned());
            if c + 1 < row.len() {
                spans.push(separator.clone());
            }
        }
        for line in wrap_spans(&spans, width) {
            out.push(Line::from(line));
        }
    }
    out
}

/// A markdown table as aligned rows: cells keep their inline styling, wrap
/// onto as many lines as they need (nothing is cut), columns pad to the
/// widest printed line, and the header gets a rule under it. When the pane
/// is too narrow for the columns at a readable width, the rows fall back to
/// plain wrapped lines instead.
fn render_table(
    rows: &[Vec<Vec<Span<'static>>>],
    header: bool,
    width: usize,
    pal: &Palette,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let columns = table_columns(rows, header);
    let rows: Vec<Vec<Vec<Span>>> = rows.iter().map(|row| normalize_row(row, columns)).collect();
    // keep the whole table near the pane when it can, cap a wide cell at MAX_CELL
    let col_cap = (width.saturating_sub(2) / columns).saturating_sub(3);
    if col_cap < MIN_CELL {
        return render_table_plain(&rows, width, pal);
    }
    let col_cap = col_cap.min(MAX_CELL);
    let dim = pal.dim();
    let cells: Vec<Vec<Vec<Vec<Span>>>> = rows
        .iter()
        .map(|row| row.iter().map(|cell| wrap_spans(cell, col_cap)).collect())
        .collect();
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            cells
                .iter()
                .flat_map(|row| row[c].iter())
                .map(|line| printed(line))
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    let separator = Span::styled(" │ ".to_string(), dim);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, row) in cells.iter().enumerate() {
        let is_header = header && i == 0;
        let height = row.iter().map(Vec::len).max().unwrap_or(1);
        for at in 0..height {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, cell) in row.iter().enumerate() {
                let content = cell
                    .get(at)
                    .cloned()
                    .unwrap_or_else(|| vec![Span::raw(String::new())]);
                let gap = widths[c].saturating_sub(printed(&content));
                spans.extend(content);
                if gap > 0 {
                    spans.push(Span::raw(" ".repeat(gap)));
                }
                if c + 1 < columns {
                    spans.push(separator.clone());
                }
            }
            out.push(Line::from(spans));
        }
        if is_header {
            let rule = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─");
            out.push(Line::from(Span::styled(rule, dim)));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn plain(markdown: &str, width: usize) -> Vec<Line<'static>> {
        render(markdown, width, &Palette::plain()).unwrap()
    }

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn span_named<'a>(line: &'a Line<'_>, needle: &str) -> &'a Span<'a> {
        line.spans
            .iter()
            .find(|s| s.content == needle)
            .or_else(|| line.spans.iter().find(|s| s.content.contains(needle)))
            .unwrap()
    }

    #[test]
    fn block_styles() {
        {
            let lines = plain("# The plan\n\ntext", 80);
            assert_eq!(text_of(&lines[0]), "The plan");
            assert!(
                lines[0].spans[0]
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD)
            );
            assert_eq!(text_of(&lines[2]), "text");
        }
        {
            let lines = plain("a **bold** and *an* and `code` and [link](https://x)", 200);
            let line = &lines[0];
            assert_eq!(span_named(line, "bold").style.add_modifier, Modifier::BOLD);
            assert_eq!(span_named(line, "an").style.add_modifier, Modifier::ITALIC);
            let pal = Palette::colored();
            let line = &render("a `code` [link](u)", 200, &pal).unwrap()[0];
            assert_eq!(span_named(line, "code").style.fg, Some(Color::Green));
            assert_eq!(span_named(line, "link").style.fg, Some(Color::Cyan));
            assert!(
                span_named(line, "link")
                    .style
                    .add_modifier
                    .contains(Modifier::UNDERLINED)
            );
        }
        {
            let lines = plain("- alpha\n- beta\n\n1. one\n2. two", 80);
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            assert_eq!(texts[0], "• alpha");
            assert_eq!(texts[1], "• beta");
            assert!(texts.contains(&"1. one".to_string()));
            assert!(texts.contains(&"2. two".to_string()));
        }
        {
            let lines = plain("- top\n  - inner", 80);
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            assert_eq!(texts[0], "• top");
            assert_eq!(texts[1], "  • inner");
        }
        {
            let pal = Palette::colored();
            let lines = render(
                "```rust\nfn main() {\n    oh so long a line that must be wrapped somewhere\n}\n```",
                20,
                &pal,
            )
            .unwrap();
            assert_eq!(text_of(&lines[0]), "fn main() {");
            assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
            // the long line was hard-wrapped, not lost
            let joined: String = lines.iter().map(text_of).collect();
            assert!(joined.contains("so long a line"));
            assert!(joined.contains("somewhere"));
            assert!(lines.iter().any(|l| text_of(l) == "}"));
        }
        {
            let lines = plain("> quoted thought", 80);
            assert_eq!(text_of(&lines[0]), "│ quoted thought");
            assert_eq!(lines[0].spans[0].style, Palette::plain().quote());
        }
        {
            let lines = plain("---", 80);
            assert_eq!(text_of(&lines[0]), "─".repeat(24));
        }
        {
            let lines = plain("- [x] done\n- [ ] open\n\n~~gone~~", 80);
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            assert!(texts[0].contains("[x] done"), "{texts:?}");
            assert!(texts[1].contains("[ ] open"), "{texts:?}");
            assert!(
                span_named(&lines[3], "gone")
                    .style
                    .add_modifier
                    .contains(Modifier::CROSSED_OUT)
            );
        }
    }

    #[test]
    fn tables() {
        {
            let pal = Palette::colored();
            let lines = render(
                "| name | result |\n| --- | --- |\n| db | **ok** |\n| api | 12 |",
                60,
                &pal,
            )
            .unwrap();
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            assert_eq!(texts[0], "name │ result");
            assert!(
                lines[0].spans[0]
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD)
            );
            assert!(texts[1].contains("─┼─"), "{texts:?}");
            // cells pad to the widest line in their column
            assert_eq!(texts[2], "db   │ ok    ");
            assert_eq!(texts[3], "api  │ 12    ");
            // the bold cell inside the table keeps its style
            assert!(
                span_named(&lines[2], "ok")
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD)
            );
        }
        {
            let long = "x".repeat(120);
            let lines = plain(&format!("| a |\n| --- |\n| {long} |"), 60);
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            let joined = texts.join("");
            assert_eq!(joined.matches('x').count(), 120, "nothing is lost");
            for line in &lines {
                assert!(width_of(&text_of(line)) <= 60, "{}", text_of(line));
            }
        }
        {
            // Ten columns cannot fit a 30-column pane at a readable width: the
            // rows render as plain wrapped lines, never 4-character fragments.
            let lines = plain(
                "| a | b | c | d | e | f | g | h | i | j |\n\
                 |---|---|---|---|---|---|---|---|---|---|\n\
                 | elephant | elephant | elephant | elephant | elephant | elephant | elephant | elephant | elephant | elephant |",
                30,
            );
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            let joined = texts.join("\n");
            assert!(joined.contains("elephant"), "{joined}");
            for line in &lines {
                assert!(width_of(&text_of(line)) <= 30, "{}", text_of(line));
            }
        }
        {
            // A `|` inside inline code on the header line inflates pulldown's
            // column count for the whole table; cells must still render whole
            // words, not 4-character tails.
            let lines = plain(
                "| `pat|tern` | watch |\n|---|---|---|\n| matching | pattern |",
                24,
            );
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            let joined = texts.join("\n");
            assert!(joined.contains("pattern"), "{joined}");
            assert!(joined.contains("matching"), "{joined}");
            assert!(joined.contains("watch"), "{joined}");
        }
        {
            // A body row split by a stray `|` (inside a regex, say) yields more
            // cells than the header; the overflow folds into the last column and
            // the other rows keep their shape.
            fn cell(text: &str) -> Vec<Span<'static>> {
                vec![Span::raw(text.to_string())]
            }
            let rows = vec![
                vec![cell("name"), cell("value")],
                vec![cell("alpha"), cell("beta")],
                // `grep -E 'a|b'` inside a cell splits into: grep -E 'a | b' | done
                vec![cell("grep -E 'a"), cell("b'"), cell("done")],
                vec![cell("matching"), cell("pattern")],
            ];
            let lines = render_table(&rows, true, 30, &Palette::plain());
            let joined: String = lines.iter().map(text_of).collect();
            assert!(joined.contains("alpha"), "{joined}");
            assert!(joined.contains("matching"), "{joined}");
            assert!(joined.contains("pattern"), "{joined}");
            assert!(joined.contains("done"), "{joined}");
        }
    }

    #[test]
    fn wrapping() {
        {
            let lines = plain("one two three four five six seven", 12);
            for line in &lines {
                assert!(width_of(&text_of(line)) <= 12, "{}", text_of(line));
            }
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            assert_eq!(texts[0], "one two");
            assert_eq!(texts[1], "three four");
        }
        {
            let lines = plain("你好世界 世上 again", 8);
            for line in &lines {
                assert!(width_of(&text_of(line)) <= 8, "{}", text_of(line));
            }
            let lines = plain("🦀 ferris 🦀 crab", 8);
            for line in &lines {
                assert!(width_of(&text_of(line)) <= 8, "{}", text_of(line));
            }
        }
        {
            // one word, 16 columns wide, wrapped at 10
            let lines = plain("你好世界你好世界", 10);
            assert!(lines.len() >= 2);
            for line in &lines {
                assert!(width_of(&text_of(line)) <= 10, "{}", text_of(line));
            }
            let joined: String = lines.iter().map(|l| text_of(l)).collect();
            assert_eq!(joined.chars().filter(|c| *c != '\n').count(), 8);
        }
        {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(wrap_spans(&[Span::raw("word")], 0));
            });
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(2)),
                Ok(Vec::new())
            );
        }
        {
            // width 1 against width-2 chars: the breaker must take one char
            // per line rather than spin pushing empty lines forever
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(wrap_spans(&[Span::raw("你好世界")], 1));
            });
            let Ok(lines) = rx.recv_timeout(std::time::Duration::from_secs(2)) else {
                panic!("wrap_spans at width 1 must return");
            };
            let joined: String = lines
                .iter()
                .map(|l| l.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect();
            assert_eq!(joined, "你好世界", "no char lost or repeated: {lines:?}");
            for line in &lines {
                assert_eq!(
                    line.iter()
                        .map(|s| s.content.chars().count())
                        .sum::<usize>(),
                    1,
                    "{lines:?}"
                );
            }
        }
        {
            assert!(plain("", 80).is_empty());
            assert_eq!(text_of(&plain("just words", 80)[0]), "just words");
        }
    }

    #[test]
    fn spacing() {
        {
            let lines = plain("# Head\n\npara one\n\n- a\n- b\n\npara two", 80);
            let texts: Vec<String> = lines.iter().map(text_of).collect();
            // after the heading, before the list, and after the list
            assert_eq!(texts[1], "");
            let list_at = texts.iter().position(|t| t == "• a").unwrap();
            assert_eq!(texts[list_at - 1], "");
            let after = texts.iter().position(|t| t == "para two").unwrap();
            assert_eq!(texts[after - 1], "");
        }
        {
            let lines = plain("- a\n- b\n- c", 80);
            assert_eq!(lines.len(), 3);
        }
        {
            let lines = plain("first  \nsecond", 80);
            assert_eq!(lines.len(), 2, "{lines:?}");
            assert_eq!(text_of(&lines[0]), "first");
            assert_eq!(text_of(&lines[1]), "second");
        }
    }
}
