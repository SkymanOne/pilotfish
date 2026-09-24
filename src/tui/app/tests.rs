//! The console's unit tests: the state machine driven through real key
//! events, with every effect asserted rather than executed.

#![allow(clippy::unwrap_used)]

use super::*;
use crate::fleet::run::{PendingDialog, PendingQuestion, RunStatus, WorkerCommand, WorkerModel};
use crate::orch::protocol::{AgentCommand, CanUseToolRequest, McpServerStatus, PermissionRequest};
use crate::orch::session::OrchestratorSession;
use crate::tui::palette::PaletteAction;
use crossterm::event::{KeyCode, KeyModifiers};
use uuid::Uuid;

fn test_console() -> Console {
    let dir = std::env::temp_dir().join(format!(
        "pilotfish-tui-app-{}-{}",
        std::process::id(),
        crate::util::new_id("t").replace('_', "")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut console = Console::new(FleetPaths::new(&dir));
    // never the developer's own ~/.pilotfish: it can hold a real key
    console.user_dir = Some(dir.join("home"));
    console
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn ctrl_code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn enter() -> KeyEvent {
    key(KeyCode::Enter)
}

fn esc() -> KeyEvent {
    key(KeyCode::Esc)
}

fn tab() -> KeyEvent {
    key(KeyCode::Tab)
}

fn running_run(run_id: &str, name: &str) -> RunEntry {
    let mut state = RunState::new(
        "/f", run_id, name, "/repo", "brief", None, None, None, None, None, None, None, None, None,
        None, None,
    );
    state.status = RunStatus::Running;
    state.pid = Some(std::process::id() as i32);
    RunEntry {
        run_id: run_id.to_string(),
        state,
    }
}

fn setup_with_worker() -> Console {
    let mut console = test_console();
    console.set_runs(vec![running_run("db-20260829120000", "db")]);
    console
}

/// Type `text` into the composer, which always has focus.
fn type_text(c: &mut Console, text: &str) {
    for character in text.chars() {
        c.handle_key(ch(character));
    }
}

/// Let a self-raised permission prompt's grace window pass, as it would for
/// a human reading it before answering.
fn read_the_prompt(c: &mut Console) {
    c.raised_at = None;
}

/// Open the fleet overlay, where single letters are commands.
fn open_fleet(c: &mut Console) {
    c.handle_key(ctrl('f'));
}

/// Select the nth fleet row and come back to the conversation.
fn select_row(c: &mut Console, index: usize) {
    open_fleet(c);
    let digit = char::from_digit(index as u32 + 1, 10).expect("rows 1-9");
    c.handle_key(ch(digit));
    c.handle_key(enter());
}

/// Press one of the fleet overlay's letter commands, leaving the console
/// back in the conversation unless the command opened something itself.
fn fleet_key(c: &mut Console, letter: char) -> Vec<Effect> {
    open_fleet(c);
    let effects = c.handle_key(ch(letter));
    if matches!(c.overlay(), Some(Overlay::Fleet)) {
        c.handle_key(esc());
    }
    effects
}

// -- navigation ----------------------------------------------------------

#[test]
fn fleet_navigation() {
    {
        let mut c = setup_with_worker();
        open_fleet(&mut c);
        assert_eq!(c.selected(), 0, "the orchestrator first");
        c.handle_key(ch('j'));
        assert_eq!(c.selected(), 1);
        c.handle_key(ch('j'));
        assert_eq!(c.selected(), 0, "wraps around");
        c.handle_key(ch('k'));
        assert_eq!(c.selected(), 1);
        c.handle_key(ch('G'));
        assert_eq!(c.selected(), 1);
        c.handle_key(ch('g'));
        assert_eq!(c.selected(), 0);
        c.handle_key(key(KeyCode::Down));
        assert_eq!(c.selected(), 1);
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.selected(), 0);
        c.handle_key(ch('2'));
        assert_eq!(c.selected(), 1, "1-9 jumps to the nth session");
        c.handle_key(ch('9'));
        assert_eq!(c.selected(), 1, "out of range jumps nowhere");
    }
    {
        let mut c = test_console();
        c.set_runs(vec![running_run("auth-20260830000000", "auth")]);
        assert_eq!(c.selected(), 0, "the orchestrator starts selected");
        assert!(c.select_target("auth-20260830000000"));
        assert_eq!(c.selected(), 1);
        assert_eq!(
            c.selected_target(),
            SessionTarget::Worker {
                run_id: "auth-20260830000000".into()
            }
        );
        assert!(c.select_target("orchestrator"));
        assert_eq!(c.selected(), 0);
        // an unknown key leaves the selection alone: the caller falls back
        assert!(!c.select_target("ghost-20260830000000"));
        assert_eq!(c.selected(), 0);
    }
    {
        let mut c = test_console();
        c.set_runs(vec![running_run("auth-20260830000000", "auth")]);
        c.set_diff_stat("auth-20260830000000", "+12 −3");
        assert_eq!(c.rows()[1].diff_stat.as_deref(), Some("+12 −3"));
        c.clear_diff_stat("auth-20260830000000");
        assert_eq!(c.rows()[1].diff_stat, None);
    }
    {
        let mut c = setup_with_worker();
        assert_eq!(c.overlay(), None, "the conversation is the console");
        open_fleet(&mut c);
        assert_eq!(c.overlay(), Some(&Overlay::Fleet));
        c.handle_key(ch('j'));
        c.handle_key(enter());
        assert_eq!(c.overlay(), None, "enter takes the row it was on");
        assert_eq!(c.selected(), 1);
        open_fleet(&mut c);
        c.handle_key(esc());
        assert_eq!(c.overlay(), None, "esc leaves it where it was");
        assert_eq!(c.selected(), 1);
    }
    {
        let mut c = setup_with_worker();
        // in the conversation every printable is text, `q` and `j` included
        type_text(&mut c, "quick job");
        assert_eq!(c.composer().input, "quick job");
        assert_eq!(c.overlay(), None);
        assert_eq!(c.selected(), 0, "nothing moved the selection");
    }
    {
        let mut c = setup_with_worker();
        c.handle_key(ch('h'));
        assert_eq!(c.composer().input, "h");
        c.handle_key(ch('e'));
        assert_eq!(c.composer().input, "he");
        // `q` used to quit the console; it is a letter like any other now
        c.handle_key(ch('q'));
        assert_eq!(c.composer().input, "heq");
        c.handle_key(enter());
        assert!(
            c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.text == "> heq")
        );
    }
    {
        let mut c = setup_with_worker();
        fleet_key(&mut c, '?');
        assert!(matches!(c.overlay(), Some(Overlay::Help)));
        // the key that opened it closes it, and so do esc and `q`
        c.handle_key(ch('?'));
        assert!(c.overlay().is_none());
        fleet_key(&mut c, '?');
        c.handle_key(ch('q'));
        assert!(c.overlay().is_none(), "`q` closes a help panel");
        fleet_key(&mut c, '?');
        c.handle_key(esc());
        assert!(c.overlay().is_none());
        // and `/help` reaches the same panel
        c.submit("/help");
        assert!(matches!(c.overlay(), Some(Overlay::Help)));
    }
}

// -- selection and diff stats --------------------------------------------

// -- the composer --------------------------------------------------------

#[test]
fn composer_input() {
    {
        let mut c = setup_with_worker();
        type_text(&mut c, "abc");
        assert_eq!(c.composer().input, "abc");
        c.handle_key(key(KeyCode::Left));
        c.handle_key(ch('X'));
        assert_eq!(c.composer().input, "abXc");
        c.handle_key(key(KeyCode::Backspace));
        assert_eq!(c.composer().input, "abc");
        c.handle_key(key(KeyCode::Home));
        c.handle_key(ch('-'));
        assert_eq!(c.composer().input, "-abc");
        c.handle_key(key(KeyCode::End));
        c.handle_key(ch('!'));
        assert_eq!(c.composer().input, "-abc!");
        c.handle_key(key(KeyCode::Delete));
        assert_eq!(c.composer().input, "-abc!");
    }
    {
        let mut c = setup_with_worker();
        type_text(&mut c, "hi");
        let effects = c.handle_key(enter());
        assert_eq!(effects, vec![Effect::SendToOrchestrator("hi".to_string())]);
        assert_eq!(c.composer().input, "");
        assert!(
            c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.text == "> hi")
        );
    }
    {
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        type_text(&mut c, "hi");
        let effects = c.handle_key(enter());
        assert_eq!(
            effects,
            vec![Effect::WorkerSteer {
                run_id: "db-20260829120000".to_string(),
                message: "hi".to_string(),
            }]
        );
    }
    {
        for modifier in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut c = setup_with_worker();
            c.handle_key(ch('a'));
            c.handle_key(KeyEvent::new(KeyCode::Enter, modifier));
            c.handle_key(ch('b'));
            let effects = c.handle_key(enter());
            assert_eq!(
                effects,
                vec![Effect::SendToOrchestrator("a\nb".to_string())],
                "{modifier:?}+enter"
            );
        }
    }
    {
        let mut c = setup_with_worker();
        c.paste("first line\r\nsecond line\n");
        assert_eq!(
            c.composer().input,
            "first line\nsecond line\n",
            "nothing was sent at the first newline"
        );
        assert_eq!(
            c.handle_key(enter()),
            vec![Effect::SendToOrchestrator(
                "first line\nsecond line".to_string()
            )]
        );
    }
    {
        let mut c = setup_with_worker();
        c.submit("first message");
        c.submit("second message");
        type_text(&mut c, "x");
        // still in insert mode: up recalls, it does not move the selection
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.composer().input, "second message");
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.composer().input, "first message");
        c.handle_key(key(KeyCode::Down));
        assert_eq!(c.composer().input, "second message");
    }
    {
        let mut c = setup_with_worker();
        c.toast("hello", false);
        let at = c.flash().unwrap().at;
        c.tick(at + FLASH_MS + 1);
        assert!(c.flash().is_none());
        c.toast("hello", false);
        let at = c.flash().unwrap().at;
        c.tick(at + 100);
        assert!(c.flash().is_some());
    }
}

// -- sending -------------------------------------------------------------

/// What claude says it offers, answered just now.
fn claude_offers(c: &mut Console, names: &[&str]) {
    c.set_capabilities(Capabilities {
        fetched_at: crate::util::now_iso(),
        commands: names
            .iter()
            .map(|name| AgentCommand {
                name: (*name).into(),
                description: None,
                argument_hint: None,
                aliases: None,
            })
            .collect(),
        ..Capabilities::default()
    });
}

#[test]
fn command_routing() {
    {
        let mut c = setup_with_worker();
        claude_offers(&mut c, &["compact", "usage"]);
        let effects = c.submit("/pemissions auto");
        assert!(effects.is_empty(), "nothing reaches claude: {effects:?}");
        assert!(
            c.flash()
                .unwrap()
                .text
                .contains("unknown command /pemissions")
        );
    }
    {
        let mut c = setup_with_worker();
        c.set_capabilities(Capabilities {
            fetched_at: "2020-01-01T00:00:00.000Z".into(),
            commands: vec![AgentCommand {
                name: "usage".into(),
                description: None,
                argument_hint: None,
                aliases: None,
            }],
            ..Capabilities::default()
        });
        let effects = c.submit("/review");
        assert_eq!(effects, vec![Effect::RefreshCapabilities]);
    }
    {
        // a fresh console, or a monitor from an older build: no list to judge by
        let mut c = setup_with_worker();
        assert_eq!(
            c.submit("/compact"),
            vec![Effect::SendToOrchestrator("/compact".to_string())]
        );
    }
    {
        assert_eq!(
            suggest_command("/pemissions", &["/permissions".to_string()]),
            Some("/permissions".to_string())
        );
        assert_eq!(
            suggest_command("/zzzzzz", &["/permissions".to_string()]),
            None
        );
    }
    {
        let mut c = setup_with_worker();
        c.set_capabilities(Capabilities {
            commands: vec![AgentCommand {
                name: "usage".into(),
                description: Some("Show usage".into()),
                argument_hint: None,
                aliases: None,
            }],
            ..Capabilities::default()
        });
        let effects = c.submit("/usage");
        assert_eq!(
            effects,
            vec![Effect::SendToOrchestrator("/usage".to_string())]
        );
    }
    {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.commands = vec![WorkerCommand {
            name: "skill:review".into(),
            description: "Review the diff".into(),
            source: "skill".into(),
        }];
        c.set_runs(vec![entry]);
        select_row(&mut c, 1);
        let effects = c.submit("/skill:review");
        assert_eq!(
            effects,
            vec![Effect::WorkerCommand {
                run_id: "db-20260829120000".to_string(),
                message: "/skill:review".to_string(),
            }]
        );
    }
    {
        let mut c = setup_with_worker();
        let events = vec![crate::fleet::event::FleetEvent::new(
            crate::fleet::event::FleetEventKind::Question,
            "db-20260829120000",
            "db",
            vec![],
        )];
        let effects = c.ingest_fleet_events(&events, "BATCH");
        assert_eq!(
            effects,
            vec![Effect::SendToOrchestrator("BATCH".to_string())]
        );
        assert!(
            c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.text.contains("⚑ question db"))
        );
    }
}

// -- completions ---------------------------------------------------------

#[test]
fn completion_popups() {
    {
        let mut c = setup_with_worker();
        c.set_capabilities(Capabilities {
            commands: vec![AgentCommand {
                name: "usage".into(),
                description: None,
                argument_hint: None,
                aliases: None,
            }],
            ..Capabilities::default()
        });
        c.handle_key(ch('/'));
        let completion = c.composer().completion.as_ref().unwrap();
        let labels: Vec<&str> = completion.items.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"/help"));
        assert!(!labels.contains(&"/answer"), "worker-only commands hide");
        assert!(labels.contains(&"/usage"), "the agent's own ride along");
        // tab accepts the highlighted suggestion
        c.handle_key(ch('q'));
        c.handle_key(tab());
        assert_eq!(c.composer().input, "/quit", "/quit takes no argument");
        // accepting does not run it
    }
    {
        let mut c = setup_with_worker();
        c.set_files(vec!["src/main.rs".into()]);
        c.handle_key(ch('@'));
        let completion = c.composer().completion.as_ref().unwrap();
        assert_eq!(completion.items[0].label, "@db");
        c.handle_key(ch('d'));
        c.handle_key(ch('b'));
        c.handle_key(tab());
        assert_eq!(c.composer().input, "@db");
    }
}

// -- the palette ---------------------------------------------------------

#[test]
fn palette_actions() {
    {
        let mut c = setup_with_worker();
        c.set_capabilities(Capabilities {
            commands: vec![AgentCommand {
                name: "usage".into(),
                description: None,
                argument_hint: None,
                aliases: None,
            }],
            mcp_servers: vec![McpServerStatus {
                name: "fleet".into(),
                status: "connected".into(),
            }],
            tools: vec!["Bash".into(), "mcp__fleet__fleet_spawn".into()],
            ..Capabilities::default()
        });
        c.handle_key(ctrl('k'));
        let Overlay::Palette(palette) = c.overlay().unwrap() else {
            panic!("palette should be open");
        };
        let has_group = |label: &str| {
            palette.items.iter().any(|item| match &item.group {
                crate::tui::palette::PaletteGroup::Console => label == "console",
                crate::tui::palette::PaletteGroup::Agent { .. } => label == "agent",
                crate::tui::palette::PaletteGroup::Servers => label == "servers",
                crate::tui::palette::PaletteGroup::Models => label == "models",
                crate::tui::palette::PaletteGroup::Sessions => label == "sessions",
            })
        };
        for group in ["console", "agent", "servers", "models", "sessions"] {
            assert!(has_group(group), "{group} group missing");
        }
        // the mcp tool landed, and the sessions are jumpable
        assert!(
            palette
                .items
                .iter()
                .any(|i| i.label == "mcp__fleet__fleet_spawn")
        );
        assert!(palette.items.iter().any(|i| i.label == "db"));
        // fuzzy narrowing works
        let mut narrowed = palette.clone();
        narrowed.query = "usg".into();
        narrowed.refilter();
        assert!(
            narrowed
                .visible
                .iter()
                .any(|i| narrowed.items[*i].label == "/usage")
        );
    }
    {
        let mut c = setup_with_worker();
        c.handle_key(ctrl('k'));
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "/help".into();
        palette.refilter();
        let effects = c.handle_palette(palette, KeyAction::Send);
        assert!(effects.is_empty());
        assert!(matches!(c.overlay(), Some(Overlay::Help)));
        c.handle_key(esc());

        // jump to the worker session
        c.handle_key(ctrl('k'));
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "db".into();
        palette.refilter();
        c.handle_palette(palette, KeyAction::Send);
        assert_eq!(c.selected(), 1, "the jump landed on the worker row");
    }
    {
        // a jump forgets the search and the pinned scroll of the session it
        // left, as every other selection move does
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        c.scroll = Some(3);
        c.search = Some(SearchState {
            query: "db".into(),
            matches: vec![1, 2],
            current: Some(0),
        });
        c.run_palette_action(PaletteAction::JumpTo(0));
        assert_eq!(c.selected(), 0);
        assert!(c.search.is_none() && c.scroll.is_none());
        assert_eq!(
            c.prefs.last_session.as_deref(),
            Some("orchestrator"),
            "the jump is remembered"
        );
    }
    {
        let mut c = setup_with_worker();
        c.handle_key(ctrl('k'));
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "/thinking".into();
        palette.refilter();
        c.handle_palette(palette, KeyAction::Send);
        assert_eq!(c.composer().input, "/thinking ");
    }
    {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.available_models = vec![WorkerModel {
            provider: "anthropic".into(),
            id: "claude-opus-5".into(),
            name: Some("Opus".into()),
            thinking_levels: Vec::new(),
            context_window: None,
            cost: None,
        }];
        c.set_runs(vec![entry]);
        select_row(&mut c, 1);
        fleet_key(&mut c, 'm');
        let Overlay::Palette(palette) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(palette.scope, PaletteScope::Models);
        assert!(
            palette
                .items
                .iter()
                .all(|i| matches!(i.action, PaletteAction::Model { .. }))
        );
        // choosing one switches the worker's model
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "opus".into();
        palette.refilter();
        let effects = c.handle_palette(palette, KeyAction::Send);
        assert_eq!(
            effects,
            vec![Effect::WorkerModel {
                run_id: "db-20260829120000".to_string(),
                model_id: "claude-opus-5".to_string(),
                provider: Some("anthropic".to_string()),
            }]
        );
    }
    {
        let mut c = setup_with_worker();
        fleet_key(&mut c, 'm');
        let Overlay::Palette(mut palette) = c.overlay().unwrap().clone() else {
            panic!();
        };
        palette.query = "fable".into();
        palette.refilter();
        let effects = c.handle_palette(palette, KeyAction::Send);
        assert_eq!(
            effects,
            vec![Effect::SetOrchestratorModel("fable".to_string())]
        );
    }
}

// -- normal-mode session actions ------------------------------------------

#[test]
fn fleet_letters() {
    {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.pending_question = Some(PendingQuestion {
            id: "q_1".into(),
            question: "bcrypt or argon2?".into(),
            options: None,
            context: None,
            asked_at: crate::util::now_iso(),
        });
        c.set_runs(vec![entry]);
        select_row(&mut c, 1);
        fleet_key(&mut c, 'a');
        let answering = c.composer().answering.as_ref().unwrap();
        assert_eq!(answering.question_id, "q_1");
        type_text(&mut c, "use");
        let effects = c.handle_key(enter());
        assert_eq!(
            effects,
            vec![Effect::WorkerAnswer {
                run_id: "db-20260829120000".to_string(),
                question_id: Some("q_1".to_string()),
                message: "use".to_string(),
            }]
        );
        assert!(c.composer().answering.is_none(), "the answer is consumed");
    }
    {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.pending_dialog = Some(PendingDialog {
            id: "u-1".into(),
            method: "select".into(),
            question: "Pick one".into(),
            options: Some(vec!["yes".into(), "no".into()]),
            context: None,
            asked_at: crate::util::now_iso(),
        });
        c.set_runs(vec![entry]);
        select_row(&mut c, 1);
        fleet_key(&mut c, 'a');
        // a select dialog is prefilled with its first option
        assert_eq!(c.composer().input, "yes");
        let answering = c.composer().answering.as_ref().unwrap();
        assert_eq!(answering.question_id, "u-1");
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::WorkerAnswer { message, .. } if message == "yes"
        ));
    }
    {
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        fleet_key(&mut c, 'a');
        assert!(c.flash().unwrap().text.contains("no pending question"));
    }
    {
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        let effects = fleet_key(&mut c, 's');
        assert_eq!(
            effects,
            vec![Effect::WorkerAbort {
                run_id: "db-20260829120000".to_string(),
            }]
        );
        // the orchestrator: interrupt only when a turn is active
        select_row(&mut c, 0);
        let effects = fleet_key(&mut c, 's');
        assert!(
            effects.is_empty(),
            "an idle orchestrator has nothing to stop"
        );
        c.orch_transcript.push_sent("hello");
        let effects = fleet_key(&mut c, 's');
        assert_eq!(effects, vec![Effect::Interrupt]);
        // esc on an empty composer stops the turn too
        let effects = c.handle_key(esc());
        assert_eq!(effects, vec![Effect::Interrupt]);
    }
    {
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        fleet_key(&mut c, 'x');
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(confirm.message.contains("Abort it and remove"));
        // n cancels; nothing was removed
        c.handle_key(ch('n'));
        assert!(c.overlay().is_none());
        // enter is not an answer to a prompt that destroys work
        fleet_key(&mut c, 'x');
        assert!(c.handle_key(enter()).is_empty());
        assert!(matches!(c.overlay(), Some(Overlay::Confirm(_))));
        // y removes with force (the worker is running)
        let effects = c.handle_key(ch('y'));
        assert_eq!(
            effects,
            vec![Effect::RemoveWorker {
                run_id: "db-20260829120000".to_string(),
                force: true,
            }]
        );
    }
    {
        let mut c = setup_with_worker();
        // the orchestrator cycles claude's effort
        let effects = fleet_key(&mut c, 't');
        assert_eq!(effects, vec![Effect::SetEffort("low".to_string())]);
        assert_eq!(c.effort(), Some("low"), "optimistic until state confirms");
        // the worker cycles pi's from the level it reports
        select_row(&mut c, 1);
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.thinking_level = Some("high".into());
        c.set_runs(vec![entry]);
        let effects = fleet_key(&mut c, 't');
        assert_eq!(
            effects,
            vec![Effect::WorkerThinking {
                run_id: "db-20260829120000".to_string(),
                level: "xhigh".to_string(),
            }]
        );
        // and the next press advances from the optimistically written level,
        // even though the monitor has not written it back into run.json yet
        let effects = fleet_key(&mut c, 't');
        assert!(matches!(
            &effects[0],
            Effect::WorkerThinking { level, .. } if level == "max"
        ));
    }
    {
        let mut c = test_console();
        assert!(c.mouse_captured(), "the console owns the wheel by default");

        let effects = c.handle_key(ctrl('y'));
        assert_eq!(effects, vec![Effect::SetMouseCapture(false)]);
        assert!(!c.mouse_captured());
        assert!(
            c.flash().is_some_and(|f| f.text.contains("select")),
            "it says what just happened"
        );

        let effects = c.handle_key(ctrl('y'));
        assert_eq!(effects, vec![Effect::SetMouseCapture(true)]);
        assert!(c.mouse_captured());

        // and the command reaches the same place, for the palette
        c.composer.input = "/mouse".to_string();
        c.composer.cursor = 6;
        let effects = c.handle_key(enter());
        assert_eq!(effects, vec![Effect::SetMouseCapture(false)]);
        assert!(!c.mouse_captured());
    }
    {
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.thinking_level = Some("high".into());
        c.set_runs(vec![entry]);

        // the monitor never writes the applied level back into run.json: the
        // polled state still says "high", yet the press advances anyway
        let first = fleet_key(&mut c, 't');
        assert!(matches!(
            &first[0],
            Effect::WorkerThinking { level, .. } if level == "xhigh"
        ));
        let second = fleet_key(&mut c, 't');
        assert!(
            matches!(
                &second[0],
                Effect::WorkerThinking { level, .. } if level == "max"
            ),
            "the second press must advance from the optimistic level: {second:?}"
        );

        // a re-poll with the stale state folds the optimistic level into the
        // view, so the statusline shows it; a poll that catches up forgets it
        let mut stale = running_run("db-20260829120000", "db");
        stale.state.thinking_level = Some("high".into());
        c.set_runs(vec![stale]);
        assert_eq!(
            c.runs[0].state.thinking_level.as_deref(),
            Some("max"),
            "the statusline reads the optimistic level until the state catches up"
        );
        let mut caught_up = running_run("db-20260829120000", "db");
        caught_up.state.thinking_level = Some("max".into());
        c.set_runs(vec![caught_up]);
        assert_eq!(
            c.pending_thinking.len(),
            0,
            "the monitor owns the level now"
        );
        let next = fleet_key(&mut c, 't');
        assert!(
            matches!(
                &next[0],
                Effect::WorkerThinking { level, .. } if level == "off"
            ),
            "max wraps to off: {next:?}"
        );
    }
    {
        // the orchestrator's pending effort survives a stale poll and is
        // cleared only when the polled state confirms the change
        let mut c = setup_with_worker();
        let effects = fleet_key(&mut c, 't');
        assert_eq!(effects, vec![Effect::SetEffort("low".to_string())]);
        c.set_orchestrator_state(OrchestratorState::default());
        assert_eq!(
            c.effort(),
            Some("low"),
            "a stale poll must not erase the pending change"
        );
        let confirmed = OrchestratorState {
            effort: Some("low".into()),
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(confirmed);
        assert_eq!(c.effort(), Some("low"));
        assert!(c.pending_effort.is_none(), "the monitor owns the level now");
    }
    {
        let mut c = setup_with_worker();
        let effects = fleet_key(&mut c, 'p');
        assert_eq!(effects, vec![Effect::SetPermissionMode("auto".to_string())]);
        // and refuses on a worker
        select_row(&mut c, 1);
        fleet_key(&mut c, 'p');
        assert!(c.flash().unwrap().text.contains("orchestrator-only"));
    }
    {
        let mut c = setup_with_worker();
        // the orchestrator's brief is the rendered prompt; none on a fresh
        // fleet, so the popup says so dimmed instead of erroring
        fleet_key(&mut c, 'b');
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!("expected the brief overlay");
        };
        assert!(state.placeholder, "no prompt.md yet on a fresh fleet");
        c.handle_key(esc());
        assert!(c.overlay().is_none(), "esc closes the brief");

        // a worker's brief is its taskBrief
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.task_brief = "Build the auth module.\n\nDo not touch tests.".into();
        c.set_runs(vec![entry]);
        select_row(&mut c, 1);
        fleet_key(&mut c, 'b');
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(
            state.text, "Build the auth module.\n\nDo not touch tests.",
            "the full brief, not the one-line transcript summary"
        );
        assert!(!state.placeholder);

        // the wheel routes here as the scroll actions, half a viewport each
        let offset = |c: &Console| {
            let Overlay::Brief(state) = c.overlay().unwrap() else {
                panic!();
            };
            state.offset
        };
        c.handle_action(KeyAction::ScrollHalfDown);
        c.handle_action(KeyAction::ScrollHalfDown);
        assert_eq!(offset(&c), 20, "two notches of the 20-row viewport");
        c.handle_action(KeyAction::ScrollHalfUp);
        assert_eq!(offset(&c), 10);
        c.handle_action(KeyAction::Escape);
        assert!(c.overlay().is_none());

        // with a rendered prompt on disk, the orchestrator shows it
        std::fs::create_dir_all(c.fleet.orchestrator_dir(&c.orch_key)).unwrap();
        std::fs::write(
            c.fleet.orchestrator_prompt(&c.orch_key),
            "You are the orchestrator.",
        )
        .unwrap();
        select_row(&mut c, 0);
        fleet_key(&mut c, 'b');
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(state.text, "You are the orchestrator.");
        assert!(!state.placeholder);
        c.handle_action(KeyAction::Escape);

        // a worker whose record is gone (or brief empty) reads as a placeholder
        let mut empty = running_run("db-20260829120000", "db");
        empty.state.task_brief = String::new();
        c.set_runs(vec![empty]);
        select_row(&mut c, 1);
        let effects = fleet_key(&mut c, 'b');
        assert!(effects.is_empty());
        let Overlay::Brief(state) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(state.placeholder, "an empty brief is not a brief");
    }
    {
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        for _ in 0..3 {
            assert!(
                c.handle_key(esc()).is_empty(),
                "reflexive esc presses must not escalate to SIGKILL"
            );
        }
        assert!(c.flash().is_some_and(|f| f.text.contains("/stop")));
    }
}

#[test]
fn quit_and_shutdown() {
    {
        let mut c = setup_with_worker();
        assert_eq!(c.submit("/quit"), vec![Effect::Quit]);
        let effects = c.submit("/shutdown");
        assert!(effects.is_empty(), "shutdown waits for confirmation");
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(
            confirm
                .message
                .contains("Stop the orchestrator and 1 running worker")
        );
        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::WorkerAbort {
            run_id: "db-20260829120000".to_string(),
        }));
        assert!(effects.contains(&Effect::StopOrchestrator));
        assert!(effects.contains(&Effect::Quit));
        // n cancels
        c.submit("/shutdown");
        let effects = c.handle_key(ch('n'));
        assert!(effects.is_empty());
        assert!(c.overlay().is_none());
    }
    {
        let mut c = setup_with_worker();
        let effects = c.submit("/shutdown");
        assert!(effects.is_empty(), "waits for confirmation");
        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::Quit));
    }
    {
        let mut c = setup_with_worker();
        c.handle_key(ch('j'));
        let effects = c.submit("/quit");
        assert_eq!(effects, vec![Effect::Quit]);
        let effects = c.submit("/help");
        assert!(effects.is_empty());
        assert!(matches!(c.overlay(), Some(Overlay::Help)));
    }
    {
        let mut c = test_console();
        let mut mine = OrchestratorSession::new("/repo");
        mine.alias = Some("mine".into());
        c.orch_key = mine.key();
        write_sessions(&c, vec![mine.clone()]);
        write_run(&c, "db-20260829120000", "db", mine.uuid);
        let other = crate::orch::session::create_session(c.fleet.root(), Some("docs")).unwrap();
        write_run(&c, "docs-20260829120001", "docs", other.uuid);
        c.set_runs(vec![running_run("db-20260829120000", "db")]);

        let effects = c.submit("/shutdown docs");
        assert!(effects.is_empty(), "waits for confirmation");
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!("expected a confirm");
        };
        assert!(confirm.message.contains("docs"), "{}", confirm.message);
        assert_eq!(confirm.action, ConfirmAction::ShutdownSession(other.key()));

        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::WorkerAbort {
            run_id: "docs-20260829120001".into(),
        }));
        assert!(effects.contains(&Effect::StopSession(other.key())));
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                Effect::WorkerAbort { run_id, .. } if run_id == "db-20260829120000"
            )),
            "the active session's workers are untouched: {effects:?}"
        );
        assert!(!effects.contains(&Effect::Quit), "the console stays open");
    }
    {
        let mut c = test_console();
        let active = crate::orch::session::create_session(c.fleet.root(), None).unwrap();
        c.orch_key = active.key();
        let _effects = c.submit(&format!("/shutdown {}", active.uuid));
        let Overlay::Confirm(confirm) = c.overlay().unwrap() else {
            panic!("expected a confirm");
        };
        assert_eq!(confirm.action, ConfirmAction::Shutdown);
        let effects = c.handle_key(ch('y'));
        assert!(effects.contains(&Effect::StopOrchestrator));
        assert!(effects.contains(&Effect::Quit));
    }
    {
        let mut c = test_console();
        let effects = c.submit("/shutdown nobody");
        assert!(effects.is_empty());
        assert!(c.overlay().is_none());
        assert!(
            c.flash().unwrap().text.contains("no orchestrator session"),
            "{}",
            c.flash().unwrap().text
        );
    }
}

#[test]
fn scroll_and_search() {
    {
        let mut c = setup_with_worker();
        for i in 0..40 {
            c.orch_transcript.push_notice(&format!("line {i}"));
        }
        assert_eq!(c.scroll(), None, "the tail is followed by default");
        c.handle_key(ctrl_code(KeyCode::Home));
        assert_eq!(c.scroll(), Some(0), "ctrl-home pins the top");
        c.handle_key(key(KeyCode::PageDown));
        assert_eq!(c.scroll(), Some(20), "one 20-row page");
        c.handle_key(key(KeyCode::PageUp));
        assert_eq!(c.scroll(), Some(0));
        // the wheel is half a page
        c.handle_action(KeyAction::ScrollHalfDown);
        assert_eq!(c.scroll(), Some(10));
        c.handle_key(ctrl_code(KeyCode::End));
        assert_eq!(c.scroll(), None, "ctrl-end follows the tail again");
        // and none of those keys are text
        assert!(c.composer().input.is_empty());
    }
    {
        let mut c = setup_with_worker();
        let notice = |c: &mut Console, text: String| {
            c.ingest_orchestrator_record(
                &crate::orch::records::OrchestratorEvent::Notice { text, error: None }.to_record(),
            );
        };
        // a transcript already at its cap, so the next records push blocks off
        for i in 0..600 {
            notice(&mut c, format!("line {i}"));
        }
        c.handle_key(ctrl_code(KeyCode::Home));
        // far enough in that the trim below does not take the pinned row with it
        for _ in 0..8 {
            c.handle_key(key(KeyCode::PageDown));
        }
        let pinned = c.scroll().expect("pinned somewhere");
        let content = c.orch_transcript.blocks()[pinned].text.clone();

        for i in 0..50 {
            notice(&mut c, format!("later {i}"));
        }
        let now = c.scroll().expect("still pinned");
        assert_eq!(
            c.orch_transcript.blocks()[now].text,
            content,
            "the pinned row still shows what it was pinned to"
        );
        assert_eq!(now, pinned - 50, "rebased by exactly what was dropped");
    }
    {
        let mut c = setup_with_worker();
        c.orch_transcript.push_notice("the quick brown fox");
        c.orch_transcript.push_notice("lazy dog");
        c.orch_transcript.push_notice("another quick fox");
        c.handle_key(ctrl('r'));
        let Overlay::Search(search) = c.overlay().unwrap() else {
            panic!();
        };
        assert!(search.matches.is_empty());
        c.handle_key(ch('q'));
        c.handle_key(ch('u'));
        let Overlay::Search(search) = c.overlay().unwrap() else {
            panic!();
        };
        assert_eq!(search.matches.len(), 2, "live matches as you type");
        c.handle_key(enter());
        assert!(c.overlay().is_none());
        assert_eq!(c.search().unwrap().current, Some(0));
        // the same key steps once a search is running, rather than reopening
        c.handle_key(ctrl('r'));
        assert_eq!(c.search().unwrap().current, Some(1));
        c.handle_key(ctrl('r'));
        assert_eq!(c.search().unwrap().current, Some(0), "wraps");
        // and the view is pinned at the match
        assert!(c.scroll().is_some());
    }
    {
        let mut c = setup_with_worker();
        c.orch_transcript.push_notice("the quick brown fox");
        // cancelled
        c.handle_key(ctrl('r'));
        c.handle_key(esc());
        assert!(c.search().is_none());
        c.handle_key(ctrl('r'));
        assert!(matches!(c.overlay(), Some(Overlay::Search(_))), "reopens");
        // a typo that matched nothing
        type_text(&mut c, "zzz");
        c.handle_key(enter());
        assert!(c.search().is_none(), "nothing to step through is not kept");
        c.handle_key(ctrl('r'));
        assert!(matches!(c.overlay(), Some(Overlay::Search(_))), "reopens");
    }
    {
        let mut c = setup_with_worker();
        c.orch_transcript.push_notice("the quick brown fox");
        c.orch_transcript.push_sent("hello"); // a turn is running
        c.handle_key(ctrl('r'));
        type_text(&mut c, "quick");
        c.handle_key(enter());
        assert!(c.search().is_some());
        assert!(c.handle_key(esc()).is_empty(), "first the highlight goes");
        assert!(c.search().is_none());
        assert_eq!(c.handle_key(esc()), vec![Effect::Interrupt]);
    }
    {
        let mut c = setup_with_worker();
        select_row(&mut c, 1);
        c.scroll = Some(3);
        c.reset_orchestrator_transcript();
        assert_eq!(c.scroll(), Some(3));
    }
}

#[test]
fn permission_prompts() {
    {
        let mut c = setup_with_worker();
        let request = PermissionRequest {
            request_id: "req_1".into(),
            request: CanUseToolRequest {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "touch a.txt"}),
                tool_use_id: "t1".into(),
                title: Some("Run touch a.txt".into()),
                permission_suggestions: vec![serde_json::json!({"type": "addRules"})],
                ..CanUseToolRequest::default()
            },
            received_at: crate::util::now_iso(),
        };
        let state = OrchestratorState {
            pending_requests: vec![request],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        // the prompt blocks the orchestrator, so it raises itself
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        read_the_prompt(&mut c);
        let effects = c.handle_key(ch('y'));
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Allow { .. },
                ..
            }
        ));
        c.last_key_at = 0;
        // deny with a reason
        let state = OrchestratorState {
            pending_requests: vec![PermissionRequest {
                request_id: "req_2".into(),
                request: CanUseToolRequest {
                    tool_name: "Bash".into(),
                    ..CanUseToolRequest::default()
                },
                received_at: crate::util::now_iso(),
            }],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        read_the_prompt(&mut c);
        c.handle_key(ch('n')); // start the deny reason
        c.handle_key(ch('n'));
        c.handle_key(ch('o'));
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Deny { message },
                ..
            } if message == "no"
        ));
    }
    {
        let mut c = setup_with_worker();
        type_text(&mut c, "can you also");
        c.set_orchestrator_state(bash_request("req_9"));
        assert!(
            c.overlay().is_none(),
            "not while the composer holds text: the next `a` would allow-always"
        );
        // the letters keep going to the composer
        type_text(&mut c, " a");
        assert_eq!(c.composer().input, "can you also a");

        // once the line is sent and the keyboard has gone quiet, it rises
        c.handle_key(enter());
        c.last_key_at = 0;
        c.set_orchestrator_state(bash_request("req_9"));
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
    }
    {
        let mut c = setup_with_worker();
        c.set_orchestrator_state(bash_request("req_10"));
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        // inside the grace window: dropped, and the prompt stays up unanswered
        let effects = c.handle_key(ch('a'));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        // after it, the prompt answers as ever
        read_the_prompt(&mut c);
        let effects = c.handle_key(ch('y'));
        assert!(matches!(&effects[0], Effect::ResolvePermission { .. }));
    }
    {
        // two queued prompts: answering the first shows the second; the
        // second counts as raised, so `esc` does not raise it again, and it
        // gets a fresh grace window of its own
        let mut c = setup_with_worker();
        let request = |id: &str| PermissionRequest {
            request_id: id.into(),
            request: CanUseToolRequest {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "git push --force"}),
                tool_use_id: "t1".into(),
                ..CanUseToolRequest::default()
            },
            received_at: crate::util::now_iso(),
        };
        let state = OrchestratorState {
            pending_requests: vec![request("req_11"), request("req_12")],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        read_the_prompt(&mut c);
        let effects = c.handle_key(ch('y'));
        assert!(matches!(&effects[0], Effect::ResolvePermission { .. }));
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        // a key this soon after the advance was meant for the composer
        let effects = c.handle_key(ch('a'));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        // `esc` dismisses the second prompt; a poll must not reopen it
        c.handle_key(esc());
        assert!(c.overlay().is_none());
        c.last_key_at = 0;
        c.raise_waiting();
        assert!(c.overlay().is_none(), "dismissed is dismissed");
    }
    {
        let mut c = setup_with_worker();
        let make_request = |id: &str, options: Value| PermissionRequest {
            request_id: id.into(),
            request: CanUseToolRequest {
                tool_name: "AskUserQuestion".into(),
                input: serde_json::json!({"questions": [
                    {"question": "Which hash?", "options": options},
                ]}),
                tool_use_id: "t2".into(),
                ..CanUseToolRequest::default()
            },
            received_at: crate::util::now_iso(),
        };
        let state = OrchestratorState {
            pending_requests: vec![make_request(
                "req_3",
                serde_json::json!([{"label": "bcrypt"}, {"label": "argon2"}]),
            )],
            ..OrchestratorState::default()
        };
        c.set_orchestrator_state(state);
        // a blocked orchestrator raises its own prompt
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        read_the_prompt(&mut c);
        // the first option is highlighted; down moves, enter answers
        c.handle_key(key(KeyCode::Down));
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Answer { answers },
                ..
            } if answers["Which hash?"] == "argon2"
        ));
        // and a custom answer
        let state = OrchestratorState {
            pending_requests: vec![make_request(
                "req_4",
                serde_json::json!([{"label": "bcrypt"}]),
            )],
            ..OrchestratorState::default()
        };
        c.last_key_at = 0; // the keyboard has been quiet since the last answer
        c.set_orchestrator_state(state);
        read_the_prompt(&mut c);
        c.handle_key(key(KeyCode::Down)); // onto "something else"
        c.handle_key(enter()); // start typing
        c.handle_key(ch('s'));
        c.handle_key(ch('c'));
        let effects = c.handle_key(enter());
        assert!(matches!(
            &effects[0],
            Effect::ResolvePermission {
                decision: PermissionDecisionRecord::Answer { answers },
                ..
            } if answers["Which hash?"] == "sc"
        ));
    }
}

fn bash_request(id: &str) -> OrchestratorState {
    OrchestratorState {
        pending_requests: vec![PermissionRequest {
            request_id: id.into(),
            request: CanUseToolRequest {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "git push --force"}),
                tool_use_id: "t1".into(),
                permission_suggestions: vec![serde_json::json!({"type": "addRules"})],
                ..CanUseToolRequest::default()
            },
            received_at: crate::util::now_iso(),
        }],
        ..OrchestratorState::default()
    }
}

const KEY: &str = "ts_live_0123456789abcdef";

fn open_routing(c: &mut Console, enabled: bool, key: KeyState) {
    assert_eq!(c.submit("/routing"), vec![Effect::LoadRoutingStatus]);
    // what the runtime would hand back after reading the store and the config
    if let Some(Overlay::Routing(panel)) = &mut c.overlay {
        panel.status = Some(RoutingStatus {
            enabled,
            key,
            candidates: Ok(3),
            threshold: 0.6,
            models: vec!["deepseek-v4-flash".into(), "gone:retired-model".into()],
        });
    }
}

#[tokio::test]
async fn routing_key_entry() {
    {
        let mut c = setup_with_worker();
        open_routing(&mut c, false, KeyState::None);
        c.handle_key(ch('s'));
        // every letter is part of the key while it is being entered — even the
        // ones the panel would otherwise read as commands
        type_text(&mut c, KEY);
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("still on the panel");
        };
        assert_eq!(panel.entering.as_ref().unwrap().len(), KEY.len());
        let effects = c.handle_key(enter());
        assert_eq!(effects, vec![Effect::SaveTypesafeKey(Secret::new(KEY))]);
        let printed = format!("{effects:?}");
        assert!(
            !printed.contains("0123456789"),
            "not even a debug print of the effect holds the key: {printed}"
        );
        assert!(
            c.composer().input.is_empty(),
            "the key never touched the composer"
        );
        assert!(
            !c.orch_transcript
                .blocks()
                .iter()
                .any(|b| b.text.contains("ts_live")),
            "nor the transcript"
        );
    }
    {
        let mut c = setup_with_worker();
        open_routing(&mut c, true, KeyState::None);
        c.handle_key(ch('s'));
        c.paste(&format!("{KEY}\n"));
        assert_eq!(
            c.handle_key(enter()),
            vec![Effect::SaveTypesafeKey(Secret::new(KEY))],
            "the paste's trailing newline is not part of the key"
        );
        c.handle_key(ch('s'));
        type_text(&mut c, "half");
        assert!(
            c.handle_key(esc()).is_empty(),
            "esc cancels, saving nothing"
        );
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("esc leaves the panel open, the entry gone");
        };
        assert!(panel.entering.is_none());
    }
    {
        let mut c = setup_with_worker();
        open_routing(&mut c, false, KeyState::None);
        assert_eq!(c.handle_key(ch('r')), vec![Effect::SetRouting(true)]);
        // nothing stored: `d` does nothing
        c.handle_key(ch('d'));
        assert!(c.handle_key(ch('y')).is_empty());

        open_routing(
            &mut c,
            true,
            KeyState::Config {
                masked: "••••cdef".into(),
                path: "~/.pilotfish/config.toml".into(),
            },
        );
        assert_eq!(c.handle_key(ch('r')), vec![Effect::SetRouting(false)]);
        c.handle_key(ch('d'));
        assert_eq!(c.handle_key(ch('y')), vec![Effect::DeleteTypesafeKey]);
        // and any other key keeps it
        c.handle_key(ch('d'));
        assert!(c.handle_key(ch('n')).is_empty());
    }
    {
        let mut c = console_with_user_config("[routing]\nenabled = true\n");
        c.ask_for_missing_key();
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("the routing panel, asking for the key");
        };
        assert!(panel.entering.is_some(), "straight into pasting it");
        assert_eq!(panel.status.as_ref().unwrap().key, KeyState::None);

        // with a key in the config, or routing off, nothing is asked
        let mut c = console_with_user_config(
            "[routing]\nenabled = true\napi_key = \"ts_live_0123456789abcdef\"\n",
        );
        c.ask_for_missing_key();
        assert!(c.overlay().is_none());
        let mut c = console_with_user_config("[routing]\nenabled = false\n");
        c.ask_for_missing_key();
        assert!(c.overlay().is_none());
    }
    {
        let mut c = console_with_user_config("# mine\n[routing]\nenabled = true\n");
        assert_eq!(c.submit("/routing"), vec![Effect::LoadRoutingStatus]);
        c.execute_all(vec![Effect::LoadRoutingStatus]).await;
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("the panel");
        };
        assert!(
            panel.entering.is_some(),
            "no key yet, so /routing asks for one"
        );

        c.paste(KEY);
        let effects = c.handle_key(enter());
        assert_eq!(effects, vec![Effect::SaveTypesafeKey(Secret::new(KEY))]);
        c.execute_all(effects).await;
        let path = c.user_dir.clone().unwrap().join("config.toml");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(KEY) && raw.contains("# mine"), "{raw}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("still the panel");
        };
        assert!(
            matches!(&panel.status.as_ref().unwrap().key, KeyState::Config { masked, .. } if masked == "••••cdef"),
            "{:?}",
            panel.status
        );
        assert!(
            !c.flash().unwrap().text.contains("0123456789"),
            "the toast names the file, never the key"
        );

        c.handle_key(ch('d'));
        let effects = c.handle_key(ch('y'));
        assert_eq!(effects, vec![Effect::DeleteTypesafeKey]);
        c.execute_all(effects).await;
        assert!(!std::fs::read_to_string(&path).unwrap().contains("api_key"));
    }
    {
        let mut c = console_with_user_config("[routing]\nenabled = false\n");
        c.submit("/routing");
        c.execute_all(vec![Effect::LoadRoutingStatus]).await;
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("the panel");
        };
        assert!(panel.entering.is_some(), "no key: /routing asks for it");
        // esc declines, and the panel stays
        c.handle_key(esc());
        let effects = c.handle_key(ch('r'));
        assert_eq!(effects, vec![Effect::SetRouting(true)]);
        c.execute_all(effects).await;
        // switching routing on with no key asks again
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("the panel");
        };
        assert!(panel.entering.is_some());
    }
}

// -- orchestrator settings -------------------------------------------------

#[test]
fn slash_commands() {
    {
        let mut c = setup_with_worker();
        let effects = c.submit("/thinking nonsense");
        assert!(c.flash().unwrap().text.contains("usage: /thinking"));
        assert!(effects.is_empty());
        let effects = c.submit("/thinking high");
        assert_eq!(effects, vec![Effect::SetEffort("high".to_string())]);
        // a settings change is a passing note, not a message
        assert!(
            !c.orchestrator_transcript()
                .blocks()
                .iter()
                .any(|b| b.kind == crate::tui::transcript::BlockKind::User)
        );
    }
    {
        let mut c = setup_with_worker();
        let effects = c.submit("/model fable");
        assert_eq!(
            effects,
            vec![Effect::SetOrchestratorModel("fable".to_string())]
        );
        select_row(&mut c, 1);
        let effects = c.submit("/model claude-opus-5");
        assert_eq!(
            effects,
            vec![Effect::WorkerModel {
                run_id: "db-20260829120000".to_string(),
                model_id: "claude-opus-5".to_string(),
                provider: None,
            }]
        );
    }
    {
        let mut c = setup_with_worker();
        c.submit("/permissions");
        assert!(c.flash().unwrap().text.contains("permissions: default"));
        c.submit("/permissions bypassPermissions");
        assert!(c.flash().unwrap().text.contains("not offered"));
        let effects = c.submit("/permissions auto");
        assert_eq!(effects, vec![Effect::SetPermissionMode("auto".to_string())]);
    }
    {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.pending_question = Some(PendingQuestion {
            id: "q_1".into(),
            question: "which?".into(),
            options: None,
            context: None,
            asked_at: crate::util::now_iso(),
        });
        c.set_runs(vec![entry]);
        select_row(&mut c, 1);
        let effects = c.submit("/answer use argon2");
        assert_eq!(
            effects,
            vec![Effect::WorkerAnswer {
                run_id: "db-20260829120000".to_string(),
                question_id: Some("q_1".to_string()),
                message: "use argon2".to_string(),
            }]
        );
    }
    {
        let mut c = setup_with_worker();
        let mut entry = running_run("db-20260829120000", "db");
        entry.state.status = RunStatus::Settled;
        entry.state.pid = None;
        c.set_runs(vec![entry]);
        select_row(&mut c, 1);
        c.submit("hello");
        assert!(c.flash().unwrap().text.contains("is settled"));
    }
    {
        let pending = PendingQuestion {
            id: "q_9".into(),
            question: String::new(),
            options: None,
            context: None,
            asked_at: String::new(),
        };
        let (id, message) = parse_answer("q_3 use argon2", Some(&pending));
        assert_eq!(id.as_deref(), Some("q_3"));
        assert_eq!(message, "use argon2");
        let (id, message) = parse_answer("use argon2", Some(&pending));
        assert_eq!(id.as_deref(), Some("q_9"), "the pending id fills in");
        assert_eq!(message, "use argon2");
        let (id, message) = parse_answer("q_3", Some(&pending));
        assert_eq!(id.as_deref(), Some("q_9"), "an id alone is not an answer");
        assert_eq!(message, "q_3");
        let (id, _) = parse_answer("u-1 yes", None);
        assert_eq!(id.as_deref(), Some("u-1"), "dialog ids parse too");
    }
}

// -- feeds ------------------------------------------------------------------

// -- sessions -----------------------------------------------------------

/// Persist sessions into the console's fleet, the way the orchestrator
/// slice's store does.
fn write_sessions(c: &Console, sessions: Vec<OrchestratorSession>) {
    let mut store = crate::orch::session::FleetSessions::new();
    for session in sessions {
        store.upsert(session);
    }
    crate::orch::session::save(c.fleet.root(), &mut store).unwrap();
}

/// Persist a running run owned by `owner` into the console's fleet.
fn write_run(c: &Console, run_id: &str, name: &str, owner: Uuid) {
    let mut entry = running_run(run_id, name);
    entry.state.orchestrator_id = Some(owner);
    let dir = c.fleet.run_dir(run_id);
    std::fs::create_dir_all(&dir).unwrap();
    crate::fleet::run::save_state(&dir, &entry.state).unwrap();
}

#[test]
fn session_state() {
    {
        let mut c = test_console();
        let key = SessionKey::new(Some("docs".into()), Uuid::new_v4());
        c.orch_key = key.clone();
        c.set_runs(vec![running_run("auth-20260830000000", "auth")]);
        assert_eq!(
            c.rows()[0].target,
            SessionTarget::Orchestrator(key.uuid),
            "the orchestrator row names its session"
        );
        assert_eq!(c.rows()[0].name, "orchestrator · docs");
        assert_eq!(c.selected_target(), SessionTarget::Orchestrator(key.uuid));
        assert_eq!(c.selected_target().key(), "orchestrator");

        // an alias-less session says what it is rather than showing a hex
        // prefix nobody can read, and the composer prompt follows
        c.orch_key = SessionKey::new(None, Uuid::new_v4());
        c.set_runs(Vec::new());
        assert_eq!(c.rows()[0].name, "orchestrator");
        assert_eq!(c.composer_prompt(), "orchestrator > ");
        // and so does the legacy default session
        c.orch_key = SessionKey::default();
        c.set_runs(Vec::new());
        assert_eq!(c.rows()[0].name, "orchestrator");
        assert_eq!(c.composer_prompt(), "orchestrator > ");
    }
    {
        let mut c = test_console();
        // a session row, as the monitor owns it
        let session = crate::orch::session::create_session(c.fleet.root(), Some("alpha")).unwrap();
        c.prefs.last_session = Some("orchestrator".into());
        c.prefs.last_session_uuid = Some(session.uuid.to_string());

        // order 1: the store writes a heartbeat, then the console writes prefs
        crate::orch::session::touch_heartbeat(c.fleet.root(), session.uuid).unwrap();
        c.save_prefs();
        let raw = std::fs::read_to_string(c.fleet.fleet_json()).unwrap();
        assert!(raw.contains("\"lastSession\": \"orchestrator\""), "{raw}");
        let store = crate::orch::session::load(c.fleet.root()).unwrap();
        assert!(
            store.sessions[&session.uuid].last_heartbeat.is_some(),
            "the heartbeat survived the console's write"
        );
        assert!(
            store
                .extra
                .get("console")
                .and_then(|v| v.get("lastSession"))
                .and_then(serde_json::Value::as_str)
                == Some("orchestrator"),
            "the store itself round-trips the console key now"
        );

        // order 2: the console writes prefs, then the store writes a heartbeat
        c.save_prefs();
        crate::orch::session::touch_heartbeat(c.fleet.root(), session.uuid).unwrap();
        let raw = std::fs::read_to_string(c.fleet.fleet_json()).unwrap();
        assert!(raw.contains("\"lastSession\": \"orchestrator\""), "{raw}");
        assert!(
            raw.contains(&session.uuid.to_string()),
            "the session row survived the prefs write: {raw}"
        );
        let store = crate::orch::session::load(c.fleet.root()).unwrap();
        assert_eq!(store.sessions.len(), 1);
        assert!(
            store.sessions[&session.uuid].last_heartbeat.is_some(),
            "the heartbeat survived the second prefs write"
        );
    }
    {
        let c = test_console();
        let uuid = Uuid::new_v4();
        // a pre-split console wrote the session uuid into lastSession
        std::fs::write(
            c.fleet.fleet_json(),
            json!({
                "version": 2,
                "sessions": {},
                "console": {"lastSession": uuid.to_string()},
            })
            .to_string(),
        )
        .unwrap();
        let mut reopened = Console::new(FleetPaths::new(c.fleet.root().to_path_buf()));
        reopened.load_prefs();
        assert_eq!(
            reopened.prefs().last_session_uuid.as_deref(),
            Some(uuid.to_string().as_str()),
            "the legacy uuid lands in the session field"
        );
        assert_eq!(reopened.prefs().last_session, None, "not doubled");

        // a row key is a row key, a uuid a uuid: the fields stay separate
        std::fs::write(
            c.fleet.fleet_json(),
            json!({
                "version": 2,
                "sessions": {},
                "console": {
                    "lastSession": "auth-20260830000000",
                    "lastSessionUuid": uuid.to_string(),
                },
            })
            .to_string(),
        )
        .unwrap();
        let mut reopened = Console::new(FleetPaths::new(c.fleet.root().to_path_buf()));
        reopened.load_prefs();
        assert_eq!(
            reopened.prefs().last_session.as_deref(),
            Some("auth-20260830000000")
        );
        assert_eq!(
            reopened.prefs().last_session_uuid.as_deref(),
            Some(uuid.to_string().as_str())
        );
        // and save_prefs persists both under the console key
        reopened.save_prefs();
        let raw = std::fs::read_to_string(c.fleet.fleet_json()).unwrap();
        assert!(raw.contains("\"lastSessionUuid\": \""), "{raw}");
        assert!(
            raw.contains("\"lastSession\": \"auth-20260830000000\""),
            "{raw}"
        );
    }
    {
        let mut c = setup_with_worker();
        c.orch_transcript.push_sent("hello");
        c.toast("a note", false);
        let other = crate::orch::session::create_session(c.fleet.root(), Some("docs")).unwrap();
        c.begin_session(&other.key());
        assert_eq!(c.orch_key, other.key());
        assert!(
            c.runs.is_empty(),
            "the new session's runs arrive with the feeds"
        );
        assert!(c.rows().is_empty());
        assert!(c.worker_transcripts.is_empty());
        assert!(c.orch_transcript.blocks().is_empty());
        assert_eq!(c.selected(), 0);
        assert!(
            c.flash().is_none(),
            "the old session's notes do not carry over"
        );
    }
}

#[test]
fn session_commands() {
    {
        let mut c = test_console();
        let effects = c.submit("/sessions");
        assert!(effects.is_empty());
        assert!(c.flash().unwrap().text.contains("no sessions yet"));
    }
    {
        use time::{Duration, OffsetDateTime};

        let mut c = test_console();
        let mut running = OrchestratorSession::new("/repo");
        running.alias = Some("add-auth".into());
        running.pid = Some(std::process::id() as i32); // live monitor
        running.last_heartbeat = Some(crate::util::now_iso());
        let mut wedged = OrchestratorSession::new("/repo");
        wedged.alias = Some("stale".into());
        wedged.pid = Some(std::process::id() as i32); // alive, but not stamping
        wedged.last_heartbeat = Some(crate::util::iso_at(
            OffsetDateTime::now_utc() - Duration::minutes(10),
        ));
        let stopped = OrchestratorSession::new("/repo"); // no pid, no heartbeat
        write_sessions(&c, vec![running.clone(), wedged.clone(), stopped.clone()]);
        write_run(&c, "auth-20260830000000", "auth", running.uuid);

        let effects = c.submit("/sessions");
        assert!(effects.is_empty());
        let text = c
            .orchestrator_transcript()
            .blocks()
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("add-auth"), "{text}");
        assert!(
            text.contains(&crate::util::short_uuid(&running.uuid)),
            "{text}"
        );
        assert!(text.contains("1 worker"), "{text}");
        assert!(text.contains("· running"), "{text}");
        assert!(text.contains("stale ("), "{text}");
        assert!(text.contains("· wedged"), "{text}");
        assert!(text.contains("· stopped"), "{text}");
        // the toolbar carries a tally, one line
        let flash = c.flash().unwrap();
        assert!(
            flash
                .text
                .contains("3 sessions · 1 running · 1 wedged · 1 stopped"),
            "{}",
            flash.text
        );
    }
    {
        let mut c = test_console();
        let effects = c.submit("/session new");
        assert_eq!(effects.len(), 2, "{effects:?}");
        assert!(matches!(&effects[0], Effect::SavePrefs));
        let Effect::SwitchSession(key) = &effects[1] else {
            panic!("expected a switch: {effects:?}");
        };
        assert!(
            key.alias.is_none(),
            "no alias: none until the orchestrator derives it"
        );
        assert!(!key.uuid.is_nil());
        let store = crate::orch::session::load(c.fleet.root()).unwrap();
        assert!(
            store.sessions.contains_key(&key.uuid),
            "the row is persisted"
        );
        assert_eq!(
            c.prefs().last_session_uuid.as_deref(),
            Some(key.uuid.to_string().as_str()),
            "the new session becomes the remembered one"
        );

        let effects = c.submit("/session new db-ops");
        let Effect::SwitchSession(key) = &effects[1] else {
            panic!("{effects:?}");
        };
        assert_eq!(key.alias.as_deref(), Some("db-ops"));
    }
    {
        let mut c = test_console();
        let session =
            crate::orch::session::create_session(c.fleet.root(), Some("add-auth")).unwrap();

        // by uuid
        let effects = c.submit(&format!("/session {}", session.uuid));
        assert!(matches!(
            &effects[1],
            Effect::SwitchSession(key) if key.uuid == session.uuid
        ));
        assert_eq!(
            c.prefs().last_session_uuid.as_deref(),
            Some(session.uuid.to_string().as_str())
        );
        // by alias
        let effects = c.submit("/session add-auth");
        assert!(matches!(
            &effects[1],
            Effect::SwitchSession(key) if key.uuid == session.uuid
        ));
        // switching to the session already served is a no-op
        c.orch_key = session.key();
        let effects = c.submit("/session add-auth");
        assert!(effects.is_empty());
        assert!(c.flash().unwrap().text.contains("already on this session"));
        // unknown keys are refused, not guessed
        let effects = c.submit("/session nope");
        assert!(effects.is_empty());
        assert!(
            c.flash().unwrap().text.contains("no orchestrator session"),
            "{}",
            c.flash().unwrap().text
        );
        // bare /session shows the usage
        let effects = c.submit("/session");
        assert!(effects.is_empty());
        assert!(c.flash().unwrap().text.contains("usage: /session"));
    }
    {
        let mut c = test_console();
        let first = crate::orch::session::create_session(c.fleet.root(), Some("dup")).unwrap();
        let second = crate::orch::session::create_session(c.fleet.root(), Some("dup")).unwrap();
        // resolving the alias must not pick silently: the error names both
        let effects = c.submit("/session dup");
        assert!(effects.is_empty());
        let flash = c.flash().unwrap();
        assert!(
            flash.text.contains("several live sessions"),
            "{}",
            flash.text
        );
        assert!(
            flash.text.contains(&crate::util::short_uuid(&first.uuid)),
            "{}",
            flash.text
        );
        assert!(
            flash.text.contains(&crate::util::short_uuid(&second.uuid)),
            "{}",
            flash.text
        );
        // the same honesty applies to /shutdown <key>
        let effects = c.submit("/shutdown dup");
        assert!(effects.is_empty());
        assert!(c.overlay().is_none());
        assert!(
            c.flash().unwrap().text.contains("several live sessions"),
            "{}",
            c.flash().unwrap().text
        );
    }
    {
        let mut c = setup_with_worker();
        let current = c.orch_key.clone();
        select_row(&mut c, 0);
        fleet_key(&mut c, 'x');
        let Some(Overlay::Confirm(state)) = c.overlay() else {
            panic!("asks first");
        };
        assert!(state.message.contains("completely"), "{}", state.message);
        assert!(
            state.message.contains("No session is left open"),
            "{}",
            state.message
        );
        let ConfirmAction::RemoveSession { key, next } = state.action.clone() else {
            panic!("{:?}", state.action);
        };
        assert_eq!(key, current);
        assert_eq!(
            next, None,
            "never a fresh session: only the user starts one"
        );
        assert!(crate::orch::session::list_sessions(c.fleet.root()).is_empty());

        let effects = c.handle_key(ch('y'));
        assert_eq!(
            effects,
            vec![
                Effect::RemoveSession(current),
                Effect::SavePrefs,
                Effect::LeaveSession,
            ]
        );
        assert_eq!(c.prefs().last_session_uuid, None);
    }
    {
        // with no session open, a message has nobody to go to; commands run
        let mut c = test_console();
        c.end_session();
        assert!(!c.has_session() && c.rows().is_empty());
        type_text(&mut c, "fix the flaky test");
        assert!(c.handle_key(enter()).is_empty());
        assert!(c.flash().unwrap().text.contains("/session new"));
        let effects = c.submit("/session new payments");
        let Some(Effect::SwitchSession(key)) = effects.last() else {
            panic!("{effects:?}");
        };
        assert_eq!(key.alias.as_deref(), Some("payments"));
    }
    {
        // a rename keeps the session's directory, found again by its uuid
        let mut c = test_console();
        let session = crate::orch::session::create_session(c.fleet.root(), Some("old")).unwrap();
        c.begin_session(&session.key());
        let dir = c.fleet.orchestrator_dir(&session.key());
        std::fs::create_dir_all(&dir).unwrap();
        crate::orch::session::create_session(c.fleet.root(), Some("taken")).unwrap();
        assert!(c.submit("/session rename taken").is_empty());
        assert!(c.flash().unwrap().error, "{:?}", c.flash());
        assert!(c.submit("/session rename payments refactor").is_empty());
        assert_eq!(c.orch_key.alias.as_deref(), Some("payments refactor"));
        assert_eq!(c.fleet.orchestrator_dir(&c.orch_key), dir);
    }
    {
        let mut c = setup_with_worker();
        let other = crate::orch::session::create_session(c.fleet.root(), Some("old-work")).unwrap();
        assert!(c.submit("/session remove old-work").is_empty());
        let Some(Overlay::Confirm(state)) = c.overlay() else {
            panic!("asks first");
        };
        assert!(
            state.message.contains("no workers left"),
            "{}",
            state.message
        );
        assert_eq!(
            state.action,
            ConfirmAction::RemoveSession {
                key: other.key(),
                next: None,
            },
            "not the session the console is on, so it stays put"
        );
        assert!(c.handle_key(ch('n')).is_empty());
        assert!(c.overlay().is_none());
        assert_eq!(c.flash().unwrap().text, "· the session is kept");
    }
}

fn model_question(id: &str) -> crate::route::ModelQuestion {
    let option = |key: &str, p: f64| crate::route::ModelOption {
        key: key.into(),
        probability: Some(p),
        detail: String::new(),
    };
    crate::route::ModelQuestion {
        id: id.into(),
        name: "add-auth".into(),
        brief: "add token refresh".into(),
        confidence: 0.3,
        threshold: 0.6,
        options: vec![
            option("anthropic:claude-opus-5", 0.4),
            option("opencode-go:deepseek-v4-flash", 0.3),
            option("anthropic:claude-haiku-4-5", 0.1),
        ],
        fallback: Some("claude-sonnet-5".into()),
        asked_at: crate::util::now_iso(),
        pid: std::process::id(),
        deadline_ms: crate::util::now_ms() + 600_000,
    }
}

#[test]
fn model_questions() {
    {
        let mut c = setup_with_worker();
        c.set_model_questions(vec![model_question("q1")]);
        assert!(
            matches!(c.overlay(), Some(Overlay::ModelChoice(s)) if s.id == "q1" && s.selected == 0),
            "the spawn is waiting, so the prompt comes to the human"
        );
        read_the_prompt(&mut c);
        c.handle_key(ch('j'));
        assert_eq!(
            c.handle_key(enter()),
            vec![Effect::AnswerModelQuestion {
                id: "q1".into(),
                model: Some("opencode-go:deepseek-v4-flash".into()),
            }]
        );
        assert!(c.overlay().is_none());
    }
    {
        let mut c = setup_with_worker();
        c.set_model_questions(vec![model_question("q1")]);
        read_the_prompt(&mut c);
        assert_eq!(
            c.handle_key(ch('3')),
            vec![Effect::AnswerModelQuestion {
                id: "q1".into(),
                model: Some("anthropic:claude-haiku-4-5".into()),
            }]
        );

        let mut c = setup_with_worker();
        c.set_model_questions(vec![model_question("q2")]);
        read_the_prompt(&mut c);
        assert_eq!(
            c.handle_key(ch('d')),
            vec![Effect::AnswerModelQuestion {
                id: "q2".into(),
                model: None,
            }]
        );

        let mut c = setup_with_worker();
        c.set_model_questions(vec![model_question("q3")]);
        read_the_prompt(&mut c);
        assert!(c.handle_key(esc()).is_empty());
        assert!(c.overlay().is_none());
        // the next poll does not throw it straight back up …
        c.last_key_at = 0;
        c.set_model_questions(vec![model_question("q3")]);
        assert!(c.overlay().is_none(), "dismissed once, it stays dismissed");
        // … and `a` on the orchestrator's row brings it back
        select_row(&mut c, 0);
        fleet_key(&mut c, 'a');
        assert!(matches!(c.overlay(), Some(Overlay::ModelChoice(s)) if s.id == "q3"));
    }
    {
        let mut c = setup_with_worker();
        type_text(&mut c, "half a thought");
        c.set_model_questions(vec![model_question("q1")]);
        assert!(c.overlay().is_none(), "not while the composer holds text");
        c.composer.input.clear();
        c.last_key_at = 0;
        c.set_model_questions(vec![model_question("q1")]);
        assert!(matches!(c.overlay(), Some(Overlay::ModelChoice(_))));
        // the spawn gave up (or was answered elsewhere): nothing to answer
        c.set_model_questions(Vec::new());
        assert!(c.overlay().is_none());
    }
    {
        // a permission prompt raised once and dismissed does not keep a
        // model question down: the spawn behind the question is waiting
        let mut c = setup_with_worker();
        c.set_orchestrator_state(bash_request("req_1"));
        assert!(matches!(c.overlay(), Some(Overlay::Permission(_))));
        c.handle_key(esc());
        c.last_key_at = 0;
        c.set_model_questions(vec![model_question("q9")]);
        assert!(
            matches!(c.overlay(), Some(Overlay::ModelChoice(s)) if s.id == "q9"),
            "{:?}",
            c.overlay()
        );
    }
}

#[test]
fn catalogue_refresh() {
    {
        let mut c = setup_with_worker();
        write_old_catalogue(&mut c, vec![model("glm-5.3")]);
        let effects = c.handle_key(ctrl('k'));
        assert!(
            effects.contains(&Effect::RefreshWorkerCapabilities {
                run_id: "db-20260829120000".to_string(),
            }),
            "{effects:?}"
        );
        assert!(!effects.contains(&Effect::FetchPiCatalogue), "{effects:?}");
    }
    {
        let mut c = test_console();
        write_old_catalogue(&mut c, vec![model("glm-5.3")]);
        let effects = c.handle_key(ctrl('k'));
        assert!(
            effects.contains(&Effect::FetchPiCatalogue),
            "the one-shot stands in for the missing worker: {effects:?}"
        );
        // and a fresh catalogue is left alone
        let mut c = test_console();
        crate::fleet::run::write_pi_cache(
            c.fleet.root(),
            &crate::fleet::run::PiCache {
                fetched_at: crate::util::now_iso(),
                available_models: vec![model("glm-5.3")],
                commands: Vec::new(),
            },
        )
        .unwrap();
        let effects = c.handle_key(ctrl('k'));
        assert!(!effects.contains(&Effect::FetchPiCatalogue), "{effects:?}");
        assert!(
            !effects.contains(&Effect::RefreshWorkerCapabilities {
                run_id: "db-20260829120000".to_string(),
            }),
            "{effects:?}"
        );
    }
    {
        // no catalogue at all: the state that once kept a fetch, a reload and a
        // key read going round every feed tick
        let mut c = test_console();
        c.overlay = Some(Overlay::Routing(RoutingPanel::default()));
        c.reload_routing_status();
        assert!(
            !c.pi_fetch
                .in_flight
                .load(std::sync::atomic::Ordering::SeqCst),
            "only opening the panel, the palette or `m` asks pi"
        );
        // a fetch that finished is collected, and reloads, without starting another
        *c.pi_fetch.result.lock().unwrap() = Some(Vec::new());
        c.collect_pi_fetch();
        assert!(
            !c.pi_fetch
                .in_flight
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(c.pi_fetch.result.lock().unwrap().is_none(), "reported once");
    }
    {
        let mut c = setup_with_worker();
        open_routing(&mut c, true, KeyState::None);
        assert_eq!(
            c.handle_key(ch('+')),
            vec![Effect::SetRoutingThreshold(0.65)]
        );
        assert_eq!(
            c.handle_key(ch('-')),
            vec![Effect::SetRoutingThreshold(0.55)]
        );
        if let Some(Overlay::Routing(panel)) = &mut c.overlay {
            panel.status.as_mut().unwrap().threshold = 1.0;
        }
        assert!(c.handle_key(ch('+')).is_empty(), "1 is as high as it goes");
    }
}

fn priced_model(provider: &str, id: &str) -> WorkerModel {
    WorkerModel::new(provider, id)
}

#[test]
fn shortlist_editor() {
    {
        let mut c = setup_with_worker();
        let catalogue = vec![
            priced_model("anthropic", "claude-opus-5"),
            priced_model("openrouter", "deepseek-v4-flash"),
            priced_model("opencode-go", "deepseek-v4-flash"),
        ];
        crate::fleet::run::write_pi_cache(
            c.fleet.root(),
            &crate::fleet::run::PiCache {
                available_models: catalogue,
                ..Default::default()
            },
        )
        .unwrap();
        open_routing(&mut c, true, KeyState::None);
        c.handle_key(ch('m'));
        let editor = |c: &Console| match c.overlay() {
            Some(Overlay::Routing(panel)) => panel.shortlist.clone().unwrap(),
            other => panic!("not the shortlist: {other:?}"),
        };
        let opened = editor(&c);
        assert_eq!(
            opened.chosen.iter().cloned().collect::<Vec<_>>(),
            vec![
                "opencode-go:deepseek-v4-flash",
                "openrouter:deepseek-v4-flash"
            ],
            "a bare id ticks every provider of it"
        );
        assert_eq!(opened.kept, vec!["gone:retired-model"]);
        // letters are the filter, not commands
        type_text(&mut c, "opus");
        assert_eq!(editor(&c).visible, vec![0]);
        c.handle_key(enter());
        assert!(editor(&c).chosen.contains("anthropic:claude-opus-5"));
        assert_eq!(
            c.handle_key(esc()),
            vec![Effect::SetRoutingModels(vec![
                "anthropic:claude-opus-5".into(),
                "opencode-go:deepseek-v4-flash".into(),
                "openrouter:deepseek-v4-flash".into(),
                "gone:retired-model".into(),
            ])]
        );
        assert!(
            matches!(c.overlay(), Some(Overlay::Routing(panel)) if panel.shortlist.is_none()),
            "back on the panel"
        );
    }
    {
        let mut c = setup_with_worker();
        let catalogue: Vec<WorkerModel> = (0..=crate::route::MAX_CHOICES)
            .map(|i| priced_model("openrouter", &format!("m-{i:03}")))
            .collect();
        let all: Vec<String> = catalogue
            .iter()
            .take(crate::route::MAX_CHOICES)
            .map(WorkerModel::key)
            .collect();
        let mut editor = ShortlistEditor::new(catalogue, &all);
        assert_eq!(editor.count(), crate::route::MAX_CHOICES);
        editor.selected = crate::route::MAX_CHOICES; // the one left unticked
        c.overlay = Some(Overlay::Routing(RoutingPanel {
            shortlist: Some(editor),
            ..RoutingPanel::default()
        }));
        c.handle_key(enter());
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("still editing");
        };
        let editor = panel.shortlist.as_ref().unwrap();
        assert_eq!(editor.count(), crate::route::MAX_CHOICES, "not ticked");
        assert!(!editor.changed);
        assert!(c.flash().is_some_and(|f| f.error && f.text.contains("255")));
    }
    {
        let catalogue = vec![
            priced_model("anthropic", "claude-opus-5"),
            priced_model("openrouter", "deepseek-v4-flash"),
        ];
        let editor = ShortlistEditor::new(catalogue, &[]);
        assert!(editor.chosen.is_empty(), "every tick is the user's own");
        assert_eq!(editor.count(), 0);
        assert_eq!(editor.visible.len(), 2, "all of them still listed");
    }
    {
        let mut c = test_console();
        assert_eq!(c.submit("/routing"), vec![Effect::LoadRoutingStatus]);
        if let Some(Overlay::Routing(panel)) = &mut c.overlay {
            panel.status = Some(RoutingStatus {
                enabled: true,
                key: KeyState::None,
                candidates: Err("pi could not be asked for its models".to_string()),
                threshold: 0.6,
                models: Vec::new(),
            });
        }
        // no pi-cache.json anywhere: `m` starts a fetch (an effect the runtime
        // executes off the UI loop) instead of claiming there is no catalogue
        let effects = c.handle_key(ch('m'));
        assert_eq!(effects, vec![Effect::FetchPiCatalogue]);
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("still on the panel");
        };
        assert!(panel.shortlist.is_none(), "no editor over nothing");
        assert!(
            c.flash()
                .is_some_and(|f| f.text.contains("fetching pi's model list")),
            "the toast says pi is being asked, not that there is no catalogue"
        );
    }
    {
        let mut c = test_console();
        write_old_catalogue(&mut c, vec![model("glm-5.3"), model("glm-5.3-flash")]);
        assert_eq!(c.submit("/routing"), vec![Effect::LoadRoutingStatus]);
        if let Some(Overlay::Routing(panel)) = &mut c.overlay {
            panel.status = Some(RoutingStatus {
                enabled: true,
                key: KeyState::None,
                candidates: Ok(2),
                threshold: 0.6,
                models: Vec::new(),
            });
        }
        assert!(
            c.handle_key(ch('m')).is_empty(),
            "no fetch over a real catalogue"
        );
        let Some(Overlay::Routing(panel)) = c.overlay() else {
            panic!("back on the panel");
        };
        let editor = panel.shortlist.as_ref().unwrap();
        assert_eq!(editor.visible.len(), 2);
    }
}

// -- the pi catalogue on demand -----------------------------------------

fn old_catalogue(models: Vec<WorkerModel>) -> crate::fleet::run::PiCache {
    crate::fleet::run::PiCache {
        fetched_at: "2020-01-02T03:04:05.000Z".to_string(),
        available_models: models,
        commands: Vec::new(),
    }
}

/// A catalogue with a past `fetchedAt`, written straight to disk: the
/// atomic writer stamps the timestamp itself, so a stale cache has to skip it.
fn write_old_catalogue(c: &mut Console, models: Vec<WorkerModel>) {
    std::fs::write(
        c.fleet.root().join(crate::paths::PI_CACHE_FILE),
        serde_json::to_string(&old_catalogue(models)).expect("serializable"),
    )
    .unwrap();
}

fn model(id: &str) -> WorkerModel {
    WorkerModel::new("fakeprovider", id)
}

/// A console whose user config says `config`, and whose fleet has a fresh
/// pi catalogue — so opening the routing panel never starts a real pi.
fn console_with_user_config(config: &str) -> Console {
    let c = test_console();
    let home = c.user_dir.clone().unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), config).unwrap();
    crate::fleet::run::write_pi_cache(
        c.fleet.root(),
        &crate::fleet::run::PiCache {
            available_models: vec![WorkerModel::new("anthropic", "claude-opus-5")],
            ..Default::default()
        },
    )
    .unwrap();
    c
}

#[test]
fn unconfirmed_thinking_lapses() {
    let now = crate::util::now_ms();
    let mut c = setup_with_worker();
    // the orchestrator never applied it: the console stops claiming it
    c.pending_effort = Some(("max".into(), now - 60_000));
    c.set_orchestrator_state(OrchestratorState {
        effort: Some("low".into()),
        permission_mode: "default".into(),
        ..OrchestratorState::default()
    });
    assert_eq!(c.effort(), Some("low"));
    assert!(
        c.flash()
            .unwrap()
            .text
            .contains("did not take thinking max"),
        "{:?}",
        c.flash()
    );
    // the same for a worker, while a fresh change still shows
    let run_id = "db-20260829120000".to_string();
    c.pending_thinking
        .insert(run_id.clone(), ("xhigh".into(), now - 60_000));
    c.set_runs(vec![running_run(&run_id, "db")]);
    assert!(c.pending_thinking.is_empty());
    assert_ne!(
        c.run_state(&run_id).unwrap().thinking_level.as_deref(),
        Some("xhigh")
    );
    c.pending_thinking
        .insert(run_id.clone(), ("high".into(), crate::util::now_ms()));
    c.set_runs(vec![running_run(&run_id, "db")]);
    assert_eq!(
        c.run_state(&run_id).unwrap().thinking_level.as_deref(),
        Some("high")
    );
}

#[tokio::test]
async fn console_worker_ops() {
    let mut c = test_console();
    write_run(&c, "db-20260829120000", "db", Uuid::new_v4());
    let inbox = c.fleet.run_inbox("db-20260829120000");

    // a steer is signed by the console, so the watcher tells it apart
    c.execute_all(vec![Effect::WorkerSteer {
        run_id: "db-20260829120000".into(),
        message: "use argon2".into(),
    }])
    .await;
    let lines = std::fs::read_to_string(&inbox).unwrap();
    let envelope: crate::fleet::envelope::Envelope =
        serde_json::from_str(lines.lines().last().unwrap()).unwrap();
    assert_eq!(envelope.from, Party::Console);
    assert_eq!(envelope.kind, "steer");

    // a refusal is a notice, not text printed under the screen
    let dir = c.fleet.run_dir("db-20260829120000");
    let mut state = crate::fleet::run::load_state(&dir).unwrap();
    state.status = RunStatus::Settled;
    crate::fleet::run::save_state(&dir, &state).unwrap();
    c.execute_all(vec![Effect::WorkerAbort {
        run_id: "db-20260829120000".into(),
    }])
    .await;
    let flash = c.chrome_flash().unwrap();
    assert!(
        flash.error && flash.text.contains("is settled"),
        "{flash:?}"
    );
    assert_eq!(std::fs::read_to_string(&inbox).unwrap(), lines);
}
