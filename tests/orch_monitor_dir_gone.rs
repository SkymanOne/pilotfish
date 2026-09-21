//! The orchestrator monitor must not outlive its fleet directory: deleting
//! `.parl` under a running monitor ends the monitor *and* its claude child,
//! instead of leaving another orphaned process polling against a directory
//! that no longer exists (sixteen of those were once reaped by hand after a
//! deleted worktree). Driven against the real `parl orchestrator-monitor`
//! binary and the scripted claude stand-in, like `tests/orch_monitor.rs`.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parl::fleet::run::is_alive;
use parl::orch::client::{OrchestratorClient, OrchestratorClientOptions};
use parl::orch::monitor::load_orchestrator_state;
use parl::orch::records::EventRecord;
use parl::paths::FleetPaths;

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

/// One fleet dir with the fake-claude environment wired for the monitor,
/// exactly as `tests/orch_monitor.rs` sets it up.
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

    fn client(&self, fresh: bool) -> Arc<OrchestratorClient> {
        let mut env: HashMap<String, String> = HashMap::new();
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
        OrchestratorClient::new(OrchestratorClientOptions {
            fresh,
            monitor_bin: Some(assert_cmd::cargo_bin!("parl").to_path_buf()),
            monitor_env: Some(env),
            poll_ms: 30,
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

/// The session row, or `None` before the monitor has written one — the
/// polling helpers below run from the moment the monitor starts, so a
/// missing row is "not yet", not a failure.
fn session_key(fleet_dir: &Path) -> Option<parl::paths::SessionKey> {
    Some(parl::orch::session::resolve_session(fleet_dir)?.key())
}

fn monitor_pid(fleet_dir: &Path) -> Option<i32> {
    load_orchestrator_state(fleet_dir, &session_key(fleet_dir)?).and_then(|state| state.pid)
}

fn count_results(fleet_dir: &Path) -> usize {
    let Some(key) = session_key(fleet_dir) else {
        return 0;
    };
    let raw = std::fs::read_to_string(FleetPaths::new(fleet_dir).orchestrator_events(&key))
        .unwrap_or_default();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<EventRecord>(line).ok())
        .filter(|record| record.kind == "result")
        .count()
}

/// The pid of the running fake claude, from the spawn line the monitor's
/// process wrote into `claude.log` (`[ts] spawn pid=1234 node …`). Read
/// before the fleet directory is deleted, since the log goes with it.
fn claude_pid(fleet_dir: &Path) -> Option<i32> {
    let log =
        std::fs::read_to_string(FleetPaths::new(fleet_dir).claude_log(&session_key(fleet_dir)?))
            .ok()?;
    let line = log
        .lines()
        .rfind(|l| l.contains("spawn pid=") && l.contains("fake-claude"))?;
    line.split("spawn pid=")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[tokio::test]
async fn deleting_the_fleet_directory_ends_the_monitor_and_its_claude_child() {
    if !node_available() {
        eprintln!("skipping: node is not available");
        return;
    }
    let fixture = Fixture::new("sess-dirgone1");
    let client = fixture.client(true);
    client.start().unwrap();
    wait(WAIT, || client.running()).await;
    // one full turn, so the child is known to be spawned, alive, and mid-session
    client.send("hello").await.unwrap();
    wait(WAIT, || count_results(&fixture.fleet_dir) >= 1).await;

    wait(WAIT, || monitor_pid(&fixture.fleet_dir).is_some()).await;
    let monitor_pid = monitor_pid(&fixture.fleet_dir).expect("a monitor is running");
    wait(WAIT, || claude_pid(&fixture.fleet_dir).is_some()).await;
    let claude_pid = claude_pid(&fixture.fleet_dir).expect("the fake claude spawn line");
    wait(Duration::from_secs(5), || is_alive(Some(claude_pid))).await;

    // Delete the fleet directory out from under the running monitor. The
    // monitor is still writing into it, and a file created between the walk
    // and the rmdir makes the removal fail with ENOTEMPTY — retry until the
    // directory is actually gone, which is the state the test is about.
    let deadline = Instant::now() + WAIT;
    while fixture.fleet_dir.exists() {
        if std::fs::remove_dir_all(&fixture.fleet_dir).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "could not remove the fleet directory"
        );
        tokio::time::sleep(POLL).await;
    }

    // the monitor notices and exits within the bound …
    wait(WAIT, || !is_alive(Some(monitor_pid))).await;
    // … and takes the claude child with it instead of orphaning it
    wait(WAIT, || !is_alive(Some(claude_pid))).await;

    client.stop();
}
