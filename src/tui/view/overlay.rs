//! The overlays: centred, bordered popups that dim what is behind them. The
//! help (from the same key tables the bindings come from), the blocking
//! confirm, the orchestrator's permission prompt and `AskUserQuestion`
//! picker, the fuzzy command palette, and the transcript search. The state
//! machine owns the keys; this module only draws what it decided.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::orch::protocol::is_ask_user_question;
use crate::tui::app::{
    BriefState, ConfirmState, Overlay, PaletteState, PermissionOverlay, SearchState, questions_of,
};
use crate::tui::keys::help_sections;
use crate::tui::theme::{OverlayRole, Palette};
use crate::tui::transcript::tool_args_text;
use crate::tui::view::Feeds;

/// Draw whichever overlay is up, centred over the full frame.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    console: &crate::tui::app::Console,
    feeds: &Feeds<'_>,
    overlay: &Overlay,
    pal: &Palette,
) {
    match overlay {
        Overlay::Help => help(frame, area, pal),
        Overlay::Fleet => fleet(frame, area, console, pal),
        Overlay::Confirm(state) => confirm(frame, area, state, pal),
        Overlay::Permission(state) => permission(frame, area, state, feeds, pal),
        Overlay::Palette(state) => palette(frame, area, state, pal),
        Overlay::Search(state) => search(frame, area, state, pal),
        Overlay::Brief(state) => brief(frame, area, state, pal),
        Overlay::Routing(panel) => routing(frame, area, panel, pal),
    }
}

/// The fleet: every session and what can be done to the one selected, as
/// wide as the screen and as tall as the fleet.
fn fleet(frame: &mut Frame, area: Rect, console: &crate::tui::app::Console, pal: &Palette) {
    // as tall as the fleet, not as tall as the screen: header, two rows per
    // session, a blank and the footer, plus the borders
    let wanted = u16::try_from(console.rows().len() * 2 + 5).unwrap_or(u16::MAX);
    let height = wanted.min(area.height.saturating_sub(2)).max(6);
    // edge to edge: a panel this wide with a sliver of transcript showing
    // down each side reads as a drawing glitch, not as a popup
    let y = area.y + area.height.saturating_sub(height) / 2;
    let inner = panel(
        frame,
        Rect::new(area.x, y, area.width, height),
        "fleet",
        OverlayRole::Fleet,
        pal,
    );
    crate::tui::view::dashboard::draw(frame, inner, console, pal);
}

/// Shrink `area` to `width`×`height` and centre it inside.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(12);
    let height = height.min(area.height.saturating_sub(2)).max(3);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// Clear, frame with a titled rounded border, return the inner area.
fn panel(frame: &mut Frame, area: Rect, title: &str, role: OverlayRole, pal: &Palette) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(pal.border(role))
        .title(Span::styled(format!(" {title} "), pal.border(role)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut lines = Vec::new();
    for source in text.split('\n') {
        let mut current = String::new();
        let mut used = 0usize;
        for word in source.split(' ') {
            // a word wider than the column has no wrap point of its own: a
            // path or a url would otherwise run straight out of the panel
            let mut word = word;
            while word.width() > width {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    used = 0;
                }
                let (head, rest) = split_at_width(word, width);
                lines.push(head.to_string());
                word = rest;
            }
            let w = word.width();
            if !current.is_empty() && used + 1 + w > width {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                used += 1;
            }
            current.push_str(word);
            used += w;
        }
        lines.push(current);
    }
    lines
}

/// Split at the last character boundary that fits in `width` columns.
fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0usize;
    for (at, ch) in text.char_indices() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            return text.split_at(at);
        }
        used += w;
    }
    (text, "")
}

/// How many wrapped rows a field being typed into may occupy before it
/// starts scrolling. Enough to read a sentence back, few enough that a
/// paragraph typed into a one-line field cannot push the panel off screen.
const INPUT_ROWS: usize = 6;

/// A field being typed into, as rows: the prompt leads the first, the
/// continuations hang under it, and the caret rides the last. A long value
/// keeps its tail — what is being typed now has to stay on screen — with a
/// dim `…` where the earlier rows were dropped.
fn input_rows(prompt: &str, value: &str, width: usize, pal: &Palette) -> Vec<Line<'static>> {
    let indent = prompt.width();
    let mut rows = wrap(value, width.saturating_sub(indent));
    let dropped = rows.len().saturating_sub(INPUT_ROWS);
    rows.drain(0..dropped);
    let last = rows.len().saturating_sub(1);
    rows.into_iter()
        .enumerate()
        .map(|(i, text)| {
            let head = match i {
                0 if dropped == 0 => Span::styled(prompt.to_string(), pal.accent()),
                0 => Span::styled(
                    format!("{}… ", " ".repeat(indent.saturating_sub(2))),
                    pal.dim(),
                ),
                _ => Span::raw(" ".repeat(indent)),
            };
            let mut spans = vec![head, Span::raw(text)];
            if i == last {
                spans.push(Span::styled("▍".to_string(), pal.accent()));
            }
            Line::from(spans)
        })
        .collect()
}

fn draw_lines(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    for (i, line) in lines.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(line.clone()),
            Rect::new(area.x, area.y + i as u16, area.width, 1),
        );
    }
}

// -- help -------------------------------------------------------------------

fn help(frame: &mut Frame, area: Rect, pal: &Palette) {
    let sections = help_sections();
    let key_width = sections
        .iter()
        .flat_map(|section| section.rows.iter())
        .map(|row| row.keys.width())
        .max()
        .unwrap_or(0)
        + 2;
    // two columns when a key column and a readable description fit twice;
    // the fleet's letters sit beside the typing and chord keys then, and the
    // whole panel fits a terminal that one long column would overflow
    let max_width = area.width.saturating_sub(4);
    let two_columns = max_width >= 2 * (key_width as u16 + 28) + 3;
    let width = if two_columns {
        max_width.min(128)
    } else {
        max_width.min(78)
    };
    let inner_width = width.saturating_sub(4) as usize;
    let (left, right) = if two_columns {
        let split = sections.len().saturating_sub(1);
        (&sections[..split], &sections[split..])
    } else {
        (&sections[..], &sections[..0])
    };
    let column_width = if two_columns {
        (inner_width - 3) / 2
    } else {
        inner_width
    };
    let left_lines = help_column(left, key_width, column_width, pal);
    let right_lines = help_column(right, key_width, column_width, pal);
    let rows = left_lines.len().max(right_lines.len());
    let height = (rows as u16 + 2).min(area.height.saturating_sub(2));
    let inner = panel(
        frame,
        centered(area, width, height),
        "keys · esc closes",
        OverlayRole::Help,
        pal,
    );
    let (left_area, right_area) = if two_columns {
        let [l, _, r] = ratatui::layout::Layout::horizontal([
            ratatui::layout::Constraint::Length(column_width as u16),
            ratatui::layout::Constraint::Length(3),
            ratatui::layout::Constraint::Min(1),
        ])
        .areas(inner);
        (l, Some(r))
    } else {
        (inner, None)
    };
    draw_help_lines(frame, left_area, left_lines, pal);
    if let Some(right_area) = right_area {
        draw_help_lines(frame, right_area, right_lines, pal);
    }
}

/// Draw a help column, counting what does not fit rather than cutting it
/// off unannounced.
fn draw_help_lines(frame: &mut Frame, area: Rect, mut lines: Vec<Line<'static>>, pal: &Palette) {
    let room = area.height as usize;
    if lines.len() > room && room > 0 {
        let hidden = lines.len() - (room - 1);
        lines.truncate(room - 1);
        lines.push(Line::styled(
            format!("… {hidden} more lines — a taller window shows them all"),
            pal.dim(),
        ));
    }
    draw_lines(frame, area, lines);
}

/// One column of help: a heading per section, then each key beside what it
/// does, the description wrapped under itself rather than clipped at the
/// panel's edge.
fn help_column(
    sections: &[crate::tui::keys::HelpSection],
    key_width: usize,
    width: usize,
    pal: &Palette,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in sections {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled(section.title.to_string(), pal.heading()));
        for row in section.rows {
            let what = wrap(row.what, width.saturating_sub(key_width).max(10));
            for (i, part) in what.into_iter().enumerate() {
                let keys = if i == 0 { row.keys } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(format!("{keys:<key_width$}"), pal.accent()),
                    Span::styled(part, pal.dim()),
                ]));
            }
        }
    }
    lines
}

// -- confirm ----------------------------------------------------------------

fn confirm(frame: &mut Frame, area: Rect, state: &ConfirmState, pal: &Palette) {
    let width = 60u16.min(area.width.saturating_sub(4));
    let inner_width = width.saturating_sub(4) as usize;
    let wrapped = wrap(&state.message, inner_width);
    let height = wrapped.len() as u16 + 4; // message + blank + hint + borders
    let inner = panel(
        frame,
        centered(area, width, height),
        "confirm",
        OverlayRole::Confirm,
        pal,
    );
    let mut lines: Vec<Line<'static>> = wrapped
        .into_iter()
        .map(|l| Line::styled(l, pal.error().add_modifier(Modifier::BOLD)))
        .collect();
    lines.push(Line::default());
    lines.push(Line::styled(
        "y confirm · n or esc cancel".to_string(),
        pal.dim(),
    ));
    draw_lines(frame, inner, lines);
}

// -- routing ----------------------------------------------------------------

/// The `/routing` panel: routing's switch, where its key lives, what it has
/// to choose from. A key being entered is drawn as one dot per character;
/// nothing on this panel ever draws a key itself.
fn routing(frame: &mut Frame, area: Rect, state: &crate::tui::app::RoutingPanel, pal: &Palette) {
    use crate::tui::app::KeyState;
    let width = 72u16.min(area.width.saturating_sub(4));
    let inner_width = width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = wrap(
        "Jev picks each worker's model, thinking level and worktree from its brief, \
when a spawn does not name a model itself.",
        inner_width,
    )
    .into_iter()
    .map(|l| Line::styled(l, pal.dim()))
    .collect();
    lines.push(Line::default());
    // a value wraps under itself, the label column left clear
    let row = |label: &str, value: String, style: ratatui::style::Style| -> Vec<Line<'static>> {
        wrap(&value, inner_width.saturating_sub(10))
            .into_iter()
            .enumerate()
            .map(|(i, part)| {
                let label = if i == 0 { label } else { "" };
                Line::from(vec![
                    Span::styled(format!("{label:<10}"), pal.dim()),
                    Span::styled(part, style),
                ])
            })
            .collect()
    };
    match &state.status {
        None => lines.push(Line::styled("checking…".to_string(), pal.dim())),
        Some(status) => {
            lines.extend(if status.enabled {
                row("routing", "on".into(), pal.accent())
            } else {
                row("routing", "off".into(), pal.dim())
            });
            let store = crate::secrets::store_name();
            lines.extend(match &status.key {
                KeyState::None => row("api key", "none set".into(), pal.attention()),
                KeyState::Store { masked } => {
                    row("api key", format!("{masked}, in {store}"), pal.accent())
                }
                KeyState::Env { var, masked } => row(
                    "api key",
                    format!("{masked}, from ${var} (it wins over {store})"),
                    pal.accent(),
                ),
                KeyState::Unavailable(why) => row("api key", why.clone(), pal.error()),
            });
            lines.extend(match &status.candidates {
                Ok(n) => row(
                    "choosing",
                    format!("between {n} model{}", if *n == 1 { "" } else { "s" }),
                    pal.dim(),
                ),
                Err(why) => row("choosing", why.clone(), pal.attention()),
            });
            if status.enabled && status.key == KeyState::None {
                lines.push(Line::default());
                lines.push(Line::styled(
                    "Routing is on but has no key, so spawns are not routed yet.".to_string(),
                    pal.attention(),
                ));
            }
        }
    }
    lines.push(Line::default());
    if let Some(key) = &state.entering {
        lines.push(Line::styled(
            "Paste or type your TypeSafe API key:".to_string(),
            pal.heading(),
        ));
        let dots = "•".repeat(key.len().min(inner_width.saturating_sub(4)));
        lines.push(Line::from(vec![
            Span::styled("▶ ".to_string(), pal.accent()),
            Span::raw(dots),
            Span::styled("▍".to_string(), pal.dim()),
        ]));
        lines.push(Line::default());
        lines.push(Line::styled(
            format!(
                "enter save to {} · esc cancel",
                crate::secrets::store_name()
            ),
            pal.dim(),
        ));
    } else if state.confirm_delete {
        lines.push(Line::styled(
            "Delete the stored key? y deletes · any other key keeps it".to_string(),
            pal.error().add_modifier(Modifier::BOLD),
        ));
    } else {
        let stored = matches!(
            state.status.as_ref().map(|s| &s.key),
            Some(KeyState::Store { .. })
        );
        let mut hint = String::from("r routing on/off · s set key");
        if stored {
            hint.push_str(" · d delete key");
        }
        hint.push_str(" · esc close");
        lines.push(Line::styled(hint, pal.dim()));
    }
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let inner = panel(
        frame,
        centered(area, width, height),
        "model routing",
        OverlayRole::Palette,
        pal,
    );
    draw_lines(frame, inner, lines);
}

// -- permission / question --------------------------------------------------

/// One permission prompt or `AskUserQuestion`, drawn from the state
/// machine's cursor into the polled orchestrator state.
fn permission(
    frame: &mut Frame,
    area: Rect,
    state: &PermissionOverlay,
    feeds: &Feeds<'_>,
    pal: &Palette,
) {
    let Some(request) = feeds.orch.pending_requests.get(state.at) else {
        return;
    };
    let width = 70u16.min(area.width.saturating_sub(4));
    let inner_width = width.saturating_sub(4) as usize;

    let queued = feeds
        .orch
        .pending_requests
        .len()
        .saturating_sub(state.at + 1);
    let title = request
        .request
        .title
        .as_deref()
        .or(request.request.display_name.as_deref())
        .unwrap_or(&request.request.tool_name);
    let title = if queued > 0 {
        format!("{title} (+{queued} waiting)")
    } else {
        title.to_string()
    };

    let is_question = is_ask_user_question(&request.request);
    let questions = questions_of(&request.request.input);
    let has_picker = is_question && !questions.is_empty();
    let input_line = state.denying || state.custom;

    let mut body: Vec<Line<'static>> = Vec::new();
    if has_picker {
        let current = &questions[state.question.min(questions.len() - 1)];
        body.push(Line::styled(
            format!(
                "question {}/{}",
                state.question.min(questions.len() - 1) + 1,
                questions.len()
            ),
            pal.dim(),
        ));
        for line in wrap(&current.question, inner_width) {
            body.push(Line::raw(line));
        }
        body.push(Line::default());
        let option_count = current.options.as_ref().map_or(0, Vec::len);
        for (i, option) in current.options.iter().flatten().enumerate() {
            body.extend(option_rows(option, state.selected == i, inner_width, pal));
        }
        body.extend(option_rows(
            "✎ something else…",
            state.selected >= option_count,
            inner_width,
            pal,
        ));
        body.push(Line::default());
        if input_line {
            body.extend(input_rows("answer > ", &state.input, inner_width, pal));
        } else {
            body.push(Line::styled(
                "↑/↓ + enter · pick “something else” to write your own".to_string(),
                pal.dim(),
            ));
        }
    } else {
        // the request, rendered readably: the primary argument first, the
        // rest of the input as key: value lines
        for line in wrap(
            &format!(
                "{} {}",
                request.request.tool_name,
                tool_args_text(&request.request.input)
            ),
            inner_width,
        ) {
            body.push(Line::styled(line, pal.dim()));
        }
        if let Some(description) = &request.request.description {
            for line in wrap(description, inner_width) {
                body.push(Line::styled(line, pal.dim()));
            }
        }
        if let Some(reason) = &request.request.decision_reason {
            for line in wrap(reason, inner_width) {
                body.push(Line::styled(line, pal.error()));
            }
        }
        body.push(Line::default());
        if input_line {
            body.extend(input_rows(
                "deny because > ",
                &state.input,
                inner_width,
                pal,
            ));
        } else {
            body.push(Line::styled(
                "y allow once · a allow for this session · n deny with a reason".to_string(),
                pal.dim(),
            ));
        }
    }

    let height = (body.len() as u16 + 5).min(area.height.saturating_sub(2));
    let inner = panel(
        frame,
        centered(area, width, height),
        &title,
        OverlayRole::Permission,
        pal,
    );
    draw_lines(frame, inner, body);
}

/// One pickable option, wrapped: the marker leads the first row and the
/// rest hang under it, so a long label reads as one option rather than
/// running out of the panel.
fn option_rows(label: &str, selected: bool, width: usize, pal: &Palette) -> Vec<Line<'static>> {
    let marker = if selected { "▸ " } else { "  " };
    let style = if selected {
        pal.accent().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    wrap(label, width.saturating_sub(marker.width()))
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            let head = if i == 0 {
                marker.to_string()
            } else {
                " ".repeat(marker.width())
            };
            Line::from(vec![Span::styled(head, style), Span::styled(text, style)])
        })
        .collect()
}

// -- palette ----------------------------------------------------------------

fn palette(frame: &mut Frame, area: Rect, state: &PaletteState, pal: &Palette) {
    let width = 80u16.min(area.width.saturating_sub(4));
    let inner_width = width.saturating_sub(4) as usize;
    // budget the rows: the query (which wraps, so it is not always one
    // row), a blank, and the footer are fixed; every distinct group in the
    // window adds its label line
    let query = input_rows("▶ ", &state.query, inner_width, pal);
    let fixed = 4u16 + query.len() as u16; // query + blank + footer + borders
    let label_budget = 5u16; // five groups exist, each earning a label line
    let max_items = (area.height.saturating_sub(fixed + label_budget)).max(3) as usize;
    let shown = state.visible.len().min(max_items);
    // a window over the ranked list, the selection kept in view
    let selected = state.selected;
    let start = if state.visible.len() <= shown {
        0
    } else {
        selected
            .saturating_sub(shown / 2)
            .min(state.visible.len() - shown)
    };
    let window: Vec<usize> = state
        .visible
        .iter()
        .skip(start)
        .take(shown)
        .copied()
        .collect();
    let label_rows = {
        let mut labels: Vec<String> = Vec::new();
        for &index in &window {
            let label = state.items[index].group.label();
            if labels.last() != Some(&label) {
                labels.push(label);
            }
        }
        labels.len() as u16
    };
    let height = (shown as u16 + fixed + label_rows)
        .min(area.height.saturating_sub(2))
        .max(5);
    let inner = panel(
        frame,
        centered(area, width, height),
        "commands",
        OverlayRole::Palette,
        pal,
    );

    let mut lines: Vec<Line<'static>> = query;

    let mut previous_group: Option<String> = None;
    for &index in &window {
        let Some(item) = state.items.get(index) else {
            continue;
        };
        let label = item.group.label();
        if previous_group.as_deref() != Some(label.as_str()) {
            lines.push(Line::styled(label, pal.dim()));
            previous_group = Some(item.group.label());
        }
        let chosen = index == state.visible.get(selected).copied().unwrap_or(usize::MAX);
        let marker = if chosen { "▸ " } else { "  " };
        let label_style = if chosen {
            pal.accent().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let detail_style = if chosen {
            pal.dim().add_modifier(Modifier::BOLD)
        } else {
            pal.dim()
        };
        let mut spans = vec![
            Span::styled(marker.to_string(), label_style),
            Span::styled(item.label.clone(), label_style),
        ];
        if !item.detail.is_empty() {
            let detail = clip_detail(
                &item.detail,
                inner_width.saturating_sub(item.label.width() + 6),
            );
            spans.push(Span::styled(format!("  {detail}"), detail_style));
        }
        lines.push(Line::from(spans));
    }
    if state.visible.is_empty() {
        lines.push(Line::styled("(no matches)".to_string(), pal.dim()));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        format!(
            "{}/{} · enter run · esc close",
            if state.visible.is_empty() {
                0
            } else {
                selected + 1
            },
            state.visible.len()
        ),
        pal.dim(),
    ));
    // The row budget above is an estimate — group labels only appear for the
    // groups the window happens to span. When it comes out short, results are
    // what give way, never the footer: a panel that hides the key that closes
    // it is worse than one showing fewer matches.
    let room = inner.height as usize;
    while lines.len() > room && lines.len() > 3 {
        lines.remove(lines.len() - 3);
    }
    draw_lines(frame, inner, lines);
}

fn clip_detail(text: &str, max: usize) -> String {
    if max == 0 || text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        if used + ch.width().unwrap_or(1) > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += ch.width().unwrap_or(1);
    }
    out.push('…');
    out
}

// -- search -----------------------------------------------------------------

fn search(frame: &mut Frame, area: Rect, state: &SearchState, pal: &Palette) {
    let width = 50u16.min(area.width.saturating_sub(4));
    let inner_width = width.saturating_sub(4) as usize;
    let count = state.matches.len();
    let current = state
        .current
        .map_or_else(|| "·".to_string(), |c| (c + 1).to_string());
    // the query wraps, so the panel is sized around the rows it needs
    let mut lines = input_rows("/ ", &state.query, inner_width, pal);
    lines.push(Line::styled(
        format!("match {current} of {count} · enter keeps the highlights · esc closes"),
        pal.dim(),
    ));
    let height = (lines.len() as u16 + 3).min(area.height.saturating_sub(2));
    let inner = panel(
        frame,
        centered(area, width, height),
        "search",
        OverlayRole::Search,
        pal,
    );
    draw_lines(frame, inner, lines);
}

// -- brief ------------------------------------------------------------------

/// The selected session's full brief (the run's `taskBrief`, or the
/// rendered orchestrator prompt), scrollable. A missing source shows a
/// dimmed placeholder; long briefs page with the wheel and the scroll keys.
fn brief(frame: &mut Frame, area: Rect, state: &BriefState, pal: &Palette) {
    let width = 74u16.min(area.width.saturating_sub(4));
    let inner_width = width.saturating_sub(4) as usize;
    let lines = wrap(&state.text, inner_width);
    let inner = panel(
        frame,
        centered(area, width, area.height.saturating_sub(2)),
        "brief",
        OverlayRole::Help,
        pal,
    );
    // the window, clamped to the wrapped text: never past the last line
    let height = inner.height as usize;
    let offset = state.offset.min(lines.len().saturating_sub(height));
    let mut body: Vec<Line<'static>> = lines
        .iter()
        .skip(offset)
        .take(height.saturating_sub(1))
        .map(|line| {
            let style = if state.placeholder {
                pal.dim()
            } else {
                Style::default()
            };
            Line::styled(line.clone(), style)
        })
        .collect();
    if lines.len() > offset + height.saturating_sub(1) {
        body.push(Line::styled(
            format!(
                "… {} more line{} — scroll or ctrl-d/ctrl-u",
                lines.len() - offset - height.saturating_sub(1),
                if lines.len() - offset - height.saturating_sub(1) == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            pal.dim(),
        ));
    } else if height > body.len() {
        body.push(Line::default());
        body.push(Line::styled("esc closes", pal.dim()));
    }
    draw_lines(frame, inner, body);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_spaces_then_inside_a_word_that_cannot_fit() {
        let lines = wrap("one two three four", 8);
        assert_eq!(lines, vec!["one two", "three", "four"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"], "explicit newlines hold");

        // a word with no wrap point of its own is broken rather than left to
        // run out of the panel — a url or a path is the usual culprit
        let lines = wrap("supercalifragilistic", 4);
        assert_eq!(lines, vec!["supe", "rcal", "ifra", "gili", "stic"]);
        assert_eq!(lines.concat(), "supercalifragilistic", "nothing lost");
        let lines = wrap("see https://example.com/a/very/long/path now", 12);
        assert!(
            lines.iter().all(|l| l.width() <= 12),
            "every row fits: {lines:?}"
        );
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>().len(),
            lines.iter().filter(|l| !l.is_empty()).count(),
            "the break points are the only new whitespace: {lines:?}"
        );
    }

    #[test]
    fn a_typed_field_wraps_under_its_prompt_and_keeps_the_caret() {
        let pal = Palette::plain();
        let rows = input_rows("answer > ", "one two three four five six", 20, &pal);
        let text: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(rows.len() > 1, "it wrapped: {text:?}");
        assert!(text[0].starts_with("answer > "), "{text:?}");
        assert!(
            text[1..].iter().all(|r| r.starts_with("         ")),
            "continuations hang under the prompt: {text:?}"
        );
        assert!(
            text.iter().all(|r| r.width() <= 21),
            "no row leaves the panel (the caret is the 21st): {text:?}"
        );
        assert!(
            text.last().is_some_and(|r| r.ends_with('▍')),
            "the caret rides the last row: {text:?}"
        );
    }

    #[test]
    fn a_long_field_keeps_its_tail_so_the_caret_stays_on_screen() {
        let pal = Palette::plain();
        let long = "word ".repeat(200);
        let rows = input_rows("answer > ", &long, 20, &pal);
        assert_eq!(rows.len(), INPUT_ROWS, "it stops growing");
        let first: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.trim_start().starts_with('…'), "{first:?}");
        let last: String = rows[INPUT_ROWS - 1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.ends_with('▍'), "{last:?}");
    }

    #[test]
    fn an_empty_field_is_still_one_row_with_a_caret() {
        let pal = Palette::plain();
        let rows = input_rows("/ ", "", 20, &pal);
        assert_eq!(rows.len(), 1);
        let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "/ ▍");
    }
}
