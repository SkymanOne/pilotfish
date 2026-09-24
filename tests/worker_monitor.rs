#![allow(clippy::unwrap_used)]

//! Port of the TypeScript monitor suites (`tests/monitor.test.ts`,
//! `monitor-control.test.ts`, `monitor-outbox.test.ts` and the monitor-facing
//! parts of `fleet-extension.test.ts`) to Rust, driving the real `pilotfish
//! monitor` binary against a scripted fake `pi --mode rpc`
//! (`tests/fixtures/fake-pi-pilotfish.mjs`). Hermetic: no network, no tokens.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pilotfish::fleet::envelope::{Envelope, Party};
use pilotfish::fleet::run::{self, DerivedView, RunState, RunStatus, derive_view};
use pilotfish::paths::FleetPaths;
use pilotfish::util::now_ms;
use serde_json::Value;

/// Settle/terminal statuses, as in the TypeScript helpers.
const TERMINAL: [RunStatus; 5] = [
    RunStatus::Settled,
    RunStatus::Stopped,
    RunStatus::Error,
    RunStatus::Dead,
    RunStatus::Archived,
];

const POLL_MS: u64 = 100;

fn fake_pi() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-pi-pilotfish.mjs")
}

/// One fleet with one prepared run; the monitor is started against it and
/// killed when the harness drops.
struct Fleet {
    tmp: tempfile::TempDir,
    pilotfish_dir: PathBuf,
    run_id: String,
    run_dir: PathBuf,
    monitor: Option<Child>,
}

impl Fleet {
    fn new(prefix: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let pilotfish_dir = tmp.path().join(".pilotfish");
        let run_id = format!("{prefix}20260828141530");
        let run_dir = pilotfish_dir.join("runs").join(&run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        Self {
            tmp,
            pilotfish_dir,
            run_id,
            run_dir,
            monitor: None,
        }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    /// Prepare `run.json`, optionally tweaked per test (must precede
    /// `spawn_monitor`, which reads it at boot).
    fn write_state_with(&self, tweak: impl FnOnce(&mut RunState)) {
        let mut state = RunState::new(
            self.pilotfish_dir.to_str().unwrap(),
            &self.run_id,
            "w",
            self.root().to_str().unwrap(),
            "t",
            None,
            None,
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
        tweak(&mut state);
        run::save_state(&self.run_dir, &state).unwrap();
    }

    fn write_state(&self) {
        self.write_state_with(|_| {});
    }

    fn state(&self) -> RunState {
        run::load_state(&self.run_dir).unwrap()
    }

    /// The worker party this run is addressed as: its recorded uuid.
    fn worker_party(&self) -> Party {
        Party::Worker(run::load_state(&self.run_dir).unwrap().uuid)
    }

    /// The default orchestrator party, as CLI/MCP steering writes it.
    fn orch_party(&self) -> Party {
        Party::Orchestrator(pilotfish::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION)
    }

    /// Poll `run.json` until `check` holds or the timeout lapses.
    fn wait_state(&self, timeout: Duration, check: impl Fn(&RunState) -> bool) -> RunState {
        let deadline = Instant::now() + timeout;
        loop {
            let state = self.state();
            if check(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting on run.json; last status {:?} error {:?}\nevents: {}\npi.log tail: {}",
                state.status,
                state.error,
                self.read("events.jsonl"),
                tail(&self.run_dir.join("pi.log"), 30)
            );
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    }

    fn events(&self) -> Vec<Value> {
        self.read("events.jsonl")
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn has_event(&self, check: impl Fn(&Value) -> bool) -> bool {
        self.events().iter().any(check)
    }

    /// Poll `events.jsonl` until an event matches, returning it.
    fn wait_event(&self, timeout: Duration, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(ev) = self.events().iter().find(|ev| check(ev)) {
                return ev.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for an event; have: {}",
                self.read("events.jsonl")
            );
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.run_dir.join(name)).unwrap_or_default()
    }

    fn append_inbox(&self, envelope: &Envelope) {
        pilotfish::fleet::envelope::append_envelope(&self.run_dir.join("inbox.jsonl"), envelope)
            .unwrap();
    }

    /// Start `pilotfish monitor` against this fleet with the fake pi binary.
    fn spawn_monitor(&mut self, extra_env: &[(&str, &str)]) {
        assert!(self.monitor.is_none(), "monitor already running");
        let mut command = Command::new(assert_cmd::cargo_bin!("pilotfish"));
        command
            .args(["monitor", "--fleet-dir"])
            .arg(&self.pilotfish_dir)
            .args(["--run", &self.run_id])
            .env("PILOTFISH_PI_BIN", format!("node {}", fake_pi().display()))
            // The child must never inherit an ambient PILOTFISH_DIR: this
            // monitor's fleet is the one this test created.
            .env("PILOTFISH_DIR", &self.pilotfish_dir)
            .stdin(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        self.monitor = Some(command.spawn().unwrap());
    }

    fn wait_monitor_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(child) = self.monitor.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                return status.code();
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    }

    fn stop_monitor(&mut self) {
        if let Some(child) = self.monitor.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.monitor = None;
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.stop_monitor();
    }
}

/// The fleet-level pi catalogue, as the monitor wrote it.
fn read_cache(pilotfish_dir: &Path) -> pilotfish::fleet::run::PiCache {
    let raw = std::fs::read_to_string(pilotfish_dir.join("pi-cache.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn tail(path: &Path, n: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(4000);
    let text = String::from_utf8_lossy(&bytes[start..]);
    let lines: Vec<&str> = text.lines().collect();
    let begin = lines.len().saturating_sub(n);
    lines[begin..].join("\n")
}

fn settled_or(fleet: &Fleet, timeout: Duration) -> RunState {
    fleet.wait_state(timeout, |state| TERMINAL.contains(&state.status))
}

/// Spawn a slow run and wait until the fake pi is visibly mid-turn.
fn spawn_slow(prefix: &str, extra_env: &[(&str, &str)]) -> Fleet {
    let mut fleet = Fleet::new(prefix);
    fleet.write_state();
    fleet.spawn_monitor(extra_env);
    fleet.wait_state(Duration::from_secs(15), |state| {
        state.status == RunStatus::Running
    });
    fleet.wait_event(Duration::from_secs(15), |ev| {
        ev["type"] == "tool_execution_end"
    });
    fleet
}

// ---------------------------------------------------------------------------
// monitor.test.ts

#[test]
fn run_lifecycle() {
    {
        let mut fleet = Fleet::new("pf-mon-");
        fleet.write_state();
        fleet.spawn_monitor(&[("FAKE_PI_DELAY_MS", "300"), ("FAKE_PI_WRITE_HELLO", "1")]);
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Settled);
        // `last_assistant_text` is fetched after the settle flush: the monitor
        // writes the terminal status first and the text lands with a later
        // flush, so wait for the flush that carries it instead of racing it.
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            state.last_assistant_text.is_some()
        });
        assert_eq!(
            state.last_assistant_text.as_deref(),
            Some("Working: wrote hello.txt")
        );
        assert_eq!(state.last_tool.as_deref(), Some("bash"));
        assert!(state.last_activity.is_some(), "last activity was recorded");
        assert!(state.settled_at.is_some(), "the settle time was recorded");
        assert_eq!(state.error, None);
        assert!(state.pid.is_some(), "the monitor pid is recorded");

        // The report lives in the new layout: runs/<id>/report.md.
        assert!(
            fleet.run_dir.join("report.md").is_file(),
            "the report lives at runs/<id>/report.md"
        );

        let events = fleet.read("events.jsonl");
        assert!(events.contains("\"task_prompt\""), "{events}");
        assert!(events.contains("\"tool_execution_end\""), "{events}");
        assert!(events.contains("\"text_end\""), "{events}");
        assert!(
            !events.contains("\"turn_start\""),
            "unselected events are not captured"
        );

        // pi.log keeps every raw line, monitor diagnostics included.
        let pi_log = fleet.read("pi.log");
        assert!(pi_log.contains("\"agent_settled\""), "{pi_log}");
        assert!(pi_log.contains("\"turn_start\""), "{pi_log}");
        assert!(pi_log.contains("[monitor] supervising run"), "{pi_log}");

        // The monitor shuts pi down and exits after settling.
        let code = fleet.wait_monitor_exit(Duration::from_secs(15));
        assert_eq!(code, Some(0), "monitor exits cleanly");
    }
    {
        let mut fleet = Fleet::new("pf-err-");
        fleet.write_state();
        let fail_pi = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fail-pi.mjs");
        let mut command = Command::new(assert_cmd::cargo_bin!("pilotfish"));
        command
            .args(["monitor", "--fleet-dir"])
            .arg(&fleet.pilotfish_dir)
            .args(["--run", &fleet.run_id])
            .env("PILOTFISH_PI_BIN", format!("node {}", fail_pi.display()))
            .env("PILOTFISH_DIR", &fleet.pilotfish_dir)
            .stdin(Stdio::null());
        fleet.monitor = Some(command.spawn().unwrap());
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Error);
        let error = state.error.unwrap_or_default();
        assert!(error.contains("exited with code 1"), "{error}");
        assert!(
            error.contains("model provider unreachable"),
            "stderr tail captured: {error}"
        );
        assert!(
            state.settled_at.is_some(),
            "the failure was recorded as settled"
        );
        let failure = fleet.wait_event(Duration::from_secs(10), |ev| ev["type"] == "run_failed");
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .contains("model provider unreachable")
        );
    }
    {
        let mut fleet = Fleet::new("pf-nopi-");
        fleet.write_state();
        let mut command = Command::new(assert_cmd::cargo_bin!("pilotfish"));
        command
            .args(["monitor", "--fleet-dir"])
            .arg(&fleet.pilotfish_dir)
            .args(["--run", &fleet.run_id])
            .env("PILOTFISH_PI_BIN", "/nonexistent/pi-binary")
            .env("PILOTFISH_DIR", &fleet.pilotfish_dir)
            .stdin(Stdio::null());
        fleet.monitor = Some(command.spawn().unwrap());
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Error);
        assert!(
            state
                .error
                .unwrap_or_default()
                .contains("failed to start pi"),
            "{:?}",
            fleet.state().error
        );
    }
    {
        let mut fleet = Fleet::new("pf-pilog-");
        fleet.write_state();
        let mut command = Command::new(assert_cmd::cargo_bin!("pilotfish"));
        command
            .args(["monitor", "--fleet-dir"])
            .arg(&fleet.pilotfish_dir)
            .args(["--run", &fleet.run_id])
            .env("PILOTFISH_PI_BIN", "/nonexistent/pi-binary")
            .env("PILOTFISH_DIR", &fleet.pilotfish_dir)
            .stdin(Stdio::null());
        fleet.monitor = Some(command.spawn().unwrap());
        settled_or(&fleet, Duration::from_secs(30));
        let pi_log = fleet.read("pi.log");
        assert!(pi_log.contains("[monitor] failed to start pi"), "{pi_log}");
    }
    {
        let fleet = Fleet::new("pf-json-");
        fleet.write_state();
        let mut state = fleet.state();
        state.available_models = vec![pilotfish::fleet::run::WorkerModel {
            provider: "fakeprovider".into(),
            id: "glm-5.3".into(),
            name: Some("GLM 5.3".into()),
            thinking_levels: Vec::new(),
            context_window: None,
            cost: None,
        }];
        state.pending_dialog = Some(pilotfish::fleet::run::PendingDialog {
            id: "u1".into(),
            method: "select".into(),
            question: "Pick one".into(),
            options: Some(vec!["a".into()]),
            context: None,
            asked_at: pilotfish::util::now_iso(),
        });
        run::save_state(&fleet.run_dir, &state).unwrap();
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(fleet.run_dir.join("run.json")).unwrap())
                .unwrap();
        // The catalogue never lands in run.json; run facts still do.
        assert!(raw.get("availableModels").is_none(), "{raw}");
        assert_eq!(raw["pendingDialog"]["method"], "select");
        // Round-trips through the tolerant reader; without a fleet cache the
        // catalogue reads empty.
        let loaded = run::load_state(&fleet.run_dir).unwrap();
        assert!(loaded.available_models.is_empty());
        assert_eq!(loaded.pending_dialog.unwrap().method, "select");
        // With the fleet cache present, loading sources the catalogue from it.
        let cache = pilotfish::fleet::run::PiCache {
            available_models: vec![pilotfish::fleet::run::WorkerModel {
                provider: "fakeprovider".into(),
                id: "glm-5.3".into(),
                name: Some("GLM 5.3".into()),
                thinking_levels: Vec::new(),
                context_window: None,
                cost: None,
            }],
            commands: Vec::new(),
            ..pilotfish::fleet::run::PiCache::default()
        };
        run::write_pi_cache(&fleet.pilotfish_dir, &cache).unwrap();
        let loaded = run::load_state(&fleet.run_dir).unwrap();
        assert_eq!(loaded.available_models.len(), 1);
        assert_eq!(loaded.available_models[0].id, "glm-5.3");
    }
}

#[test]
fn errored_turn_reads_error() {
    {
        // The last turn before settling errored (e.g. a 402): the run must
        // read as error, not settled.
        let mut fleet = Fleet::new("pf-turnerr-");
        fleet.write_state();
        fleet.spawn_monitor(&[("FAKE_PI_TURN_ERROR", "1"), ("FAKE_PI_DELAY_MS", "100")]);
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Error);
        let error = state.error.unwrap_or_default();
        assert!(
            error.contains("in_flight_budget_exhausted"),
            "the turn's errorMessage is recorded: {error}"
        );
        assert!(
            state.settled_at.is_some(),
            "an errored settle is still stamped"
        );
        let code = fleet.wait_monitor_exit(Duration::from_secs(15));
        assert_eq!(code, Some(0), "monitor exits cleanly");
    }
    {
        // pi retries on its own: an errored turn followed by a successful
        // turn settles normally.
        let mut fleet = Fleet::new("pf-turnretry-");
        fleet.write_state();
        fleet.spawn_monitor(&[
            ("FAKE_PI_RETRY_AFTER_ERROR", "1"),
            ("FAKE_PI_DELAY_MS", "100"),
        ]);
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Settled);
        assert_eq!(state.error, None);
    }
}

#[test]
fn commands_and_activity() {
    {
        let mut fleet = spawn_slow("pf-cmds-", &[("FAKE_PI_DELAY_MS", "20000")]);
        let state = fleet.wait_state(Duration::from_secs(20), |state| !state.commands.is_empty());
        let names: Vec<&str> = state.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["skill:fleet-worker-report", "compact-notes", "session-name"]
        );
        assert_eq!(state.commands[0].source, "skill");
        // The fleet cache carries them, and the per-run file does not.
        let cache = read_cache(&fleet.pilotfish_dir);
        let names: Vec<&str> = cache.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["skill:fleet-worker-report", "compact-notes", "session-name"]
        );
        let run_raw = fleet.read("run.json");
        assert!(!run_raw.contains("\"commands\""), "{run_raw}");

        // Asked for, not snapshotted: a skill installed after boot shows up on
        // the next refresh, and the catalogue records when pi last answered.
        assert!(!cache.fetched_at.is_empty(), "the answer is stamped");
        fleet.append_inbox(&Envelope::refresh_capabilities(
            Party::Console,
            fleet.worker_party(),
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let refreshed = read_cache(&fleet.pilotfish_dir);
            if refreshed.commands.iter().any(|c| c.name == "late-skill") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the refreshed catalogue never arrived: {:?}",
                refreshed.commands
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        fleet.append_inbox(&Envelope::command(
            Party::Console,
            fleet.worker_party(),
            "/session-name mine",
        ));
        // The `command_delivered` event is written before the steering record is
        // flushed to run.json (the monitor flushes state on a timer), so wait for
        // the state — once it shows the steering, the event is already on disk.
        fleet.wait_state(Duration::from_secs(15), |state| {
            state.steer_count == 1
                && state
                    .steering_log
                    .first()
                    .is_some_and(|s| s.message == "command: /session-name mine")
        });
        assert!(
            fleet.has_event(|ev| ev["type"] == "command_delivered"),
            "a command_delivered event was written"
        );

        fleet.append_inbox(&Envelope::abort(Party::Console, fleet.worker_party()));
        settled_or(&fleet, Duration::from_secs(20));
        assert_eq!(fleet.wait_monitor_exit(Duration::from_secs(15)), Some(0));
    }
    {
        let mut fleet = Fleet::new("pf-activity-");
        fleet.write_state();
        fleet.spawn_monitor(&[("FAKE_PI_THINK_MS", "1500"), ("FAKE_PI_DELAY_MS", "1500")]);
        let mut seen = std::collections::HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            let state = fleet.state();
            seen.insert(format!(
                "{}:{}",
                state.status,
                state
                    .activity
                    .map(|a| format!("{a:?}").to_lowercase())
                    .unwrap_or_default()
            ));
            if TERMINAL.contains(&state.status) {
                break;
            }
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
        assert!(
            seen.contains("running:thinking"),
            "expected a thinking phase, saw {seen:?}"
        );
        assert!(
            seen.contains("running:tool") || seen.contains("running:text"),
            "and then work, saw {seen:?}"
        );
        assert_eq!(
            fleet.state().activity,
            None,
            "a finished worker is doing nothing"
        );
    }
    {
        let mut fleet = Fleet::new("pf-ext-");
        // The spawn flags ride on run.json; set the user-facing ones.
        fleet.write_state_with(|state| {
            state.model = Some("glm-5.3".into());
            state.thinking = Some("high".into());
            state.skill = Some("/extra/skill".into());
            state.session_arg = Some("abc123".into());
        });
        let argv_file = fleet.root().join("argv.json");
        fleet.spawn_monitor(&[
            ("FAKE_PI_ARGV_FILE", argv_file.to_str().unwrap()),
            ("FAKE_PI_DELAY_MS", "300"),
        ]);
        settled_or(&fleet, Duration::from_secs(30));
        let argv: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(&argv_file).unwrap()).unwrap();
        assert_eq!(&argv[..2], &["--mode".to_string(), "rpc".to_string()]);
        let paths = FleetPaths::new(&fleet.pilotfish_dir);
        // The extension and skill were materialized into the fleet dir.
        let extension = paths.pi_extension();
        let skill = paths.pi_skill();
        assert!(extension.is_file(), "{extension:?}");
        assert!(skill.is_file(), "{skill:?}");
        assert_eq!(
            argv.iter()
                .position(|a| a == "--extension")
                .map(|at| argv[at + 1].as_str()),
            Some(extension.to_str().unwrap())
        );
        assert_eq!(
            argv.iter()
                .position(|a| a == "--skill")
                .map(|at| argv[at + 1].as_str()),
            Some(skill.to_str().unwrap())
        );
        // The user's extra --skill still arrives, after the worker skill.
        let last_skill_flag = argv.iter().rposition(|a| a == "--skill").unwrap();
        assert_eq!(argv[last_skill_flag + 1], "/extra/skill");
        let pair = |flag: &str| {
            let at = argv.iter().rposition(|a| a == flag).unwrap();
            argv[at + 1].as_str()
        };
        assert_eq!(pair("--session"), "abc123");
        assert_eq!(pair("--model"), "glm-5.3");
        assert_eq!(pair("--thinking"), "high");
        // The materialized skill keeps the report-skill frontmatter and the template.
        let skill_md = std::fs::read_to_string(&skill).unwrap();
        assert!(
            skill_md.starts_with("---\nname: fleet-worker-report\n"),
            "the skill keeps its frontmatter"
        );
        assert!(
            skill_md.contains("## Steering received"),
            "the skill keeps the report template"
        );
        assert!(
            skill_md.contains("$PILOTFISH_DIR/runs/$PILOTFISH_RUN/report.md"),
            "the skill targets the PILOTFISH layout"
        );
        // The extension speaks the PILOTFISH layout.
        let extension_ts = std::fs::read_to_string(&extension).unwrap();
        assert!(extension_ts.contains("PILOTFISH_RUN"), "env names");
        assert!(!extension_ts.contains("PI_FLEET"), "old env names are gone");
        assert!(
            extension_ts.contains("runs/${runId}/report.md"),
            "{extension_ts}"
        );
        assert!(!extension_ts.contains("progress.md"), "progress.md is gone");
    }
}

#[test]
fn thinking_and_models() {
    {
        let mut fleet = Fleet::new("pf-think-");
        fleet.write_state();
        fleet.spawn_monitor(&[("FAKE_PI_DELAY_MS", "20000"), ("FAKE_PI_THINKING", "low")]);
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            state.thinking_level.is_some()
        });
        assert_eq!(state.thinking_level.as_deref(), Some("low"));

        fleet.append_inbox(&Envelope::thinking(
            Party::Console,
            fleet.worker_party(),
            "xhigh",
        ));
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            state.thinking_level.as_deref() == Some("xhigh")
        });
        assert_eq!(state.thinking_level.as_deref(), Some("xhigh"));
        assert!(
            fleet.has_event(|ev| ev["type"] == "thinking_requested"),
            "a thinking_requested event was written"
        );

        fleet.append_inbox(&Envelope::thinking(
            Party::Console,
            fleet.worker_party(),
            "ludicrous",
        ));
        fleet.wait_event(Duration::from_secs(20), |ev| {
            ev["type"] == "thinking_rejected"
        });
        let state = fleet.state();
        assert_eq!(
            state.thinking_level.as_deref(),
            Some("xhigh"),
            "a rejected level does not stick"
        );

        fleet.append_inbox(&Envelope::abort(Party::Console, fleet.worker_party()));
        settled_or(&fleet, Duration::from_secs(20));
        assert_eq!(fleet.wait_monitor_exit(Duration::from_secs(15)), Some(0));
    }
    {
        let mut fleet = Fleet::new("pf-think-gap-");
        fleet.write_state();
        fleet.spawn_monitor(&[
            ("FAKE_PI_DELAY_MS", "20000"),
            ("FAKE_PI_THINKING", "xhigh"),
            ("FAKE_PI_THINKING_LEVELS", "off,high,xhigh"),
        ]);
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            !state.available_thinking_levels.is_empty()
        });
        assert_eq!(
            state.available_thinking_levels,
            vec!["off", "high", "xhigh"],
            "what pi says the model has, so the console stops offering the rest"
        );

        fleet.append_inbox(&Envelope::thinking(
            Party::Console,
            fleet.worker_party(),
            "max",
        ));
        fleet.wait_event(Duration::from_secs(20), |ev| {
            ev["type"] == "thinking_unavailable"
        });
        let state = fleet.state();
        assert_eq!(
            state.thinking_level.as_deref(),
            Some("xhigh"),
            "the level pi is actually running at, not the one we asked for"
        );

        // a level it does have still lands, and says nothing extra
        fleet.append_inbox(&Envelope::thinking(
            Party::Console,
            fleet.worker_party(),
            "high",
        ));
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            state.thinking_level.as_deref() == Some("high")
        });
        assert_eq!(state.thinking_level.as_deref(), Some("high"));

        fleet.append_inbox(&Envelope::abort(Party::Console, fleet.worker_party()));
        settled_or(&fleet, Duration::from_secs(20));
        assert_eq!(fleet.wait_monitor_exit(Duration::from_secs(15)), Some(0));
    }
    {
        let mut fleet = Fleet::new("pf-model-");
        fleet.write_state();
        fleet.spawn_monitor(&[
            ("FAKE_PI_MODEL_ID", "vendor/model-9"),
            ("FAKE_PI_PROVIDER", "vendorco"),
        ]);
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            state.active_model.is_some()
        });
        assert_eq!(state.active_model.as_deref(), Some("vendor/model-9"));
        assert_eq!(state.active_provider.as_deref(), Some("vendorco"));
        // The available-model list is a fleet property now: it lands in
        // pi-cache.json (with the fake's provider), not in run.json — the state
        // only reflects it through the load-time cache merge.
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            !state.available_models.is_empty()
        });
        assert!(
            state
                .available_models
                .iter()
                .any(|m| m.id == "glm-5.3-flash" && m.provider == "vendorco")
        );
        let cache = read_cache(&fleet.pilotfish_dir);
        assert!(
            cache
                .available_models
                .iter()
                .any(|m| m.id == "glm-5.3-flash" && m.provider == "vendorco")
        );
        let run_raw = fleet.read("run.json");
        assert!(!run_raw.contains("availableModels"), "{run_raw}");
    }
    {
        let mut fleet = spawn_slow(
            "pf-model-set-",
            &[
                ("FAKE_PI_DELAY_MS", "20000"),
                ("FAKE_PI_ACCEPTS_MODEL", "1"),
            ],
        );
        // Wait for the cached model list, then switch with an explicit provider.
        fleet.wait_state(Duration::from_secs(20), |state| {
            !state.available_models.is_empty()
        });
        fleet.append_inbox(&Envelope::model(
            Party::Console,
            fleet.worker_party(),
            "glm-5.3-flash",
            Some("fakeprovider".into()),
        ));
        fleet.wait_event(Duration::from_secs(15), |ev| {
            ev["type"] == "model_requested"
        });
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            state.active_model.as_deref() == Some("glm-5.3-flash")
        });
        assert_eq!(state.active_provider.as_deref(), Some("fakeprovider"));
        fleet.append_inbox(&Envelope::abort(Party::Console, fleet.worker_party()));
        settled_or(&fleet, Duration::from_secs(20));
        assert_eq!(fleet.wait_monitor_exit(Duration::from_secs(15)), Some(0));
    }
    {
        let fleet = spawn_slow(
            "pf-model-res-",
            &[
                ("FAKE_PI_DELAY_MS", "20000"),
                ("FAKE_PI_ACCEPTS_MODEL", "1"),
            ],
        );
        fleet.wait_state(Duration::from_secs(20), |state| {
            !state.available_models.is_empty()
        });
        fleet.append_inbox(&Envelope::model(
            Party::Console,
            fleet.worker_party(),
            "glm-5.3-flash",
            None,
        ));
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            state.active_model.as_deref() == Some("glm-5.3-flash")
        });
        // The provider came from the cached list, not from the envelope.
        assert_eq!(state.active_provider.as_deref(), Some("fakeprovider"));
        fleet.append_inbox(&Envelope::abort(Party::Console, fleet.worker_party()));
        settled_or(&fleet, Duration::from_secs(20));
    }
    {
        let fleet = spawn_slow(
            "pf-model-bad-",
            &[
                ("FAKE_PI_DELAY_MS", "20000"),
                ("FAKE_PI_ACCEPTS_MODEL", "1"),
            ],
        );
        fleet.wait_state(Duration::from_secs(20), |state| {
            !state.available_models.is_empty()
        });
        fleet.append_inbox(&Envelope::model(
            Party::Console,
            fleet.worker_party(),
            "ghost-model",
            None,
        ));
        let unresolved = fleet.wait_event(Duration::from_secs(15), |ev| {
            ev["type"] == "model_unresolved"
        });
        assert_eq!(unresolved["model"], "ghost-model");
        assert!(
            unresolved["reason"]
                .as_str()
                .unwrap()
                .contains("not in pi's available models")
        );
        // pi was never asked to switch: the resolved model is unchanged.
        assert_eq!(fleet.state().active_model.as_deref(), Some("fake/model-1"));
        assert!(
            !fleet.has_event(|ev| ev["type"] == "model_rejected"),
            "pi was never told to switch"
        );
        fleet.append_inbox(&Envelope::abort(Party::Console, fleet.worker_party()));
        settled_or(&fleet, Duration::from_secs(20));
    }
}

// ---------------------------------------------------------------------------
// monitor-control.test.ts

#[test]
fn steering() {
    {
        let fleet = spawn_slow("pf-steer-", &[("FAKE_PI_DELAY_MS", "4000")]);
        fleet.append_inbox(&Envelope::steer(
            Party::Console,
            fleet.worker_party(),
            "use tabs not spaces",
        ));
        fleet.append_inbox(&Envelope::follow_up(
            fleet.orch_party(),
            fleet.worker_party(),
            "then summarize",
        ));
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Settled);
        assert_eq!(state.steer_count, 2);
        let log: Vec<(&str, &str)> = state
            .steering_log
            .iter()
            .map(|s| (s.source.as_str(), s.message.as_str()))
            .collect();
        assert_eq!(
            log,
            vec![
                ("console", "use tabs not spaces"),
                ("orchestrator", "then summarize")
            ]
        );
        assert!(
            state.steering_log.iter().all(|s| !s.ts.is_empty()),
            "every steering record is timestamped"
        );
        let events = fleet.read("events.jsonl");
        assert!(events.contains("\"steering_delivered\""), "{events}");
        assert!(events.contains("use tabs not spaces"), "{events}");
        let report = fleet.read("report.md");
        assert!(
            report.contains("## Steering received\n- use tabs not spaces"),
            "{report}"
        );
    }
    {
        let fleet = spawn_slow("pf-abort-", &[("FAKE_PI_DELAY_MS", "4000")]);
        fleet.append_inbox(&Envelope::abort(fleet.orch_party(), fleet.worker_party()));
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Stopped);
        assert!(state.settled_at.is_some(), "the stop was recorded");
        assert!(
            fleet.has_event(|ev| ev["type"] == "abort_requested"),
            "an abort_requested event was written"
        );
    }
    {
        let mut fleet = Fleet::new("pf-late-");
        fleet.write_state();
        // pi lingers after settle so the monitor is still polling when the late steer lands
        fleet.spawn_monitor(&[("FAKE_PI_EXIT_DELAY_MS", "3000")]);
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Settled);
        fleet.append_inbox(&Envelope::steer(
            Party::Console,
            fleet.worker_party(),
            "too late",
        ));
        let dropped = fleet.wait_event(Duration::from_secs(10), |ev| {
            ev["type"] == "control_dropped" && ev["reason"] == "run already settled"
        });
        assert_eq!(dropped["control"], "steer");
        let events = fleet.read("events.jsonl");
        assert!(!events.contains("steering_delivered"), "{events}");
        assert_eq!(fleet.state().steer_count, 0);
    }
    {
        let mut fleet = Fleet::new("pf-early-");
        fleet.write_state();
        // Written before the monitor starts; the inbox is read from byte 0.
        fleet.append_inbox(&Envelope::steer(
            fleet.orch_party(),
            fleet.worker_party(),
            "early bird",
        ));
        fleet.spawn_monitor(&[("FAKE_PI_DELAY_MS", "3000")]);
        let state = settled_or(&fleet, Duration::from_secs(30));
        assert_eq!(state.status, RunStatus::Settled);
        assert_eq!(state.steer_count, 1);
        assert_eq!(state.steering_log[0].message, "early bird");
        let report = fleet.read("report.md");
        assert!(report.contains("- early bird"), "{report}");
    }
}

// ---------------------------------------------------------------------------
// monitor-outbox.test.ts

#[test]
fn questions_and_dialogs() {
    {
        let mut fleet = Fleet::new("pf-outbox-1-");
        fleet.write_state();
        fleet.spawn_monitor(&[
            ("FAKE_PI_ASK", "1"),
            ("FAKE_PI_PROGRESS", "1"),
            ("FAKE_PI_DELAY_MS", "200"),
        ]);
        let state = fleet.wait_state(Duration::from_secs(15), |state| {
            state.pending_question.is_some()
        });
        let pending = state.pending_question.unwrap();
        assert_eq!(pending.question, "bcrypt or argon2?");
        assert_eq!(
            pending.options,
            Some(vec!["bcrypt".into(), "argon2".into()])
        );
        assert_eq!(pending.context, None);
        assert!(
            pending.id.starts_with("q_fake_"),
            "the question carries the fake's id: {}",
            pending.id
        );
        assert!(
            pending.asked_at.starts_with('2'),
            "asked_at is an RFC3339 timestamp: {}",
            pending.asked_at
        );
        assert_eq!(state.last_progress.as_deref(), Some("starting the work"));

        // A running worker waiting on its question reads as blocked.
        let view = derive_view(&fleet.state(), |_| true, now_ms());
        assert_eq!(view, DerivedView::Blocked);

        let events = fleet.events();
        let question = events
            .iter()
            .find(|ev| ev["type"] == "worker_question")
            .expect("worker_question mirrored");
        assert_eq!(question["questionId"], pending.id.as_str());
        assert_eq!(question["question"], "bcrypt or argon2?");
        assert!(
            events.iter().any(|ev| ev["type"] == "worker_progress"),
            "a worker_progress event was mirrored"
        );

        fleet.append_inbox(&Envelope::answer(
            fleet.orch_party(),
            fleet.worker_party(),
            "argon2",
            Some(pending.id.clone()),
        ));
        fleet.wait_event(Duration::from_secs(10), |ev| {
            ev["type"] == "answer_delivered"
        });
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            TERMINAL.contains(&state.status)
        });
        assert_eq!(state.status, RunStatus::Settled);
        assert_eq!(state.pending_question, None);
        assert_eq!(state.steer_count, 1);
        assert_eq!(state.steering_log[0].source, "orchestrator");
        assert_eq!(
            state.steering_log[0].message,
            format!("answer({}): argon2", pending.id)
        );
        let events = fleet.events();
        let delivered = events
            .iter()
            .find(|ev| ev["type"] == "answer_delivered")
            .unwrap();
        assert_eq!(delivered["questionId"], pending.id.as_str());
        assert_eq!(delivered["source"], "orchestrator");
        assert_eq!(delivered["message"], "argon2");
        let resolved = events
            .iter()
            .find(|ev| ev["type"] == "worker_question_resolved")
            .unwrap();
        assert_eq!(resolved["questionId"], pending.id.as_str());
        assert_eq!(resolved["how"], "answered");
        let report = fleet.read("report.md");
        assert!(report.contains("Answer received: argon2"), "{report}");
    }
    {
        let mut fleet = Fleet::new("pf-outbox-2-");
        fleet.write_state();
        fleet.spawn_monitor(&[
            ("FAKE_PI_ASK", "1"),
            ("FAKE_PI_ASK_TIMEOUT_MS", "600"),
            ("FAKE_PI_DELAY_MS", "100"),
        ]);
        fleet.wait_state(Duration::from_secs(15), |state| {
            state.pending_question.is_some()
        });
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            TERMINAL.contains(&state.status)
        });
        assert_eq!(state.status, RunStatus::Settled);
        assert_eq!(state.pending_question, None);
        assert_eq!(state.steer_count, 0);
        let resolved = fleet
            .events()
            .into_iter()
            .find(|ev| ev["type"] == "worker_question_resolved")
            .expect("resolved event");
        assert_eq!(resolved["how"], "timeout");
        assert!(
            !fleet.has_event(|ev| ev["type"] == "answer_delivered"),
            "no answer was delivered"
        );
    }
    {
        let mut fleet = Fleet::new("pf-dialog-");
        fleet.write_state();
        fleet.spawn_monitor(&[("FAKE_PI_DIALOG", "1"), ("FAKE_PI_DELAY_MS", "300")]);
        let state = fleet.wait_state(Duration::from_secs(15), |state| {
            state.pending_dialog.is_some()
        });
        let dialog = state.pending_dialog.unwrap();
        assert_eq!(dialog.id, "dlg_fake_1");
        assert_eq!(dialog.method, "select");
        assert_eq!(dialog.question, "Pick one");
        assert_eq!(dialog.options, Some(vec!["a".into(), "b".into()]));
        // Rendered like a pending question.
        assert_eq!(
            derive_view(&fleet.state(), |_| true, now_ms()),
            DerivedView::Blocked
        );
        let request = fleet.wait_event(Duration::from_secs(10), |ev| ev["type"] == "worker_dialog");
        assert_eq!(request["questionId"], "dlg_fake_1");

        fleet.append_inbox(&Envelope::answer(
            fleet.orch_party(),
            fleet.worker_party(),
            "b",
            Some("dlg_fake_1".into()),
        ));
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            TERMINAL.contains(&state.status)
        });
        assert_eq!(
            state.status,
            RunStatus::Settled,
            "the dialog never stalls the worker"
        );
        assert_eq!(state.pending_dialog, None, "answered dialog is cleared");
        // The value reply reached pi and was recorded.
        let report = fleet.read("report.md");
        assert!(report.contains(r#""value":"b""#), "{report}");
        assert!(
            fleet.has_event(|ev| ev["type"] == "answer_delivered"),
            "the answer reached pi"
        );
    }
    {
        let mut fleet = Fleet::new("pf-dialog-t-");
        fleet.write_state();
        fleet.spawn_monitor(&[
            ("FAKE_PI_DIALOG", "1"),
            ("FAKE_PI_DIALOG_TIMEOUT", "1500"),
            ("FAKE_PI_DELAY_MS", "100"),
        ]);
        fleet.wait_state(Duration::from_secs(15), |state| {
            state.pending_dialog.is_some()
        });
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            TERMINAL.contains(&state.status)
        });
        assert_eq!(
            state.status,
            settled_status(),
            "the monitor cancelled instead of hanging"
        );
        assert_eq!(state.pending_dialog, None);
        let report = fleet.read("report.md");
        // pi got the cancellation, not its own timeout resolution.
        assert!(report.contains(r#""cancelled":true"#), "{report}");
        assert!(!report.contains("(pi timeout)"), "{report}");
        assert!(
            fleet.has_event(|ev| ev["type"] == "dialog_cancelled"),
            "a dialog_cancelled event was written"
        );
    }
    {
        let mut fleet = Fleet::new("pf-notify-");
        fleet.write_state();
        fleet.spawn_monitor(&[("FAKE_PI_NOTIFY", "1"), ("FAKE_PI_DELAY_MS", "200")]);
        let state = settled_or(&fleet, Duration::from_secs(20));
        assert_eq!(state.status, settled_status());
        assert_eq!(state.pending_dialog, None, "no reply was expected");
        let events = fleet.read("events.jsonl");
        assert!(events.contains("\"notify\""), "{events}");
        assert!(events.contains("\"setTitle\""), "{events}");
        assert!(
            !fleet.has_event(|ev| ev["type"] == "dialog_cancelled"),
            "no cancellation was needed"
        );
    }
    {
        let mut fleet = Fleet::new("pf-dialog-c-");
        fleet.write_state();
        fleet.spawn_monitor(&[
            ("FAKE_PI_DIALOG", "1"),
            ("FAKE_PI_DIALOG_METHOD", "confirm"),
            ("FAKE_PI_DELAY_MS", "300"),
        ]);
        fleet.wait_state(Duration::from_secs(15), |state| {
            state.pending_dialog.is_some()
        });
        fleet.append_inbox(&Envelope::answer(
            Party::Console,
            fleet.worker_party(),
            "yes",
            Some("dlg_fake_1".into()),
        ));
        let state = fleet.wait_state(Duration::from_secs(20), |state| {
            TERMINAL.contains(&state.status)
        });
        assert_eq!(state.status, settled_status());
        let report = fleet.read("report.md");
        assert!(report.contains(r#""confirmed":true"#), "{report}");
    }
}

// ---------------------------------------------------------------------------
// fleet-extension.test.ts (monitor-facing parts) + the new dialog/model work

const fn settled_status() -> RunStatus {
    RunStatus::Settled
}
