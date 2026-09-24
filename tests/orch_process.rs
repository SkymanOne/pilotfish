//! The orchestrator process driven against the scripted claude stand-in
//! (`tests/fixtures/fake-claude.mjs`): hermetic, no tokens spent.
//!
//! Ports the behaviours pinned by the TypeScript
//! `tests/orchestrator-process.test.ts` (minus the session store, which is not
//! part of the orch step) and adds the `set_model` control path and the 5 s
//! control-request timeout the rewrite makes explicit.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use pilotfish::orch::process::{
    ControlOutcome, OrchestratorOptions, OrchestratorProcess, ProcEvent,
};
use pilotfish::orch::protocol::{is_replayed_user_message, text_of_assistant, user_text};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Harness

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "{name}-{}-{}",
        std::process::id(),
        pilotfish::util::new_id("t").replace('_', "")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn node_bin() -> Option<String> {
    if let Ok(path) = std::env::var("PILOTFISH_TEST_NODE") {
        return Some(path);
    }
    let ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    ok.then(|| "node".to_string())
}

fn fake_claude_env(over: &[(&str, &str)]) -> HashMap<String, String> {
    let node = node_bin().expect("node is available");
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.insert(
        pilotfish::paths::env_var("CLAUDE_BIN"),
        format!("{} {}", node, fixture("fake-claude.mjs").display()),
    );
    for (key, value) in over {
        env.insert((*key).to_string(), (*value).to_string());
    }
    env
}

fn is_alive(pid: u32) -> bool {
    let pid = i32::try_from(pid).expect("pids fit in i32");
    // SIGCONT is a no-op on a running process, and the only signal nix can
    // send without a target-specific risk; ESRCH means it is gone.
    matches!(kill(Pid::from_raw(pid), Signal::SIGCONT), Ok(()))
}

fn start_proc(
    root: &Path,
    over: &[(&str, &str)],
) -> (Arc<OrchestratorProcess>, mpsc::UnboundedReceiver<ProcEvent>) {
    let prompt_file = root.join("prompt.md");
    std::fs::write(&prompt_file, "# test prompt\n").unwrap();
    let mut options = OrchestratorOptions::new(
        root.to_path_buf(),
        prompt_file.to_string_lossy().into_owned(),
        "{}".into(),
    );
    options.log_path = Some(root.join("claude.log"));
    options.env = Some(fake_claude_env(over));
    options.stop_grace_ms = 500;
    let (process, rx) = OrchestratorProcess::new(options);
    process.start();
    (process, rx)
}

/// Receive events until one matches, returning it and everything skipped.
async fn collect_until(
    rx: &mut mpsc::UnboundedReceiver<ProcEvent>,
    timeout: Duration,
    mut pred: impl FnMut(&ProcEvent) -> bool,
) -> (Vec<ProcEvent>, ProcEvent) {
    let deadline = Instant::now() + timeout;
    let mut skipped = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for an event");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(event)) if pred(&event) => return (skipped, event),
            Ok(Some(event)) => skipped.push(event),
            Ok(None) => panic!("event stream ended"),
            Err(err) => panic!("timed out waiting for an event: {err}"),
        }
    }
}

async fn wait_for(
    rx: &mut mpsc::UnboundedReceiver<ProcEvent>,
    timeout: Duration,
    pred: impl FnMut(&ProcEvent) -> bool,
) -> ProcEvent {
    collect_until(rx, timeout, pred).await.1
}

/// Poll a log file until it satisfies `pred` (the protocol log is written
/// line-buffered as we go, but the child writes asynchronously).
async fn wait_log(path: &Path, timeout: Duration, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && pred(&text)
        {
            return text;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the log");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn result_text(event: &ProcEvent) -> Option<String> {
    match event {
        ProcEvent::Result(result) => result.result.clone(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn turn_protocol() {
    if node_bin().is_none() {
        eprintln!("skipping: node is not available");
        return;
    }
    {
        let root = tmp_dir("pilotfish-proc-1-");
        let argv_file = root.join("argv.json");
        let (process, mut rx) = start_proc(
            &root,
            &[
                ("FAKE_CLAUDE_ARGV_FILE", &argv_file.to_string_lossy()),
                ("FAKE_CLAUDE_SESSION_ID", "sess-fixed"),
            ],
        );
        assert!(process.pid().is_some(), "the child spawned with a pid");
        assert!(process.running(), "the process is running");
        assert!(
            !process.init_received(),
            "init arrives only with the first turn"
        );
        assert!(process.send("hello"), "the first message was accepted");
        assert!(process.turn_active(), "a send starts a turn");

        // The fake emits system/init first, then the replay, deltas, assistant and
        // result: collect through to the result and find each shape in between.
        let (events, result_event) = collect_until(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        let ProcEvent::Result(result) = result_event else {
            unreachable!()
        };
        assert_eq!(result.subtype, "success");
        assert!(!process.turn_active(), "the turn ended with the result");
        let cost = process.cost_usd();
        assert!(
            (cost - 0.001).abs() < f64::EPSILON,
            "one turn costs $0.001, was {cost}"
        );
        assert_eq!(process.num_turns(), 1);

        let init = events
            .iter()
            .find_map(|event| match event {
                ProcEvent::Init(init) => Some(init.clone()),
                _ => None,
            })
            .expect("init arrives with the first turn");
        assert_eq!(init.session_id, "sess-fixed");
        assert_eq!(process.session_id().as_deref(), Some("sess-fixed"));
        assert_eq!(process.model().as_deref(), Some("fake-model"));
        assert_eq!(
            process.capabilities(),
            vec!["interrupt_receipt_v1".to_string()]
        );

        let users: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                ProcEvent::User(msg) if is_replayed_user_message(msg) => user_text(msg),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["hello"]);
        let deltas: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                ProcEvent::TextDelta(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["echo: hello"]);
        let assistants: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                ProcEvent::Assistant(msg) => Some(text_of_assistant(msg)),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec!["echo: hello"]);

        // the child saw the exact permission-prompt flag
        let argv: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(&argv_file).unwrap()).unwrap();
        let prompt_tool = argv
            .iter()
            .position(|a| a == "--permission-prompt-tool")
            .map(|i| &argv[i + 1]);
        assert_eq!(prompt_tool.map(String::as_str), Some("stdio"));
        assert!(
            argv.iter().any(|a| a == "--replay-user-messages"),
            "expected the replay flag in the argv: {argv:?}"
        );

        // the protocol log carries both directions. Key order inside a line is
        // serde_json's (sorted), so match on type markers, not on byte shapes.
        let log_path = root.join("claude.log");
        let log = wait_log(&log_path, Duration::from_secs(5), |text| {
            text.lines()
                .any(|l| l.starts_with("> ") && l.contains("\"type\":\"user\""))
                && text
                    .lines()
                    .any(|l| l.starts_with("< ") && l.contains("\"subtype\":\"init\""))
        })
        .await;
        assert!(
            log.lines()
                .any(|l| l.starts_with("< ") && l.contains("\"type\":\"system\""))
        );

        // the real CLI re-emits system/init after every user message; the wrapper
        // must not mind
        assert!(process.send("second"), "the second message was accepted");
        let (second, _) = collect_until(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        assert_eq!(
            second
                .iter()
                .filter(|event| matches!(event, ProcEvent::Init(_)))
                .count(),
            1
        );
        assert_eq!(process.num_turns(), 2);
        let cost = process.cost_usd();
        assert!(
            (cost - 0.002).abs() < f64::EPSILON,
            "two turns cost $0.002, was {cost}"
        );

        process.stop().await;
        assert!(
            !is_alive(process.pid().unwrap()),
            "the child is gone after stop"
        );
        assert!(process.exited().is_some(), "the exit is recorded");
    }
    {
        let root = tmp_dir("pilotfish-proc-cmds-");
        let (process, mut rx) = start_proc(&root, &[]);
        let ProcEvent::Commands(commands) = wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Commands(_))
        })
        .await
        else {
            unreachable!()
        };
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["model", "usage", "research"]);
        assert_eq!(
            process
                .slash_commands()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<&str>>(),
            vec!["model", "usage", "research"]
        );
        assert_eq!(commands[0].argument_hint.as_deref(), Some("<model>"));
        assert_eq!(
            commands[1].aliases.as_deref(),
            Some(&["cost".to_string()][..])
        );
        process.stop().await;
    }
    {
        let root = tmp_dir("pilotfish-proc-init-");
        let (process, mut rx) = start_proc(&root, &[("FAKE_CLAUDE_REQUIRE_INIT", "1")]);
        // this fake refuses to prompt until it has seen an initialize control request
        wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Commands(_))
        })
        .await;
        let log = wait_log(&root.join("claude.log"), Duration::from_secs(5), |text| {
            text.contains("\"subtype\":\"initialize\"")
        })
        .await;
        assert_eq!(
            log.matches("\"subtype\":\"initialize\"").count(),
            1,
            "sent exactly once: {log}"
        );
        assert!(
            process.send("perm:touch d.txt"),
            "the permission prompt was requested"
        );
        let ProcEvent::PermissionRequest(req) =
            wait_for(&mut rx, Duration::from_secs(10), |event| {
                matches!(event, ProcEvent::PermissionRequest(_))
            })
            .await
        else {
            unreachable!()
        };
        assert!(
            process.allow(&req.request_id, None),
            "the allow is accepted"
        );
        let result = wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        assert_eq!(result_text(&result).as_deref(), Some("allowed:touch d.txt"));
        process.stop().await;
    }
}

#[tokio::test]
async fn control_requests() {
    if node_bin().is_none() {
        eprintln!("skipping: node is not available");
        return;
    }
    {
        let root = tmp_dir("pilotfish-proc-2-");
        let (process, mut rx) = start_proc(&root, &[]);
        assert!(
            process.send("perm:touch a.txt"),
            "the permission prompt was requested"
        );
        let ProcEvent::PermissionRequest(req) =
            wait_for(&mut rx, Duration::from_secs(10), |event| {
                matches!(event, ProcEvent::PermissionRequest(_))
            })
            .await
        else {
            unreachable!()
        };
        assert_eq!(req.request.tool_name, "Bash");
        assert_eq!(req.request.input["command"], "touch a.txt");
        assert_eq!(req.request.title.as_deref(), Some("Run touch a.txt"));
        assert_eq!(process.pending_requests().len(), 1);

        let suggestions = req.request.permission_suggestions.clone();
        assert!(
            process.allow(&req.request_id, Some(&suggestions)),
            "the allow is accepted"
        );
        assert_eq!(process.pending_requests().len(), 0);
        assert!(!process.allow(&req.request_id, None), "already answered");
        let r1 = wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        assert_eq!(result_text(&r1).as_deref(), Some("allowed:touch a.txt"));

        assert!(
            process.send("perm:rm -rf x"),
            "the second prompt was requested"
        );
        let ProcEvent::PermissionRequest(req2) =
            wait_for(&mut rx, Duration::from_secs(10), |event| {
                matches!(event, ProcEvent::PermissionRequest(_))
            })
            .await
        else {
            unreachable!()
        };
        assert!(process.deny(&req2.request_id, "not that"));
        let r2 = wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        assert_eq!(result_text(&r2).as_deref(), Some("denied:not that"));

        assert!(
            process.send("ask:Which style?|terse|verbose"),
            "the question was queued"
        );
        let ProcEvent::PermissionRequest(ask) =
            wait_for(&mut rx, Duration::from_secs(10), |event| {
                matches!(event, ProcEvent::PermissionRequest(_))
            })
            .await
        else {
            unreachable!()
        };
        assert_eq!(ask.request.tool_name, "AskUserQuestion");
        let questions = &ask.request.input["questions"];
        assert_eq!(questions[0]["question"], "Which style?");
        let labels: Vec<&str> = questions[0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, vec!["terse", "verbose"]);
        assert!(process.answer_question(
            &ask.request_id,
            serde_json::json!({"Which style?": "verbose"}),
        ));
        let r3 = wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        assert_eq!(
            result_text(&r3).as_deref(),
            Some(r#"answers:{"Which style?":"verbose"}"#)
        );
        // the log carries the answer we sent
        let _ = wait_log(&root.join("claude.log"), Duration::from_secs(5), |text| {
            text.contains(r#""answers":{"Which style?":"verbose"}"#)
        })
        .await;
        process.stop().await;
    }
    {
        let root = tmp_dir("pilotfish-proc-3-");
        let (process, mut rx) = start_proc(&root, &[("FAKE_CLAUDE_NO_FLAG_SETTINGS", "1")]);
        assert!(process.send("slow:"), "the slow turn was accepted");
        wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::TextDelta(_))
        })
        .await;
        let receipt = process.interrupt(false).await;
        assert_eq!(
            receipt,
            Some(ControlOutcome::Success(
                serde_json::json!({"still_queued": []})
            ))
        );
        let result = wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        let ProcEvent::Result(result) = result else {
            unreachable!()
        };
        assert_eq!(result.result.as_deref(), Some("interrupted"));
        assert!(!process.turn_active(), "the interrupt ended the turn");

        // errors surface as results
        assert!(process.send("fail:"), "the failing turn was accepted");
        let failed = wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::Result(_))
        })
        .await;
        let ProcEvent::Result(failed) = failed else {
            unreachable!()
        };
        assert_eq!(failed.subtype, "error_during_execution");
        assert_eq!(failed.is_error, Some(true));

        // control requests resolve to the CLI's receipt — or its verbatim error
        assert_eq!(
            process.set_permission_mode("acceptEdits").await,
            Some(ControlOutcome::Success(serde_json::json!({})))
        );
        // the fake does not validate model names; the receipt is still the CLI's
        assert_eq!(
            process.set_model("fable").await,
            Some(ControlOutcome::Success(serde_json::json!({})))
        );
        let error = process
            .apply_flag_settings(serde_json::json!({"effort": "high"}))
            .await;
        assert_eq!(error, Some(ControlOutcome::Error("unknown subtype".into())));
        process.stop().await;
    }
    {
        let root = tmp_dir("pilotfish-proc-timeout-");
        let prompt_file = root.join("p.md");
        std::fs::write(&prompt_file, "x").unwrap();
        let mut options = OrchestratorOptions::new(
            root.clone(),
            prompt_file.to_string_lossy().into_owned(),
            "{}".into(),
        );
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert(
            pilotfish::paths::env_var("CLAUDE_BIN"),
            format!("{} {}", node_bin().unwrap(), fixture("hang.mjs").display()),
        );
        options.env = Some(env);
        options.stop_grace_ms = 200;
        let (process, _rx) = OrchestratorProcess::new(options);
        process.start();
        let started = Instant::now();
        // hang.mjs never answers a control request; the waiter resolves to none
        assert_eq!(process.interrupt(false).await, None);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(4_900),
            "waited the full timeout, took {elapsed:?}"
        );
        process.stop_now().await;
    }
}

#[tokio::test]
async fn shutdown() {
    if node_bin().is_none() {
        eprintln!("skipping: node is not available");
        return;
    }
    {
        let root = tmp_dir("pilotfish-proc-stopturn-");
        let (process, mut rx) = start_proc(&root, &[]);
        assert!(process.send("slow:"), "the slow turn was accepted");
        wait_for(&mut rx, Duration::from_secs(10), |event| {
            matches!(event, ProcEvent::TextDelta(_))
        })
        .await;
        assert!(process.turn_active(), "the turn is active while streaming");
        process.stop().await;
        // the interrupt went out before the child was closed, so the turn is not
        // left half-finished for the next session to resume into
        let log = std::fs::read_to_string(root.join("claude.log")).unwrap();
        let interrupt_at = log
            .find("\"subtype\":\"interrupt\"")
            .expect("an interrupt was sent");
        assert!(
            log[interrupt_at..].contains("\"result\""),
            "and the turn ended before we closed the child: {}",
            &log[interrupt_at..]
        );
        assert!(!process.turn_active(), "the interrupt ended the turn");
        // a second stop is a no-op returning the recorded exit
        let info = process.stop_now().await;
        assert_eq!(info, process.exited().unwrap());
    }
    {
        let root = tmp_dir("pilotfish-proc-6-");
        let prompt_file = root.join("p.md");
        std::fs::write(&prompt_file, "x").unwrap();
        let mut options = OrchestratorOptions::new(
            root.clone(),
            prompt_file.to_string_lossy().into_owned(),
            "{}".into(),
        );
        // hang.mjs ignores stdin closing, so only the signals end it
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert(
            pilotfish::paths::env_var("CLAUDE_BIN"),
            format!("{} {}", node_bin().unwrap(), fixture("hang.mjs").display()),
        );
        options.env = Some(env);
        options.stop_grace_ms = 200;
        let (process, _rx) = OrchestratorProcess::new(options);
        process.start();
        let started = Instant::now();
        let info = process.stop().await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(info.signal.as_deref(), Some("SIGTERM"));
        assert!(
            !is_alive(process.pid().unwrap()),
            "the child is gone after stop"
        );
    }
    {
        let root = tmp_dir("pilotfish-proc-dead-");
        let (process, mut rx) = start_proc(&root, &[]);
        process.stop().await;
        assert!(process.exited().is_some(), "the exit is recorded");
        assert!(!process.running(), "the process is no longer running");
        // A write racing the child's death must not take the process down.
        assert!(!process.send("too late"), "a write after death is refused");
        assert!(
            !process.allow("req_x", None),
            "an allow after death is refused"
        );
        assert!(
            !process.deny("req_x", "no"),
            "a deny after death is refused"
        );
        let started = Instant::now();
        assert_eq!(process.interrupt(false).await, None);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the dead-child write returned fast, took {:?}",
            started.elapsed()
        );
        // the event stream still ends with the exit
        let _ = wait_for(&mut rx, Duration::from_secs(2), |event| {
            matches!(event, ProcEvent::Exit(_))
        })
        .await;
    }
}
