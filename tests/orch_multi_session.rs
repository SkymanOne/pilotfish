//! Two live orchestrator sessions side by side on one fleet: each monitor
//! is pinned to its own session by `--session <uuid>`, reads and writes only
//! its own `orchestrators/<key>/` directory, keeps its own heartbeat, and
//! survives its neighbour's removal. The core guarantee of the
//! multi-session feature — a worker settling in session A never produces a
//! fleet event in session B's transcript — is proven here with two real
//! monitors and two real watchers, against the built `pilotfish`
//! `orchestrator-monitor` binary and the scripted claude stand-in.
//!
//! Also the per-session shutdown primitive (Fix B): removing one session's
//! `orchestrators/<key>/` directory stops exactly that monitor within two
//! polls and leaves the other fully functional.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::process::Child;

use pilotfish::fleet::event::FleetEventKind;
use pilotfish::fleet::run::{self, RunState, is_alive};
use pilotfish::orch::monitor::{append_command, load_orchestrator_state};
use pilotfish::orch::records::{EventRecord, OrchestratorCommand, OrchestratorState};
use pilotfish::orch::session::{self, OrchestratorSession};
use pilotfish::orch::watcher::{FleetWatcher, FleetWatcherOptions};
use pilotfish::paths::{FleetPaths, SessionKey};

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

/// Poll until `check` holds or the timeout lapses, naming the step on a
/// timeout so a failure says exactly which wait was missed.
async fn wait_for(label: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting: {label}");
        tokio::time::sleep(POLL).await;
    }
}

/// One fleet with two sessions and the monitors each session deserves.
struct TwoSessions {
    _tmp: tempfile::TempDir,
    fleet_dir: PathBuf,
    a: OrchestratorSession,
    b: OrchestratorSession,
}

fn build() -> TwoSessions {
    let tmp = tempfile::tempdir().unwrap();
    let fleet_dir = tmp.path().join(".pilotfish");
    std::fs::create_dir_all(&fleet_dir).unwrap();
    let a = session::create_session(&fleet_dir, Some("ms-a")).unwrap();
    let b = session::create_session(&fleet_dir, Some("ms-b")).unwrap();
    TwoSessions {
        _tmp: tmp,
        fleet_dir,
        a,
        b,
    }
}

/// Environment for one monitor's spawn: the scripted claude, a pinned
/// `PILOTFISH_DIR` (test isolation: nothing ambient may leak in), and the fake
/// claude's session identity, distinct per session.
fn monitor_env(
    fleet_dir: &Path,
    fake_session_id: &str,
    argv_file: &Path,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "PILOTFISH_CLAUDE_BIN".to_string(),
        format!("node {}", fake_claude().display()),
    );
    env.insert(
        "PILOTFISH_DIR".to_string(),
        fleet_dir.to_string_lossy().into_owned(),
    );
    env.insert(
        "FAKE_CLAUDE_SESSION_ID".to_string(),
        fake_session_id.to_string(),
    );
    env.insert(
        "FAKE_CLAUDE_ARGV_FILE".to_string(),
        argv_file.to_string_lossy().into_owned(),
    );
    env
}

/// Spawn `pilotfish orchestrator-monitor --fleet-dir <fleet> --session <uuid>`
/// detached, exactly the shape the console uses. stderr goes to a file so
/// a failing run leaves its reason behind.
fn spawn_monitor(
    fleet_dir: &Path,
    session: &OrchestratorSession,
    env: HashMap<String, String>,
    log_file: &Path,
) -> Child {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .unwrap();
    let mut command = tokio::process::Command::new(assert_cmd::cargo_bin!("pilotfish"));
    command
        .args(["orchestrator-monitor", "--fleet-dir"])
        .arg(fleet_dir)
        .args(["--session"])
        .arg(session.uuid.to_string())
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log));
    command.spawn().unwrap()
}

fn state_of(fleet_dir: &Path, key: &SessionKey) -> Option<OrchestratorState> {
    load_orchestrator_state(fleet_dir, key)
}

/// Remove a session directory the monitor is still writing into. A file
/// created between the walk and the rmdir makes the removal fail with
/// ENOTEMPTY, so retry until the directory is actually gone — that state is
/// what the assertions below are about.
fn remove_dir_under_a_live_monitor(dir: &Path) {
    let deadline = Instant::now() + WAIT;
    while dir.exists() {
        if std::fs::remove_dir_all(dir).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "could not remove {}",
            dir.display()
        );
        std::thread::sleep(POLL);
    }
}

fn monitor_pid(fleet_dir: &Path, session: &OrchestratorSession) -> i32 {
    state_of(fleet_dir, &session.key())
        .and_then(|state| state.pid)
        .expect("the monitor wrote its pid")
}

fn count_results(fleet_dir: &Path, key: &SessionKey) -> usize {
    let raw = std::fs::read_to_string(FleetPaths::new(fleet_dir).orchestrator_events(key))
        .unwrap_or_default();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<EventRecord>(line).ok())
        .filter(|record| record.kind == "result")
        .count()
}

fn session_by_uuid(fleet_dir: &Path, session: &OrchestratorSession) -> OrchestratorSession {
    session::session_by_key(fleet_dir, &session.uuid.to_string()).unwrap()
}

/// Wait for a spawned monitor process itself to exit, naming the step and
/// dumping diagnostics when it does not. The child handle is the exact exit
/// signal — immune to pid recycling, which `is_alive(pid)` is not while the
/// suite's parallel tests keep spawning fresh processes that can inherit a
/// dead monitor's pid within seconds.
async fn wait_for_child_exit(
    label: &str,
    timeout: Duration,
    child: &mut Child,
    err_file: &Path,
    removed_dir: &Path,
    fleet_dir: &Path,
) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        match tokio::time::timeout(POLL, child.wait()).await {
            Ok(Ok(status)) => return status,
            Ok(Err(err)) => panic!("waiting on {label} failed: {err}"),
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting: {label} (removed dir exists: {}, fleet dir exists: {}, \
                     stderr tail: {:?})",
                    removed_dir.exists(),
                    fleet_dir.exists(),
                    std::fs::read_to_string(err_file).map(|s| s
                        .lines()
                        .rev()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" | "))
                );
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

/// A fixture run owned by `owner`, running.
fn add_owned_run(fleet_dir: &Path, name: &str, owner: uuid::Uuid) -> (String, PathBuf) {
    let run_id = format!("{name}-20260830000000");
    let run_dir = fleet_dir.join("runs").join(&run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    let mut state = RunState::new(
        fleet_dir.to_string_lossy().as_ref(),
        &run_id,
        name,
        "/repo",
        "brief",
        None,
        Some(format!("pilotfish/{name}-1234567")),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    state.orchestrator_id = Some(owner);
    state.status = run::RunStatus::Running;
    state.pid = Some(i32::try_from(std::process::id()).unwrap_or(1));
    run::save_state(&run_dir, &state).unwrap();
    std::fs::write(run_dir.join("events.jsonl"), "").unwrap();
    (run_id, run_dir)
}

fn settle_run(run_dir: &Path) {
    let mut state = run::load_state(run_dir).unwrap();
    state.status = run::RunStatus::Settled;
    run::save_state(run_dir, &state).unwrap();
}

fn watcher_for(fleet_dir: &Path, owner: uuid::Uuid) -> FleetWatcher {
    FleetWatcher::new(FleetWatcherOptions {
        fleet_dir: fleet_dir.to_path_buf(),
        owner: Some(owner),
        ..FleetWatcherOptions::default()
    })
}

#[tokio::test]
async fn two_live_sessions_are_isolated_and_one_dir_removal_stops_only_its_own_monitor() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fix = build();
    let fleet_dir = &fix.fleet_dir;
    let cwd = fix._tmp.path();

    // Two monitors, each pinned to its own session.
    let mut child_a = spawn_monitor(
        fleet_dir,
        &fix.a,
        monitor_env(fleet_dir, "sess-ms-a", &cwd.join("argv-a.json")),
        &cwd.join("monitor-a.err"),
    );
    let mut child_b = spawn_monitor(
        fleet_dir,
        &fix.b,
        monitor_env(fleet_dir, "sess-ms-b", &cwd.join("argv-b.json")),
        &cwd.join("monitor-b.err"),
    );

    // Both boot: pid on disk, then the fake claude reports its own session id.
    wait_for("a-state", WAIT, || {
        state_of(fleet_dir, &fix.a.key()).is_some()
    })
    .await;
    wait_for("b-state", WAIT, || {
        state_of(fleet_dir, &fix.b.key()).is_some()
    })
    .await;
    append_command(
        fleet_dir,
        &fix.a.key(),
        &OrchestratorCommand::User {
            text: "hello".into(),
        },
    )
    .unwrap();
    append_command(
        fleet_dir,
        &fix.b.key(),
        &OrchestratorCommand::User {
            text: "hello".into(),
        },
    )
    .unwrap();
    wait_for("a-session-id", WAIT, || {
        state_of(fleet_dir, &fix.a.key())
            .and_then(|s| s.session_id)
            .as_deref()
            == Some("sess-ms-a")
    })
    .await;
    wait_for("b-session-id", WAIT, || {
        state_of(fleet_dir, &fix.b.key())
            .and_then(|s| s.session_id)
            .as_deref()
            == Some("sess-ms-b")
    })
    .await;
    wait_for("a-results", WAIT, || {
        count_results(fleet_dir, &fix.a.key()) >= 1
    })
    .await;
    wait_for("b-results", WAIT, || {
        count_results(fleet_dir, &fix.b.key()) >= 1
    })
    .await;
    let pid_a = monitor_pid(fleet_dir, &fix.a);
    let pid_b = monitor_pid(fleet_dir, &fix.b);
    assert!(is_alive(Some(pid_a)) && is_alive(Some(pid_b)));

    // Each monitor writes only its own session's state, and the heartbeat
    // loop keeps both rows fresh (Fix C).
    assert_ne!(
        state_of(fleet_dir, &fix.a.key())
            .unwrap()
            .session_id
            .as_deref(),
        state_of(fleet_dir, &fix.b.key())
            .unwrap()
            .session_id
            .as_deref()
    );
    wait_for("heartbeats", WAIT, || {
        session_by_uuid(fleet_dir, &fix.a).last_heartbeat.is_some()
            && session_by_uuid(fleet_dir, &fix.b).last_heartbeat.is_some()
    })
    .await;

    // -- The core guarantee: a run settling in session B is never news in
    // -- session A's transcript, with both sessions live. -----------------
    let (b_run, b_run_dir) = add_owned_run(fleet_dir, "wb-run", fix.b.uuid);
    let (a_run, a_run_dir) = add_owned_run(fleet_dir, "wa-run", fix.a.uuid);
    let mut watcher_a = watcher_for(fleet_dir, fix.a.uuid);
    let mut watcher_b = watcher_for(fleet_dir, fix.b.uuid);
    watcher_a.start(false);
    watcher_b.start(false);
    watcher_a.tick();
    watcher_b.tick();
    assert!(watcher_a.take_batch().is_empty());
    assert!(watcher_b.take_batch().is_empty());

    settle_run(&b_run_dir);
    watcher_a.tick();
    watcher_a.tick();
    assert!(
        watcher_a.take_batch().is_empty(),
        "a worker settling in session B must never produce a fleet event in session A's transcript"
    );
    watcher_b.tick();
    let events = watcher_b.take_batch();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, FleetEventKind::Settled);
    assert_eq!(events[0].run_id, b_run);

    settle_run(&a_run_dir);
    watcher_a.tick();
    let events = watcher_a.take_batch();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, FleetEventKind::Settled);
    assert_eq!(events[0].run_id, a_run);
    watcher_b.tick();
    assert!(watcher_b.take_batch().is_empty());

    // -- Fix B: removing session A's directory stops exactly A's monitor --
    let a_key = fix.a.key();
    let a_dir = FleetPaths::new(fleet_dir).orchestrator_dir(&a_key);
    remove_dir_under_a_live_monitor(&a_dir);
    let exit_a = wait_for_child_exit(
        "a-monitor-exit",
        WAIT,
        &mut child_a,
        &cwd.join("monitor-a.err"),
        &a_dir,
        fleet_dir,
    )
    .await;
    assert!(exit_a.success());

    // B never noticed: its monitor is still alive, its state still reads,
    // and it still answers commands — the session is fully functional.
    assert!(is_alive(Some(pid_b)), "session B's monitor keeps running");
    wait_for("b-still-alive", Duration::from_secs(3), || {
        is_alive(Some(pid_b)) && state_of(fleet_dir, &fix.b.key()).is_some()
    })
    .await;
    let results_before = count_results(fleet_dir, &fix.b.key());
    append_command(
        fleet_dir,
        &fix.b.key(),
        &OrchestratorCommand::User {
            text: "still alive".into(),
        },
    )
    .unwrap();
    wait_for("b-results-after", WAIT, || {
        count_results(fleet_dir, &fix.b.key()) > results_before
    })
    .await;
    let b_row = session_by_uuid(fleet_dir, &fix.b);
    assert!(
        b_row.last_heartbeat.is_some(),
        "B's heartbeat continues: {b_row:?}"
    );
    // A's monitor cleared its pid from the row on the way out — the row no
    // longer claims a live monitor.
    wait_for("a-row-cleared", WAIT, || {
        session_by_uuid(fleet_dir, &fix.a).pid.is_none()
    })
    .await;

    // Cleanup: delete B's directory and both monitors are gone for good.
    let b_key = fix.b.key();
    let b_dir = FleetPaths::new(fleet_dir).orchestrator_dir(&b_key);
    remove_dir_under_a_live_monitor(&b_dir);
    let exit_b = wait_for_child_exit(
        "b-monitor-exit",
        WAIT,
        &mut child_b,
        &cwd.join("monitor-b.err"),
        &b_dir,
        fleet_dir,
    )
    .await;
    assert!(exit_b.success());
}
