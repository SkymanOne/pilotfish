//! The orchestrator monitor and console client driven end to end against the
//! scripted claude stand-in (`tests/fixtures/fake-claude.mjs`), like the
//! TypeScript `tests/orchestrator-monitor.test.ts`. The monitor runs as the
//! real `parl orchestrator-monitor` binary, detached, exactly as the console
//! spawns it. Hermetic: no network, no tokens.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use parl::fleet::run::is_alive;
use parl::orch::client::{ClientEvent, OrchestratorClient, OrchestratorClientOptions};
use parl::orch::monitor::load_orchestrator_state;
use parl::orch::records::EventRecord;
use parl::paths::FleetPaths;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

fn fake_claude() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.mjs")
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// One fleet dir with the fake-claude environment wired for the monitor.
struct Fixture {
    _tmp: tempfile::TempDir,
    fleet_dir: PathBuf,
    cwd: PathBuf,
    session_id: &'static str,
}

impl Fixture {
    fn new(session_id: &'static str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let fleet_dir = tmp.path().join(".parl");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        Self {
            _tmp: tmp,
            fleet_dir,
            cwd,
            session_id,
        }
    }

    /// A client for this fleet, pointed at the real binary and the fake
    /// claude. The fake-claude knobs travel through the monitor's env, like
    /// the TypeScript test's `useFakeClaude`.
    fn client(&self, fresh: bool) -> Arc<OrchestratorClient> {
        self.client_with(fresh, HashMap::new(), None)
    }

    /// A client with extra fake-claude env, and optionally a `~/.parl` of its
    /// own so a test can set user config without touching the real home.
    fn client_with(
        &self,
        fresh: bool,
        extra: HashMap<String, String>,
        user_dir: Option<&Path>,
    ) -> Arc<OrchestratorClient> {
        let mut env: HashMap<String, String> = extra;
        env.insert(
            "PARL_CLAUDE_BIN".to_string(),
            format!("node {}", fake_claude().display()),
        );
        // The monitor must never inherit an ambient PARL_DIR: its fleet is
        // the one this fixture created (also passed as --fleet-dir).
        env.insert(
            "PARL_DIR".to_string(),
            self.fleet_dir.to_string_lossy().into_owned(),
        );
        env.insert(
            "FAKE_CLAUDE_ARGV_FILE".to_string(),
            self.cwd.join("argv.json").to_string_lossy().into_owned(),
        );
        env.insert(
            "FAKE_CLAUDE_SESSION_ID".to_string(),
            self.session_id.to_string(),
        );
        if let Some(dir) = user_dir {
            env.insert("PARL_HOME".to_string(), dir.to_string_lossy().into_owned());
        }
        OrchestratorClient::new(OrchestratorClientOptions {
            fresh,
            monitor_bin: Some(assert_cmd::cargo_bin!("parl").to_path_buf()),
            monitor_env: Some(env),
            poll_ms: 30,
            user_config_dir: user_dir.map(Path::to_path_buf),
            ..OrchestratorClientOptions::new(self.fleet_dir.clone(), self.cwd.clone())
        })
    }
}

/// Poll until `check` holds or the timeout lapses.
async fn wait(timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting");
        tokio::time::sleep(POLL).await;
    }
}

/// Receive events until one matches, returning it.
async fn wait_event(
    rx: &mut mpsc::UnboundedReceiver<ClientEvent>,
    timeout: Duration,
    pred: impl Fn(&ClientEvent) -> bool,
) -> ClientEvent {
    let deadline = Instant::now() + timeout;
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for an event");
        match tokio::time::timeout(POLL, rx.recv()).await {
            Ok(Some(event)) if pred(&event) => return event,
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => panic!("event stream ended"),
        }
    }
}

fn session_key(fleet_dir: &Path) -> parl::paths::SessionKey {
    parl::orch::session::resolve_session(fleet_dir)
        .expect("the monitor writes a session row")
        .key()
}

fn state_of(fleet_dir: &Path) -> Option<parl::orch::records::OrchestratorState> {
    load_orchestrator_state(fleet_dir, &session_key(fleet_dir))
}

fn caps_of(fleet_dir: &Path) -> parl::orch::records::Capabilities {
    parl::orch::records::read_capabilities(
        &FleetPaths::new(fleet_dir).orchestrator_capabilities(&session_key(fleet_dir)),
    )
}

fn monitor_pid(fleet_dir: &Path) -> Option<i32> {
    state_of(fleet_dir).and_then(|state| state.pid)
}

/// The parsed transcript records, oldest first.
fn transcript_of(fleet_dir: &Path) -> Vec<EventRecord> {
    let raw = std::fs::read_to_string(
        FleetPaths::new(fleet_dir).orchestrator_events(&session_key(fleet_dir)),
    )
    .unwrap_or_default();
    raw.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<EventRecord>(line).ok())
        .collect()
}

fn count_results(fleet_dir: &Path) -> usize {
    transcript_of(fleet_dir)
        .iter()
        .filter(|record| record.kind == "result")
        .count()
}

/// Stop the monitor the way a reboot would: SIGTERM, then wait until the
/// state no longer claims a live pid — both the process and its last state
/// write must be done, or the next client sees something to attach to.
async fn stop_monitor(fleet_dir: &Path) {
    let Some(pid) = monitor_pid(fleet_dir) else {
        return;
    };
    if is_alive(Some(pid)) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    wait(Duration::from_secs(10), || match state_of(fleet_dir) {
        Some(state) => state.pid.is_none_or(|pid| !is_alive(Some(pid))),
        None => true,
    })
    .await;
}

fn argv_of(cwd: &Path) -> Vec<String> {
    serde_json::from_str(&std::fs::read_to_string(cwd.join("argv.json")).unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn the_monitor_owns_the_claude_session_and_consoles_come_and_go() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fixture = Fixture::new("sess-detach01");
    let first = fixture.client(true);
    let mut rx = first.subscribe();
    let started = first.start().unwrap();
    assert!(!started, "there was nothing to attach to yet");

    // claude only announces its session on the first turn, so wait for the
    // monitor itself, then send
    wait(WAIT, || first.running()).await;
    first.send("hello there").await.unwrap();
    wait_event(
        &mut rx,
        WAIT,
        |event| matches!(event, ClientEvent::Record(record) if record.kind == "result"),
    )
    .await;

    let state = state_of(&fixture.fleet_dir).unwrap();
    assert_eq!(state.session_id.as_deref(), Some("sess-detach01"));
    assert_eq!(state.model.as_deref(), Some("fake-model"));
    let monitor = state.pid.expect("a monitor pid");
    assert!(is_alive(Some(monitor)), "a monitor is running");
    let caps = caps_of(&fixture.fleet_dir);
    let names: Vec<&str> = caps.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["model", "usage", "research"]);
    assert!(!caps.fetched_at.is_empty(), "the answer is stamped");
    wait(Duration::from_secs(5), || {
        transcript_of(&fixture.fleet_dir)
            .iter()
            .any(|record| record.kind == "stream_text")
    })
    .await;

    // this is what /quit does: the console goes away, the orchestrator does not
    first.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        is_alive(Some(monitor)),
        "the orchestrator survived the console"
    );

    // reopening picks the same session back up: the same monitor, not a new one
    let second = fixture.client(false);
    let reattached = second.start().unwrap();
    assert!(reattached, "it attached rather than starting another one");
    wait(WAIT, || monitor_pid(&fixture.fleet_dir) == Some(monitor)).await;

    // and it still works
    second.send("again").await.unwrap();
    wait(WAIT, || count_results(&fixture.fleet_dir) >= 2).await;
    second.stop();
    stop_monitor(&fixture.fleet_dir).await;
}

#[tokio::test]
async fn a_permission_request_waits_until_some_console_answers_it() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fixture = Fixture::new("sess-perm01");
    let first = fixture.client(true);
    let mut rx = first.subscribe();
    first.start().unwrap();
    wait(WAIT, || first.running()).await;

    first.send("perm:touch a.txt").await.unwrap();
    let asked = wait_event(&mut rx, WAIT, |event| {
        matches!(event, ClientEvent::PermissionRequest(_))
    })
    .await;
    let ClientEvent::PermissionRequest(pending) = asked else {
        unreachable!()
    };
    assert_eq!(pending.request.tool_name, "Bash");
    wait(Duration::from_secs(5), || {
        state_of(&fixture.fleet_dir).is_some_and(|state| state.pending_requests.len() == 1)
    })
    .await;
    let request_id = pending.request_id.clone();

    // a console that dies mid-question leaves the request for the next one
    first.stop();
    let next = fixture.client(false);
    let mut rx2 = next.subscribe();
    next.start().unwrap();
    let again = wait_event(&mut rx2, WAIT, |event| {
        matches!(event, ClientEvent::PermissionRequest(req) if req.request_id == request_id)
    })
    .await;
    let ClientEvent::PermissionRequest(again) = again else {
        unreachable!()
    };
    assert_eq!(again.request_id, request_id, "the same question, re-asked");

    next.allow(&again.request_id, None).await.unwrap();
    wait(WAIT, || {
        transcript_of(&fixture.fleet_dir)
            .iter()
            .any(|record| record.kind == "result")
    })
    .await;
    wait(Duration::from_secs(5), || {
        state_of(&fixture.fleet_dir).is_some_and(|state| state.pending_requests.is_empty())
    })
    .await;
    next.stop();
    stop_monitor(&fixture.fleet_dir).await;
}

#[tokio::test]
async fn a_restarted_monitor_resumes_the_session_and_fresh_starts_over() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fixture = Fixture::new("sess-restore1");
    let first = fixture.client(true);
    first.start().unwrap();
    wait(WAIT, || first.running()).await;
    first.send("remember this").await.unwrap();
    wait(WAIT, || {
        transcript_of(&fixture.fleet_dir)
            .iter()
            .any(|record| record.kind == "result")
    })
    .await;
    first.stop();

    // the monitor dies (a reboot, a kill): the console must still come back
    // to the same conversation
    stop_monitor(&fixture.fleet_dir).await;

    let second = fixture.client(false);
    assert!(
        !second.start().unwrap(),
        "there was nothing alive to attach to"
    );
    wait(WAIT, || second.running()).await;
    // the transcript says where the seam is, and claude resumes the session
    wait(Duration::from_secs(10), || {
        transcript_of(&fixture.fleet_dir).iter().any(|record| {
            record.kind == "notice"
                && record
                    .body
                    .get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.contains("resumed the orchestrator session"))
        })
    })
    .await;
    wait(WAIT, || {
        std::path::Path::new(&fixture.cwd.join("argv.json")).is_file()
            && argv_of(&fixture.cwd).contains(&"--resume".to_string())
    })
    .await;
    let argv = argv_of(&fixture.cwd);
    let resume_at = argv.iter().position(|a| a == "--resume").expect("--resume");
    assert_eq!(argv[resume_at + 1], "sess-restore1");
    second.stop();
    stop_monitor(&fixture.fleet_dir).await;

    // --fresh is the way to start over: no --resume, empty transcript
    let third = fixture.client(true);
    let mut rx3 = third.subscribe();
    third.start().unwrap();
    wait(WAIT, || third.running()).await;
    wait(Duration::from_secs(10), || {
        transcript_of(&fixture.fleet_dir)
            .iter()
            .any(|record| record.kind == "notice")
    })
    .await;
    wait(Duration::from_secs(5), || rx3.try_recv().is_ok()).await;
    assert!(
        !transcript_of(&fixture.fleet_dir)
            .iter()
            .any(|record| record.kind == "notice"
                && record
                    .body
                    .get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.contains("resumed"))),
        "a fresh session does not resume"
    );
    wait(Duration::from_secs(10), || {
        std::path::Path::new(&fixture.cwd.join("argv.json")).is_file()
            && !argv_of(&fixture.cwd).contains(&"--resume".to_string())
    })
    .await;
    third.stop();
    stop_monitor(&fixture.fleet_dir).await;
}

#[tokio::test]
async fn the_model_command_switches_the_session_and_shutdown_ends_the_monitor() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fixture = Fixture::new("sess-model01");
    let client = fixture.client(true);
    client.start().unwrap();
    wait(WAIT, || client.running()).await;
    client.send("hello").await.unwrap();
    wait(WAIT, || count_results(&fixture.fleet_dir) >= 1).await;
    let before = state_of(&fixture.fleet_dir).unwrap();
    assert_eq!(before.model.as_deref(), Some("fake-model"));

    // a live model switch: claude's receipt lands in the state
    client.set_model("fable").await.unwrap();
    wait(Duration::from_secs(10), || {
        state_of(&fixture.fleet_dir).is_some_and(|state| state.model.as_deref() == Some("fable"))
    })
    .await;

    // shutdown ends the orchestrator for good, and the state records it
    let pid = monitor_pid(&fixture.fleet_dir).unwrap();
    client.shutdown().await.unwrap();
    wait(WAIT, || !is_alive(Some(pid))).await;
    let state = state_of(&fixture.fleet_dir).unwrap();
    assert!(state.exited.is_some(), "the state records that it is gone");
    assert_eq!(state.pid, None);
    assert!(
        !client.running(),
        "the console no longer has a live session"
    );
    client.stop();

    // the next console starts a new one rather than attaching to a corpse
    let after = fixture.client(false);
    assert!(
        !after.start().unwrap(),
        "a new console starts its own session"
    );
    wait(WAIT, || after.running()).await;
    after.stop();
    stop_monitor(&fixture.fleet_dir).await;
}

/// Capabilities are asked for, not snapshotted: a skill installed after the
/// handshake shows up on the next refresh, and `state.json` never carries
/// the command list at all.
#[tokio::test]
async fn refreshing_capabilities_picks_up_a_command_installed_later() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fixture = Fixture::new("sess-caps01");
    let client = fixture.client(true);
    let mut rx = client.subscribe();
    client.start().unwrap();
    wait(WAIT, || client.running()).await;
    client.send("hello").await.unwrap();
    wait_event(
        &mut rx,
        WAIT,
        |event| matches!(event, ClientEvent::Record(record) if record.kind == "result"),
    )
    .await;

    // The handshake's answer, and nothing about it in state.json.
    wait(WAIT, || !caps_of(&fixture.fleet_dir).commands.is_empty()).await;
    let first = caps_of(&fixture.fleet_dir);
    let names: Vec<&str> = first.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["model", "usage", "research"]);
    assert!(
        first.tools.iter().any(|t| t == "Bash"),
        "init's tool list is kept, not dropped: {:?}",
        first.tools
    );
    let raw = std::fs::read_to_string(
        FleetPaths::new(&fixture.fleet_dir).orchestrator_state(&session_key(&fixture.fleet_dir)),
    )
    .unwrap();
    assert!(
        !raw.contains("\"commands\""),
        "state.json no longer snapshots the command list: {raw}"
    );

    // Ask again; the fake answers the second initialize with one more command.
    client.refresh_capabilities().await.unwrap();
    wait(WAIT, || {
        caps_of(&fixture.fleet_dir)
            .commands
            .iter()
            .any(|c| c.name == "late-skill")
    })
    .await;

    client.shutdown().await.unwrap();
}

/// A long session keeps itself small: the monitor compacts claude's context
/// once the turn threshold is crossed, and caps the transcript file.
#[tokio::test]
async fn a_session_past_its_turn_threshold_compacts_itself() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fixture = Fixture::new("sess-compact1");
    let home = fixture.cwd.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[session]\nauto_compact_turns = 1\n",
    )
    .unwrap();
    let stdin_log = fixture.cwd.join("stdin.log");
    let mut extra = HashMap::new();
    extra.insert(
        "FAKE_CLAUDE_STDIN_LOG".to_string(),
        stdin_log.to_string_lossy().into_owned(),
    );

    let client = fixture.client_with(true, extra, Some(&home));
    let mut rx = client.subscribe();
    client.start().unwrap();
    wait(WAIT, || client.running()).await;
    client.send("first").await.unwrap();
    wait_event(
        &mut rx,
        WAIT,
        |event| matches!(event, ClientEvent::Record(record) if record.kind == "result"),
    )
    .await;

    wait(WAIT, || {
        std::fs::read_to_string(&stdin_log).is_ok_and(|log| log.contains("/compact"))
    })
    .await;
    assert!(
        transcript_of(&fixture.fleet_dir)
            .iter()
            .any(|record| record.kind == "notice"
                && format!("{:?}", record.body).contains("compacting")),
        "the seam is marked in the transcript"
    );

    client.shutdown().await.unwrap();
}
