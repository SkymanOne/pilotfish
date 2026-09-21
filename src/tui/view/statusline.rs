//! The status line: what is selected, in one row. For a worker its state,
//! model, reasoning level and branch; for the orchestrator its model,
//! session, spend and turns. Plus the pending-approval count, the
//! permission mode when it is not the default, and the console's mode
//! indicator (NORMAL/INSERT) on the right.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::fleet::run::derive_view;
use crate::tui::app::Console;
use crate::tui::model::SessionTarget;
use crate::tui::theme::Palette;
use crate::tui::view::Feeds;
use crate::util::now_ms;

/// Draw one status row across `area`.
pub fn draw(frame: &mut Frame, area: Rect, console: &Console, feeds: &Feeds<'_>, pal: &Palette) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let part = |parts: &mut Vec<Span<'static>>, text: String, style: ratatui::style::Style| {
        if !parts.is_empty() {
            parts.push(Span::styled(" · ".to_string(), pal.dim()));
        }
        parts.push(Span::styled(text, style));
    };

    match console.selected_target() {
        SessionTarget::Worker { run_id } => {
            if let Some(entry) = feeds.runs.iter().find(|r| r.run_id == run_id) {
                let state = &entry.state;
                let view = derive_view(state, crate::fleet::run::is_alive, now_ms());
                let state_style = if state.pending_question.is_some() {
                    pal.attention()
                } else {
                    pal.dim()
                };
                part(&mut spans, state.name.clone(), pal.accent());
                part(&mut spans, view.to_string(), state_style);
                part(
                    &mut spans,
                    state.model_label().unwrap_or("default model").to_string(),
                    pal.dim(),
                );
                if let Some(level) = &state.thinking_level {
                    part(&mut spans, format!("thinking {level}"), pal.dim());
                }
                if let Some(branch) = &state.branch {
                    part(&mut spans, branch.clone(), pal.dim());
                }
            } else {
                part(&mut spans, "gone".to_string(), pal.error());
            }
        }
        SessionTarget::Orchestrator(_) => {
            let transcript = console.orchestrator_transcript();
            let model = transcript
                .model()
                .or(feeds.orch.model.as_deref())
                .unwrap_or("starting…");
            part(&mut spans, model.to_string(), pal.dim());
            part(
                &mut spans,
                transcript.session_id().map_or_else(
                    || "no session".to_string(),
                    |id| id.chars().take(8).collect(),
                ),
                pal.dim(),
            );
            part(
                &mut spans,
                format!("${:.3}", transcript.cost_usd()),
                pal.dim(),
            );
            let turns = transcript.num_turns();
            part(
                &mut spans,
                format!("{turns} turn{}", if turns == 1 { "" } else { "s" }),
                pal.dim(),
            );
            if let Some(effort) = console.effort() {
                part(&mut spans, format!("thinking {effort}"), pal.dim());
            }
            let working = feeds.orch.turn_active || transcript.turn_active();
            if working {
                part(&mut spans, "working".to_string(), pal.attention());
            }
            // only worth saying when it is not the mode that asks about everything
            let mode = &feeds.orch.permission_mode;
            if mode != "default" {
                part(&mut spans, format!("perms {mode}"), pal.attention());
            }
        }
    }

    // a wheel that stopped scrolling must never be a mystery
    if !console.mouse_captured() {
        part(&mut spans, "select".to_string(), pal.attention());
    }

    let approvals = feeds.orch.pending_requests.len();
    if approvals > 0 {
        part(
            &mut spans,
            format!(
                "{} approval{} pending",
                approvals,
                if approvals == 1 { "" } else { "s" }
            ),
            pal.attention(),
        );
    }

    // the chords ride on the right, so the way out of here is always on
    // screen; the facts on the left win the row when it is tight
    let chip = Span::styled(" ctrl+f fleet · ctrl+k commands ".to_string(), pal.dim());
    let width = area.width as usize;
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    let chip_width = chip.content.width();
    if used + chip_width < width {
        spans.push(Span::raw(" ".repeat(width - used - chip_width)));
        spans.push(chip);
    } else {
        truncate_spans(&mut spans, width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Drop whole spans off the end until the line fits `width` *columns* —
/// counting spans against a width in columns let a handful of wide facts
/// overflow the row.
fn truncate_spans(spans: &mut Vec<Span<'static>>, width: usize) {
    let mut used = 0;
    let mut keep = 0;
    for span in spans.iter() {
        let w = span.content.width();
        if used + w > width {
            break;
        }
        used += w;
        keep += 1;
    }
    spans.truncate(keep);
}
