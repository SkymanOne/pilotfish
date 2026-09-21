//! The draw functions: one module per region of the screen, one
//! orchestration point here. Every draw call is pure over the view model:
//! the `Console` state machine, plus the [`Feeds`] the runtime polled from
//! `.parl` this frame. Nothing here mutates state except
//! `console.viewport_rows`, which the state machine's scrolling keys read.

pub mod composer;
pub mod dashboard;
pub mod overlay;
pub mod session;
pub mod statusline;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::orch::records::OrchestratorState;
use crate::tui::app::{Console, RunEntry};
use crate::tui::theme::Palette;

/// The facts the runtime polled from `.parl` and hands the renderer beside
/// the `Console`: the orchestrator's durable state (permission mode, pending
/// approvals) and every run's, so the status line and the permission overlay
/// can read what the state machine deliberately keeps private.
pub struct Feeds<'a> {
    pub orch: &'a OrchestratorState,
    pub runs: &'a [RunEntry],
}

/// Draw the whole console for one frame. Infallible: every widget here
/// renders into the buffer, nothing touches fallible IO.
pub fn draw(frame: &mut Frame, console: &mut Console, feeds: &Feeds<'_>, pal: &Palette) {
    let area = frame.area();
    let [main, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    session::draw(frame, main, console, feeds, pal);
    statusline::draw(frame, status, console, feeds, pal);
    // overlays go last so they sit over everything, dimming what is behind
    if let Some(overlay) = console.overlay().cloned() {
        overlay::draw(frame, area, console, feeds, &overlay, pal);
    }
    scrub_controls(frame.buffer_mut());
}

/// The last thing every frame passes through: no control character reaches
/// the terminal.
///
/// Almost everything on screen is written by an agent or by whatever it
/// ran, and command-line tools draw with control characters — `git rebase`
/// emits `Rebasing (1/6)\rRebasing (2/6)\r…`. A cell holding a bare CR
/// sends the cursor back to column 0 mid-row, so the rest of the row
/// repaints over what was already drawn and the frame tears; an `ESC` could
/// do considerably worse. Models cannot be careless here, so this is the one
/// place it is caught, after every widget has had its say: a control cell
/// becomes a space, which keeps the geometry the layout already computed.
///
/// [`crate::util::visible_line`] keeps the transcript's own text honest, so
/// what reaches here is the residue — a widget that skipped it, or a source
/// added later.
fn scrub_controls(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        if cell.symbol().chars().any(char::is_control) {
            cell.set_symbol(" ");
        }
    }
}

/// Clip to `max` printed columns, ellipsis on the cut: a name yields for
/// the age column rather than pushing it off the row.
pub(crate) fn clip_to(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(1);
        if used + w > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fleet::run::{PendingQuestion, RunState, RunStatus};

    #[test]
    fn the_frame_scrub_leaves_no_control_character_for_the_terminal() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        for (x, ch) in "a\rb\u{1b}c\t".chars().enumerate() {
            buffer[(x as u16, 0)].set_symbol(&ch.to_string());
        }
        scrub_controls(&mut buffer);
        let row: String = (0..6).map(|x| buffer[(x, 0)].symbol()).collect();
        assert_eq!(row, "a b c ", "controls became spaces, the width held");
        assert!(!row.chars().any(char::is_control));
    }

    use crate::orch::protocol::{CanUseToolRequest, PermissionRequest};
    use crate::orch::records::{EventRecord, OrchestratorEvent};
    use crate::paths::FleetPaths;
    use crate::util::now_iso;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// A pid that is alive no matter what.
    fn alive_pid() -> i32 {
        i32::try_from(std::process::id()).unwrap()
    }

    fn run_state(name: &str, run_id: &str) -> RunState {
        RunState::new(
            "/f", run_id, name, "/repo", "brief", None, None, None, None, None, None, None, None,
            None, None, None,
        )
    }

    fn running(name: &str, run_id: &str) -> RunEntry {
        let mut state = run_state(name, run_id);
        state.status = RunStatus::Running;
        state.pid = Some(alive_pid());
        state.branch = Some(format!("parl/{name}-7"));
        RunEntry {
            run_id: run_id.to_string(),
            state,
        }
    }

    fn blocked(name: &str, run_id: &str) -> RunEntry {
        let mut entry = running(name, run_id);
        entry.state.pending_question = Some(PendingQuestion {
            id: "q_1".into(),
            question: "bcrypt or argon2?".into(),
            options: None,
            context: None,
            asked_at: now_iso(),
        });
        entry
    }

    fn settled(name: &str, run_id: &str) -> RunEntry {
        let mut state = run_state(name, run_id);
        state.status = RunStatus::Settled;
        RunEntry {
            run_id: run_id.to_string(),
            state,
        }
    }

    /// A fleet: the orchestrator, a running worker, a blocked one, a done one.
    fn fleet() -> (Console, Vec<RunEntry>, OrchestratorState) {
        let dir = std::env::temp_dir().join(format!(
            "parl-tui-view-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut console = Console::new(FleetPaths::new(dir));
        let runs = vec![
            running("db", "db-20260829120000"),
            blocked("api", "api-20260829120001"),
            settled("tests", "tests-20260829120002"),
        ];
        console.set_runs(runs.clone());
        console.set_orchestrator_state(OrchestratorState::default());
        (console, runs, OrchestratorState::default())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    /// A notice record, so tests can feed the orchestrator transcript the way
    /// the runtime does (through the public ingest path).
    fn notice_record(text: &str) -> EventRecord {
        OrchestratorEvent::Notice {
            text: text.to_string(),
            error: None,
        }
        .to_record()
    }

    fn draw_to_buffer(
        console: &mut Console,
        runs: &[RunEntry],
        orch: &OrchestratorState,
        w: u16,
        h: u16,
    ) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let feeds = Feeds { orch, runs };
        terminal
            .draw(|frame| draw(frame, console, &feeds, &Palette::colored()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut out = String::new();
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.trim_end().to_string()
    }

    fn find_row(buf: &Buffer, needle: &str) -> Option<u16> {
        (0..buf.area.height).find(|&y| row_text(buf, y).contains(needle))
    }

    fn assert_visible(buf: &Buffer, needle: &str) {
        assert!(
            find_row(buf, needle).is_some(),
            "{needle:?} not drawn anywhere:\n{}",
            (0..buf.area.height)
                .map(|y| row_text(buf, y))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    // -- the fleet overlay ----------------------------------------------------

    fn ctrl_f() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
    }

    #[test]
    fn the_fleet_overlay_draws_a_fleet_of_workers_in_different_states() {
        let (mut console, runs, orch) = fleet();
        console.handle_key(ctrl_f());
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 20);

        // header: the fleet summary — db running, api asking, tests done
        assert_visible(&buf, "parl");
        assert_visible(&buf, "orchestrator + 3 workers");
        assert_visible(&buf, "running 1");
        assert_visible(&buf, "needs an answer 1");
        assert_visible(&buf, "done 1");

        // rows, orchestrator first, glyph + name + age on the primary line
        let orch_row = find_row(&buf, "○ orchestrator").expect("orchestrator row");
        assert!(
            row_text(&buf, orch_row).contains("▸"),
            "selected by default"
        );
        assert!(row_text(&buf, orch_row).contains("○"), "idle glyph");
        assert_visible(&buf, "● db");
        assert!(row_text(&buf, find_row(&buf, "● db").unwrap()).contains("parl/db-7"));
        assert_visible(&buf, "? api");
        assert_visible(&buf, "✓ tests");

        // the detail line under the blocked worker says what it needs
        assert_visible(&buf, "needs an answer");

        // the footer carries the panel's key hints
        assert_visible(&buf, "j/k move");
    }

    #[test]
    fn the_fleet_overlay_marks_the_selected_worker_and_shows_the_flash() {
        let (mut console, runs, orch) = fleet();
        console.handle_key(ctrl_f());
        console.handle_key(ch('j'));
        console.toast("! that worker is gone", true);
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 20);

        // the selection moved to db; its row is marked, the orchestrator is not
        let db_row = find_row(&buf, "● db").unwrap();
        assert!(row_text(&buf, db_row).contains("▸"));
        let orch_row = find_row(&buf, "○ orchestrator").unwrap();
        assert!(!row_text(&buf, orch_row).contains("▸"));
        // the flash replaced the hints in the footer
        assert_visible(&buf, "! that worker is gone");
    }

    // -- session view ---------------------------------------------------------

    #[test]
    fn the_conversation_draws_the_transcript_and_the_composer_with_no_rail() {
        let (mut console, runs, orch) = fleet();
        console.ingest_orchestrator_record(&notice_record("· halfway there"));
        for c in "fix the tests".chars() {
            console.handle_key(ch(c));
        }
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 20);

        // the transcript shows the fed block
        assert_visible(&buf, "· halfway there");
        // the composer is aimed at the orchestrator and holds the message
        assert_visible(&buf, "orchestrator > ");
        assert_visible(&buf, "fix the tests");
        // the way into the fleet is always on screen
        assert_visible(&buf, "ctrl+f fleet");
        // no rail: the other sessions are not listed beside the transcript
        let drawn = (0..buf.area.height)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !drawn.contains("● db"),
            "the fleet is an overlay, not a column: {drawn}"
        );
    }

    #[test]
    fn the_session_view_renders_transcript_block_kinds_with_blank_separators() {
        let (mut console, runs, orch) = fleet();
        // a sent prompt lands as a cyan user block
        console.submit("hello there");
        // a fleet batch lands as a yellow event block
        let events = vec![crate::fleet::event::FleetEvent::new(
            crate::fleet::event::FleetEventKind::Settled,
            "db-20260829120000",
            "db",
            vec![],
        )];
        console.ingest_fleet_events(&events, "BATCH");
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 20);
        assert_visible(&buf, "> hello there");
        assert_visible(&buf, "⚑ settled db");
    }

    // -- status line ----------------------------------------------------------

    #[test]
    fn the_status_line_shows_the_selected_worker_facts() {
        let (mut console, runs, orch) = fleet();
        console.handle_key(ctrl_f());
        console.handle_key(ch('j'));
        console.handle_key(key(KeyCode::Enter));
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 12);
        let status = row_text(&buf, buf.area.height - 1);
        assert!(status.contains("db"), "{status}");
        assert!(status.contains("running"), "{status}");
        assert!(status.contains("default model"), "{status}");
        assert!(status.contains("parl/db-7"), "{status}");
    }

    #[test]
    fn the_status_line_shows_the_orchestrator_spend_and_pending_approvals() {
        let (mut console, runs, mut orch) = fleet();
        orch.pending_requests = vec![PermissionRequest {
            request_id: "req_1".into(),
            request: CanUseToolRequest {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "touch a.txt"}),
                tool_use_id: "t1".into(),
                ..CanUseToolRequest::default()
            },
            received_at: now_iso(),
        }];
        console.set_orchestrator_state(orch.clone());
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 12);
        let status = row_text(&buf, buf.area.height - 1);
        assert!(status.contains("starting…"), "no model yet: {status}");
        assert!(status.contains("$0.000"), "{status}");
        assert!(status.contains("0 turns"), "{status}");
        assert!(status.contains("1 approval pending"), "{status}");
    }

    // -- overlays -------------------------------------------------------------

    #[test]
    fn the_help_overlay_lists_the_keys() {
        let (mut console, runs, orch) = fleet();
        console.handle_key(ctrl_f());
        console.handle_key(ch('?'));
        // tall enough that help_lines shows every row, not a counted tail
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 50);
        assert_visible(&buf, "keys");
        assert_visible(&buf, "move the selection");
        assert_visible(&buf, "the command palette");
    }

    #[test]
    fn the_confirm_overlay_asks_before_destroying() {
        let (mut console, runs, orch) = fleet();
        console.handle_key(ctrl_f());
        console.handle_key(ch('j'));
        console.handle_key(ch('x'));
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 20);
        assert_visible(&buf, "confirm");
        assert_visible(&buf, "Abort it and remove");
        assert_visible(&buf, "y confirm");
    }

    #[test]
    fn the_palette_overlay_groups_and_labels_its_results() {
        let (mut console, runs, orch) = fleet();
        console.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 30);
        assert_visible(&buf, "commands");
        assert_visible(&buf, "console");
        assert_visible(&buf, "sessions");
        assert_visible(&buf, "/quit");
        assert_visible(&buf, "enter run · esc close");
    }

    #[test]
    fn the_permission_overlay_draws_the_request_and_its_choices() {
        let (mut console, runs, mut orch) = fleet();
        orch.pending_requests = vec![PermissionRequest {
            request_id: "req_1".into(),
            request: CanUseToolRequest {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "touch a.txt"}),
                tool_use_id: "t1".into(),
                title: Some("Run touch a.txt".into()),
                ..CanUseToolRequest::default()
            },
            received_at: now_iso(),
        }];
        // a blocked orchestrator raises its own prompt
        console.set_orchestrator_state(orch.clone());
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 24);
        assert_visible(&buf, "Run touch a.txt");
        assert_visible(&buf, "Bash touch a.txt");
        assert_visible(
            &buf,
            "y allow once · a allow for this session · n deny with a reason",
        );
    }

    #[test]
    fn the_permission_overlay_picks_from_ask_user_question_options() {
        let (mut console, runs, mut orch) = fleet();
        orch.pending_requests = vec![PermissionRequest {
            request_id: "req_2".into(),
            request: CanUseToolRequest {
                tool_name: "AskUserQuestion".into(),
                input: serde_json::json!({"questions": [
                    {"question": "Which hash?", "options": [{"label": "bcrypt"}, {"label": "argon2"}]},
                ]}),
                tool_use_id: "t2".into(),
                ..CanUseToolRequest::default()
            },
            received_at: now_iso(),
        }];
        console.set_orchestrator_state(orch.clone());
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 24);
        assert_visible(&buf, "question 1/1");
        assert_visible(&buf, "Which hash?");
        assert_visible(&buf, "bcrypt");
        assert_visible(&buf, "argon2");
        assert_visible(&buf, "✎ something else…");
    }

    #[test]
    fn the_search_overlay_counts_its_matches() {
        let (mut console, runs, orch) = fleet();
        console.ingest_orchestrator_record(&notice_record("the quick brown fox"));
        console.ingest_orchestrator_record(&notice_record("another quick fox"));
        console.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        for c in "quick".chars() {
            console.handle_key(ch(c));
        }
        let buf = draw_to_buffer(&mut console, &runs, &orch, 100, 24);
        assert_visible(&buf, "search");
        assert_visible(&buf, "quick");
        assert_visible(&buf, "match 1 of 2");
    }
}
