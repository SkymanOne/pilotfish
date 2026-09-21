#![allow(clippy::unwrap_used)]

//! End-to-end tests driving the built `parl` binary the way a user (or the
//! orchestrator's scripts) invokes it: every subcommand dispatched, the
//! exit-code contract, a full worker lifecycle against the scripted fake pi,
//! the `.parl` layout on disk, and the requirements that travel inside the
//! binary (the orchestrator prompt and the pi worker extension). Hermetic:
//! no real `pi`, no real `claude`, no network, no tokens — everything runs
//! against `tests/fixtures/` fakes, like `mcp_stdio.rs` and
//! `worker_monitor.rs`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command as StdCommand, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};

/// Generous ceilings: three workers build and test on this machine at once,
/// so every wait polls instead of sleeping and every deadline assumes load.
const SETTLE: Duration = Duration::from_secs(40);
const MONITOR_EXIT: Duration = Duration::from_secs(20);

/// Subprocess-heavy tests run one at a time (same reason as `mcp_stdio.rs`).
static SERIAL: Mutex<()> = Mutex::new(());

/// Poll cadence for the polling waits.
const POLL: Duration = Duration::from_millis(150);

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn parl() -> assert_cmd::Command {
    let mut command = assert_cmd::Command::new(assert_cmd::cargo_bin!("parl"));
    command
        // No child inherits an ambient PARL_DIR; the helpers that know the
        // test's own fleet dir pin it explicitly below.
        .env_remove("PARL_DIR")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    command
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn fake_pi() -> PathBuf {
    fixture("fake-pi-parl.mjs")
}

/// pi replacement that dies immediately (the monitor's error path).
fn fail_pi() -> PathBuf {
    fixture("fail-pi.mjs")
}

/// The shared fake plus the session files real pi writes into
/// `--session-dir`, for the documented-layout assertions.
fn session_pi() -> PathBuf {
    fixture("fake-pi-session.mjs")
}

fn fake_claude() -> PathBuf {
    fixture("fake-claude.mjs")
}

/// `PARL_PI_BIN` is an executable spec split on spaces.
fn pi_spec(path: &Path) -> String {
    format!("node {}", path.display())
}

/// A plain (non-git) directory for runs without a worktree.
fn plain_dir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    (tmp, root)
}

/// Run a subcommand and return (exit code, stdout, stderr).
fn run(root: &Path, args: &[&str]) -> (i32, String, String) {
    let output = parl()
        .args(args)
        .current_dir(root)
        // The fleet dir this root resolves to — canonicalized, like the
        // product's own resolution, so path assertions read the same strings.
        .env(
            "PARL_DIR",
            root.canonicalize()
                .unwrap()
                .join(parl::paths::STATE_DIR_NAME),
        )
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) {
    let output = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git repo with one committed seed file and no `.gitignore` yet — spawn
/// adds the `.parl/` entry itself, which the gitignore and conflict tests
/// assert against.
fn init_repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "seed"]);
    (tmp, root)
}

/// Spawn a worker through the real binary with the fake pi; returns the run
/// id parsed from the output's first line (`Spawned <id>`).
fn spawn_ok(
    root: &Path,
    name: &str,
    brief: &str,
    pi: &Path,
    extra_env: &[(&str, &str)],
    flags: &[&str],
) -> String {
    let output = parl()
        .args(["spawn", name])
        .args(flags)
        .args(["--", brief])
        .current_dir(root)
        .env(
            "PARL_DIR",
            root.canonicalize()
                .unwrap()
                .join(parl::paths::STATE_DIR_NAME),
        )
        .env("PARL_PI_BIN", pi_spec(pi))
        .envs(extra_env.iter().copied())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "spawn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    stdout
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Spawned ")
        .unwrap_or_else(|| panic!("unexpected spawn output: {stdout}"))
        .to_string()
}

/// Parsed `parl status <name> --json` (the single-run state object).
fn status_json(root: &Path, name: &str) -> Value {
    let output = parl()
        .args(["status", name, "--json"])
        .current_dir(root)
        .env(
            "PARL_DIR",
            root.canonicalize()
                .unwrap()
                .join(parl::paths::STATE_DIR_NAME),
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "status {name} failed");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("status {name} was not one JSON object: {err}: {stdout}"))
}

/// Poll `parl status <name> --json` until `check` holds or the timeout lapses.
fn poll_status(
    root: &Path,
    name: &str,
    timeout: Duration,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let state = status_json(root, name);
        if check(&state) {
            return state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting on {name}; last state: {state}"
        );
        std::thread::sleep(POLL);
    }
}

/// Wait for any terminal state (what `parl wait` exits on).
fn settled(root: &Path, name: &str) -> Value {
    poll_status(root, name, SETTLE, |state| {
        matches!(
            state["status"].as_str(),
            Some("settled" | "stopped" | "error" | "dead" | "archived")
        )
    })
}

/// `parl wait`'s exit code, for the exit-code matrix.
fn wait_code(root: &Path, name: &str, timeout_secs: u64) -> i32 {
    parl()
        .args(["wait", name, "--timeout", &timeout_secs.to_string()])
        .current_dir(root)
        .env(
            "PARL_DIR",
            root.canonicalize()
                .unwrap()
                .join(parl::paths::STATE_DIR_NAME),
        )
        .output()
        .unwrap()
        .status
        .code()
        .unwrap_or(-1)
}

fn monitor_pid(run_json: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(run_json).ok()?;
    let state: Value = serde_json::from_str(&raw).ok()?;
    state["pid"]
        .as_i64()
        .and_then(|pid| i32::try_from(pid).ok())
}

/// Block until the run's detached monitor is gone, so a test never leaves a
/// stray process behind; a SIGKILL is the last resort. The monitor is
/// orphaned (its parent, `parl spawn`, has exited), so an exited pid is
/// reaped by launchd and `kill(pid, 0)` reads it as gone — checked with the
/// product's own liveness rule.
fn reap_monitor(root: &Path, run_id: &str) {
    let run_json = root
        .canonicalize()
        .unwrap()
        .join(parl::paths::STATE_DIR_NAME)
        .join("runs")
        .join(run_id)
        .join("run.json");
    let deadline = std::time::Instant::now() + MONITOR_EXIT;
    loop {
        let Some(pid) = monitor_pid(&run_json) else {
            return;
        };
        if !parl::fleet::run::is_alive(Some(pid)) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// Safety net for failed assertions: a bounded reap when the test itself
/// did not get there. Declared before the temp dir so it drops (kills)
/// while the tree still exists.
struct ReapOnDrop {
    run_json: PathBuf,
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while let Some(pid) = monitor_pid(&self.run_json) {
            if !parl::fleet::run::is_alive(Some(pid)) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                return;
            }
            std::thread::sleep(POLL);
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Dispatch and the CLI surface.
// ---------------------------------------------------------------------------

#[test]
fn help_and_version_exit_zero_and_hide_the_internal_monitors() {
    let help = parl().arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&help.stdout);
    for public in [
        "spawn", "status", "wait", "output", "logs", "send", "followup", "answer", "stop",
        "report", "diff", "merge", "cleanup", "attach", "mcp",
    ] {
        assert!(
            stdout.contains(public),
            "--help mentions {public}: {stdout}"
        );
    }
    assert!(stdout.contains("Usage: parl"), "{stdout}");
    // The two internal monitors are hidden from the public help…
    assert!(!stdout.contains("orchestrator-monitor"), "{stdout}");
    assert!(!stdout.contains("fleet-dir"), "{stdout}");

    let version = parl().arg("--version").output().unwrap();
    assert_eq!(version.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(
        stdout.contains("parl") && stdout.contains("0.2.0"),
        "{stdout}"
    );

    // …but respond to --help themselves: they parse.
    let monitor = parl().args(["monitor", "--help"]).output().unwrap();
    assert_eq!(monitor.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&monitor.stdout);
    assert!(
        stdout.contains("--fleet-dir") && stdout.contains("--run"),
        "{stdout}"
    );
    let orch = parl()
        .args(["orchestrator-monitor", "--help"])
        .output()
        .unwrap();
    assert_eq!(orch.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&orch.stdout).contains("--fleet-dir"),
        "{}",
        String::from_utf8_lossy(&orch.stdout)
    );
}

#[test]
fn unknown_subcommand_and_missing_required_arguments_exit_one() {
    let (code, _, stderr) = run(&std::env::temp_dir(), &["frobnicate"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("unrecognized subcommand"), "{stderr}");

    for args in [
        vec!["wait"],          // <name> required
        vec!["spawn"],         // <name> required
        vec!["cleanup"],       // <name|all> required
        vec!["send", "ghost"], // message required after --
        vec!["monitor"],       // --run required for the internal monitor
    ] {
        let (code, _, stderr) = run(&std::env::temp_dir(), &args);
        assert_eq!(code, 1, "parl {args:?} → {code}: {stderr}");
    }
}

/// Every public subcommand reaches its implementation and answers with a
/// sane exit code even on an empty fleet: refusals name the missing run, and
/// the read-only queries answer.
#[test]
fn every_public_subcommand_reaches_its_implementation() {
    let (_tmp, root) = plain_dir();

    let (code, stdout, _) = run(&root, &["status"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "(no runs)");
    let (code, stdout, _) = run(&root, &["status", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "[]");

    // spawn's refusals are ops results, not parse errors.
    let (code, _, stderr) = run(&root, &["spawn", "x", "--", "  "]);
    assert_eq!(code, 1);
    assert!(stderr.contains("task brief required"), "{stderr}");
    let (code, _, stderr) = run(&root, &["spawn", "!!!", "--", "b"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("<name> required"), "{stderr}");
    let (code, _, stderr) = run(
        &root,
        &["spawn", "x", "--model", "no-such-model", "--", "b"],
    );
    assert_eq!(
        code, 2,
        "an unknown model is refused with the NoReport code: {stderr}"
    );
    assert!(stderr.contains("unknown model"), "{stderr}");
    // The model check happens before anything is created.
    assert_eq!(
        root.join(parl::paths::STATE_DIR_NAME)
            .join("runs")
            .read_dir()
            .map(std::iter::Iterator::count)
            .unwrap_or(0),
        0,
        "no run was created"
    );

    // Everything that addresses a run refuses an unknown one with exit 1.
    for args in [
        vec!["status", "ghost"],
        vec!["wait", "ghost"],
        vec!["output", "ghost"],
        vec!["logs", "ghost"],
        vec!["report", "ghost"],
        vec!["attach", "ghost"],
        vec!["send", "ghost", "--", "m"],
        vec!["followup", "ghost", "--", "m"],
        vec!["answer", "ghost", "--", "m"],
        vec!["stop", "ghost"],
        vec!["diff", "ghost"],
        vec!["merge", "ghost"],
        vec!["cleanup", "ghost"],
    ] {
        let (code, _, stderr) = run(&root, &args);
        assert_eq!(code, 1, "parl {args:?} → {code}: {stderr}");
        assert!(
            stderr.contains("No run found matching \"ghost\""),
            "parl {args:?}: {stderr}"
        );
    }
}

/// The hidden worker monitor parses and reaches its implementation: a run
/// without a readable run.json is a startup failure with exit 1.
#[test]
fn the_hidden_worker_monitor_reaches_its_implementation() {
    let (_tmp, root) = plain_dir();
    let fleet_dir = root.join(parl::paths::STATE_DIR_NAME);
    std::fs::create_dir_all(&fleet_dir).unwrap();
    let (code, _, stderr) = run(
        &root,
        &[
            "monitor",
            "--fleet-dir",
            fleet_dir.to_str().unwrap(),
            "--run",
            "ghost-20260828141530",
        ],
    );
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("cannot start"), "{stderr}");
}

/// The hidden orchestrator monitor parses and reaches its implementation: an
/// unspawnable claude is recorded (claude.log) and the monitor ends cleanly
/// instead of hanging or crashing.
#[test]
fn the_hidden_orchestrator_monitor_reaches_its_implementation() {
    let (_tmp, root) = plain_dir();
    let fleet_dir = root.join(parl::paths::STATE_DIR_NAME);
    let output = StdCommand::new(assert_cmd::cargo_bin!("parl"))
        .args(["orchestrator-monitor", "--fleet-dir"])
        .arg(&fleet_dir)
        .env("PARL_DIR", &fleet_dir)
        .env("PARL_CLAUDE_BIN", "definitely-not-a-real-claude")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "the monitor ends cleanly when claude cannot start"
    );
    // The boot state landed in the documented layout (the monitor's own
    // session directory), and the failure was diagnosed in the raw
    // protocol log.
    let key = parl::orch::session::resolve_session(&fleet_dir)
        .expect("boot writes the session row")
        .key();
    assert!(
        parl::paths::FleetPaths::new(&fleet_dir)
            .orchestrator_state(&key)
            .is_file(),
        "the boot state landed in the documented layout"
    );
    let claude_log =
        std::fs::read_to_string(parl::paths::FleetPaths::new(&fleet_dir).claude_log(&key))
            .unwrap_or_default();
    assert!(claude_log.contains("could not spawn"), "{claude_log}");
}

/// `parl mcp` serves the fleet tools over stdio; a clean disconnect exits 0.
#[test]
fn mcp_serves_the_fleet_tools_over_stdio() {
    let _serial = serial();
    let (_tmp, root) = plain_dir();
    let mut child = StdCommand::new(assert_cmd::cargo_bin!("parl"))
        .arg("mcp")
        .current_dir(&root)
        .env(
            "PARL_DIR",
            root.canonicalize()
                .unwrap()
                .join(parl::paths::STATE_DIR_NAME),
        )
        .env("PARL_PI_BIN", pi_spec(&fake_pi()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let send = |stdin: &mut ChildStdin, value: &Value| {
        stdin.write_all(value.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "parl-e2e", "version": "0"},
            },
        }),
    );
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(
        response["result"]["serverInfo"]["name"], "fleet",
        "{response}"
    );
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(line.trim()).unwrap();
    let mut names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    names.sort_unstable();
    let mut expected = parl::mcp::server::FLEET_TOOL_NAMES.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected, "{names:?}");

    // Disconnecting stdin ends the server cleanly.
    drop(stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the mcp server did not exit after stdin closed"
        );
        std::thread::sleep(POLL);
    };
    assert_eq!(code, Some(0));
}

/// A spawned `parl` writes only into the fleet dir it was given: `PARL_DIR`
/// is pinned to one temp dir (the fleet) while the canary stays completely
/// empty. The canary is also the child's cwd, so it traps both failure modes
/// this suite once suffered: an inherited ambient `PARL_DIR` and a
/// `<cwd>/.parl` fallback.
#[test]
fn spawn_writes_only_to_the_fleet_dir_it_was_given() {
    let _serial = serial();
    let fleet = tempfile::tempdir().unwrap();
    let canary = tempfile::tempdir().unwrap();

    let output = parl()
        .args(["spawn", "isolated", "--no-worktree", "--", "b"])
        .current_dir(canary.path())
        .env("PARL_DIR", fleet.path())
        .env("PARL_PI_BIN", pi_spec(&fake_pi()))
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "spawn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let run_id = stdout
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Spawned ")
        .unwrap_or_else(|| panic!("unexpected spawn output: {stdout}"))
        .to_string();

    // The given fleet dir received the run's state.
    let run_json = fleet.path().join("runs").join(&run_id).join("run.json");
    assert!(run_json.is_file(), "run.json in the given dir: {stdout}");

    // The canary is still completely empty.
    assert_eq!(
        std::fs::read_dir(canary.path()).unwrap().count(),
        0,
        "the canary must stay empty"
    );

    // The detached monitor (and its fake pi) ran against the given fleet
    // dir; wait for it to be gone before the temp tree drops.
    let deadline = std::time::Instant::now() + MONITOR_EXIT;
    while let Some(pid) = monitor_pid(&run_json) {
        if !parl::fleet::run::is_alive(Some(pid)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the detached monitor never exited"
        );
        std::thread::sleep(POLL);
    }
}

// ---------------------------------------------------------------------------
// 5. Requirements that travel inside the binary.
// ---------------------------------------------------------------------------

/// The orchestrator prompt is embedded in the binary and renders with every
/// placeholder substituted.
#[test]
fn the_embedded_orchestrator_prompt_renders_with_placeholders_substituted() {
    use parl::orch::prompt::{PromptVars, render_orchestrator_prompt};

    assert!(
        parl::orch::prompt::ORCHESTRATOR_PROMPT_TEMPLATE.contains("{{FLEET_DIR}}"),
        "the shipped template is the placeholdered source"
    );
    let rendered = render_orchestrator_prompt(&PromptVars {
        fleet_dir: "/repo/.parl".into(),
        repo_root: "/repo".into(),
        max_workers: Some(2),
        bin_name: None,
    });
    assert!(rendered.starts_with("# Fleet orchestrator"), "{rendered}");
    assert!(
        !rendered.contains("{{"),
        "all placeholders rendered: {rendered}"
    );
    assert!(rendered.contains("`/repo/.parl`"), "{rendered}");
    assert!(rendered.contains("`/repo`"), "{rendered}");
    assert!(rendered.contains("At most 2 workers"), "{rendered}");
    assert!(rendered.contains("`parl`"), "{rendered}");
}

/// The override chain: `$PARL_PROMPT` wins, then `<repo>/.parl/orchestrator.md`,
/// then `~/.config/parl/orchestrator.md`, then the embedded copy. A dangling
/// `$PARL_PROMPT` is an error, never a silent fallback. The home directory is
/// injected, so the real `$HOME` is never touched.
#[test]
fn the_prompt_override_chain_resolves_in_order() {
    use parl::orch::prompt::resolve_prompt_source;

    let (_tmp, repo) = plain_dir();
    let parl_dir = repo.join(parl::paths::STATE_DIR_NAME);
    std::fs::create_dir_all(&parl_dir).unwrap();
    // A legacy config home plus a new `~/.parl`, both fabricated.
    let home_tmp = tempfile::tempdir().unwrap();
    let home = home_tmp.path();
    let user_root = tempfile::tempdir().unwrap();
    let user_dir = user_root.path().join(parl::paths::STATE_DIR_NAME);
    std::fs::create_dir_all(&user_dir).unwrap();

    // Nothing anywhere: the embedded copy.
    assert_eq!(
        resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
        None
    );

    // ~/.parl/orchestrator.md next, the new user location.
    let user = user_dir.join("orchestrator.md");
    std::fs::write(&user, "user override").unwrap();
    assert_eq!(
        resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
        Some(user.clone())
    );

    // The legacy ~/.config/parl location is no longer consulted at all,
    // even when the new one is empty.
    let legacy = home.join(".config/parl/orchestrator.md");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "legacy override").unwrap();
    std::fs::remove_file(&user).unwrap();
    assert_eq!(
        resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
        None
    );

    // <repo>/.parl/orchestrator.md beats the user config.
    let repo_override = parl_dir.join("orchestrator.md");
    std::fs::write(&repo_override, "repo override").unwrap();
    assert_eq!(
        resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
        Some(repo_override.clone())
    );

    // $PARL_PROMPT (a path) beats everything.
    let env_file = repo.join("custom.md");
    std::fs::write(&env_file, "env override").unwrap();
    assert_eq!(
        resolve_prompt_source(Some(env_file.to_str().unwrap()), &repo, Some(&user_dir)).unwrap(),
        Some(env_file)
    );

    // A dangling $PARL_PROMPT is user intent gone wrong: an error.
    let err = resolve_prompt_source(
        Some(repo.join("missing.md").to_str().unwrap()),
        &repo,
        Some(&user_dir),
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a file"), "{err}");

    // The rendered prompt honours the repo override (no $PARL_PROMPT in this
    // environment), so what claude reads is the override, rendered.
    if std::env::var_os(parl::paths::env_var("PROMPT")).is_none() {
        let rendered = parl::orch::prompt::render_prompt(&parl_dir, &repo).unwrap();
        assert!(
            rendered.contains("repo override") && !rendered.contains("{{"),
            "{rendered}"
        );
    }
}

/// Spawning copies nothing prompt-shaped into the project: the repo only
/// gains the gitignored state dir and the `.gitignore` entry.
#[test]
fn spawning_copies_nothing_into_the_project() {
    let _serial = serial();
    let (_tmp, root) = init_repo();
    let run_id = spawn_ok(&root, "copycat", "b", &fake_pi(), &[], &["--no-worktree"]);
    reap_monitor(&root, &run_id);

    let status = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "?? .gitignore",
        "the only project change is the gitignore entry"
    );
    for outside in [
        root.join("orchestrator.md"),
        root.join("prompt.md"),
        root.join("prompts"),
    ] {
        assert!(!path_exists(&outside), "{}", outside.display());
    }
}

/// The pi worker extension and report skill are materialized from the
/// binary into the fleet dir — never read from a source checkout — and pi is
/// invoked with those paths. A stale file is rewritten; an identical one is
/// left alone.
#[test]
fn the_worker_extension_is_materialized_from_the_binary() {
    use parl::worker::monitor::{FLEET_EXTENSION_TS, FLEET_SKILL_MD};

    let _serial = serial();
    let (_tmp, root) = init_repo();
    let fleet = root
        .canonicalize()
        .unwrap()
        .join(parl::paths::STATE_DIR_NAME);
    let extension = fleet.join("pi/extensions/fleet-worker.ts");
    let skill = fleet.join("pi/skills/fleet-worker-report/SKILL.md");

    // A stale extension (different contents) must be rewritten; an identical
    // skill must be left alone (content-identical, so no write, no mtime
    // bump — the sleep guards against coarse-grained filesystem timestamps).
    std::fs::create_dir_all(extension.parent().unwrap()).unwrap();
    std::fs::write(&extension, "// stale install\n").unwrap();
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, FLEET_SKILL_MD).unwrap();
    let skill_mtime = std::fs::metadata(&skill).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(1200));

    let argv_file = root.join("argv.json");
    let run_id = spawn_ok(
        &root,
        "extworker",
        "b",
        &fake_pi(),
        &[("FAKE_PI_ARGV_FILE", argv_file.to_str().unwrap())],
        &["--no-worktree"],
    );
    settled(&root, "extworker");
    reap_monitor(&root, &run_id);

    // Rewritten from the embedded copy:
    assert_eq!(
        std::fs::read_to_string(&extension).unwrap(),
        FLEET_EXTENSION_TS,
        "a stale extension is replaced with the binary's copy"
    );
    // The identical skill was not touched:
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), FLEET_SKILL_MD);
    assert_eq!(
        std::fs::metadata(&skill).unwrap().modified().unwrap(),
        skill_mtime,
        "an identical file is left alone"
    );

    // pi was invoked with the materialized paths, which live under the
    // fleet dir — not the source checkout's `pi/` tree.
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(&argv_file).unwrap()).unwrap();
    let pair = |flag: &str| {
        argv.iter()
            .rposition(|a| a == flag)
            .map(|at| argv[at + 1].as_str())
    };
    assert_eq!(
        pair("--extension"),
        Some(extension.to_str().unwrap()),
        "{argv:?}"
    );
    assert_eq!(pair("--skill"), Some(skill.to_str().unwrap()), "{argv:?}");
    assert!(extension.starts_with(&fleet), "{extension:?}");
    // The materialized files are the worker protocol: the skill keeps the
    // report template, the extension speaks the PARL layout.
    assert!(
        FLEET_SKILL_MD.starts_with("---\nname: fleet-worker-report\n"),
        "the skill keeps its frontmatter"
    );
    assert!(
        FLEET_SKILL_MD.contains("## Steering received"),
        "the skill keeps the report template"
    );
    assert!(
        FLEET_EXTENSION_TS.contains("PARL_RUN"),
        "the extension speaks the PARL layout"
    );
    assert!(
        !FLEET_EXTENSION_TS.contains("PI_FLEET"),
        "old env names are gone from the extension"
    );
}

// ---------------------------------------------------------------------------
// 4. The `.parl` layout — gitignore hygiene and the orchestrator's half.
// ---------------------------------------------------------------------------

/// `spawn` adds `.parl/` to the repository `.gitignore` without disturbing
/// existing entries, and never adds it twice.
#[test]
fn spawn_gitignores_the_state_dir_without_disturbing_existing_entries() {
    let _serial = serial();
    let (_tmp, root) = init_repo();
    std::fs::write(root.join(".gitignore"), "node_modules/\n*.log\n").unwrap();
    git(&root, &["add", ".gitignore"]);
    git(&root, &["commit", "-qm", "gitignore"]);

    let first = spawn_ok(&root, "ignored1", "b", &fake_pi(), &[], &["--no-worktree"]);
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(gitignore.contains("node_modules/"), "{gitignore}");
    assert!(gitignore.contains("*.log"), "{gitignore}");
    assert!(gitignore.contains("# parl\n.parl/"), "{gitignore}");
    assert_eq!(gitignore.matches(".parl/").count(), 1, "{gitignore}");
    reap_monitor(&root, &first);

    let second = spawn_ok(&root, "ignored2", "b", &fake_pi(), &[], &["--no-worktree"]);
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
    assert_eq!(
        gitignore.matches(".parl/").count(),
        1,
        "still one entry: {gitignore}"
    );
    reap_monitor(&root, &second);
}

/// `cleanup` refuses a running worker without `--force`, and with it aborts
/// the worker, removes the worktree, deletes the branch and archives.
#[test]
fn cleanup_refuses_a_running_worker_and_forces_with_the_flag() {
    let _serial = serial();
    let (_tmp, root) = init_repo();
    let run_id = spawn_ok(
        &root,
        "sleeper",
        "a long task",
        &fake_pi(),
        &[("FAKE_PI_DELAY_MS", "30000")],
        &[],
    );
    let state = poll_status(&root, "sleeper", SETTLE, |state| {
        state["status"] == "running"
    });
    let worktree = PathBuf::from(state["worktree"].as_str().unwrap());
    let branch = state["branch"].as_str().unwrap().to_string();

    let (code, _, stderr) = run(&root, &["cleanup", "sleeper"]);
    assert_eq!(code, 1, "a running worker is not cleaned up: {stderr}");
    assert!(
        stderr.contains("use --force to abort and clean"),
        "{stderr}"
    );
    assert!(worktree.exists(), "the refusal left the worktree alone");
    assert_ne!(status_json(&root, "sleeper")["status"], "archived");

    let (code, stdout, stderr) = run(&root, &["cleanup", "sleeper", "--force"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(&format!("archived {run_id}")), "{stdout}");
    assert!(!worktree.exists(), "the worktree is gone");
    let listed = StdCommand::new("git")
        .args(["branch", "--list", &branch])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).trim().is_empty(),
        "the aborted, unmerged branch is deleted by --force"
    );
    assert_eq!(status_json(&root, "sleeper")["status"], "archived");
    reap_monitor(&root, &run_id);
}

/// The orchestrator side completes the documented layout: booted exactly as
/// the console boots it (`parl orchestrator-monitor`), one user message
/// makes the fake claude report init, and the monitor then keeps
/// `fleet.json`, the rendered prompt (the embedded template, placeholders
/// substituted), the transcript, the raw protocol log and the state file —
/// and never the removed `orchestrator.json`. A `stop` command ends the
/// monitor cleanly.
#[test]
fn the_orchestrator_side_writes_the_documented_fleet_layout() {
    let _serial = serial();
    let (tmp, root) = plain_dir();
    let fleet_dir = root.join(parl::paths::STATE_DIR_NAME);
    let mut monitor = StdCommand::new(assert_cmd::cargo_bin!("parl"))
        .args(["orchestrator-monitor", "--fleet-dir"])
        .arg(&fleet_dir)
        .env("PARL_DIR", &fleet_dir)
        .env("PARL_CLAUDE_BIN", pi_spec(&fake_claude()))
        .env("FAKE_CLAUDE_SESSION_ID", "sess-e2e-12345678")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(&root)
        .process_group(0)
        .spawn()
        .unwrap();

    // Wait for boot (the session dir appears), then send one user
    // message — the fake claude emits init after it, and init is when the
    // monitor persists the session record.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !path_exists(&fleet_dir.join("orchestrators")) {
        assert!(
            std::time::Instant::now() < deadline,
            "the monitor never booted"
        );
        std::thread::sleep(POLL);
    }
    let session_path = session_dir(&fleet_dir);
    let inbox = session_path.join("inbox.jsonl");
    let user = parl::orch::records::OrchestratorCommand::User {
        text: "hello fleet".into(),
    }
    .to_envelope(parl::fleet::envelope::Party::Console);
    parl::fleet::envelope::append_envelope(&inbox, &user).unwrap();

    // the store is created before the session row is upserted into it, so
    // waiting for the file alone can catch it empty
    let fleet_json = fleet_dir.join("fleet.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let store: Value = loop {
        let store = std::fs::read_to_string(&fleet_json)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        if let Some(store) = store
            && store["sessions"].as_object().is_some_and(|s| !s.is_empty())
        {
            break store;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no session row appeared in fleet.json; claude.log: {}",
            std::fs::read_to_string(session_path.join("claude.log")).unwrap_or_default()
        );
        std::thread::sleep(POLL);
    };
    assert_eq!(store["version"], json!(2), "{store}");
    let sessions = store["sessions"].as_object().unwrap();
    assert_eq!(sessions.len(), 1, "{store}");
    let session = sessions.values().next().unwrap();
    assert_eq!(session["sessionId"], "sess-e2e-12345678");
    assert_eq!(session["cwd"], root.to_string_lossy().as_ref());
    assert!(session["uuid"].is_string(), "{store}");
    let key =
        parl::paths::SessionKey::new(None, session["uuid"].as_str().unwrap().parse().unwrap());
    // `fleet.json` is written just before `state.json`, so seeing the one
    // does not mean the other has landed yet
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let state = loop {
        let state = parl::orch::monitor::load_orchestrator_state(&fleet_dir, &key);
        if state.as_ref().is_some_and(|s| s.session_id.is_some()) {
            break state.unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "state.json never carried the session id"
        );
        std::thread::sleep(POLL);
    };
    assert_eq!(state.session_id.as_deref(), Some("sess-e2e-12345678"));

    for documented in [
        session_path.join("state.json"),
        session_path.join("capabilities.json"),
        session_path.join("events.jsonl"),
        session_path.join("inbox.jsonl"),
        session_path.join("claude.log"),
        session_path.join("prompt.md"),
    ] {
        assert!(documented.is_file(), "missing {}", documented.display());
    }
    assert!(
        !path_exists(&fleet_dir.join("orchestrator.json")),
        "the removed orchestrator.json is gone for good"
    );

    // The prompt was rendered from the copy embedded in the binary: the
    // placeholders are substituted with this fleet's paths, nothing unknown
    // remains, and nothing was copied outside the state directory.
    let prompt = std::fs::read_to_string(session_path.join("prompt.md")).unwrap();
    assert!(prompt.starts_with("# Fleet orchestrator"), "{prompt}");
    assert!(!prompt.contains("{{"), "{prompt}");
    assert!(
        prompt.contains(fleet_dir.to_string_lossy().as_ref()),
        "the fleet dir placeholder was substituted: {prompt}"
    );
    assert!(
        !root.join("orchestrator.md").exists()
            && !root.join("prompts").exists()
            && !root.join("pi").exists(),
        "nothing was copied into the project"
    );

    // A stop command ends the monitor (and its claude child) cleanly.
    let stop = parl::orch::records::OrchestratorCommand::Stop
        .to_envelope(parl::fleet::envelope::Party::Console);
    parl::fleet::envelope::append_envelope(&inbox, &stop).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let code = loop {
        if let Some(status) = monitor.try_wait().unwrap() {
            break status.code();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the monitor did not exit after the stop command"
        );
        std::thread::sleep(POLL);
    };
    assert_eq!(code, Some(0), "the monitor exits cleanly on stop");
    let _ = monitor.wait(); // reap: no zombie

    // The state records the ended session for the next console open.
    let ended = parl::orch::monitor::load_orchestrator_state(&fleet_dir, &key).unwrap();
    assert!(
        ended.exited.is_some(),
        "the state records the ended child: {ended:?}"
    );
    let _ = tmp; // the tree outlives the monitor
}

/// The one session directory under `<fleet>/orchestrators/`.
fn session_dir(fleet_dir: &Path) -> PathBuf {
    let dirs: Vec<PathBuf> = std::fs::read_dir(fleet_dir.join("orchestrators"))
        .unwrap_or_else(|_| panic!("no orchestrators dir under {}", fleet_dir.display()))
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    assert_eq!(dirs.len(), 1, "expected one session dir: {dirs:?}");
    dirs.into_iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// 3. A full worker lifecycle against the fake pi.
// ---------------------------------------------------------------------------

/// spawn → status (table and --json) → send → answer a fleet_ask question →
/// wait → report → output/logs/attach → diff → merge → cleanup, then the
/// on-disk proof: the branch really merged, the worktree and branch are
/// gone, the run reads `archived`, and `.parl` holds exactly the documented
/// tree — none of the removed one.
#[test]
fn full_worker_lifecycle_happy_path() {
    let _serial = serial();
    let (_tmp, root) = init_repo();
    let run_id = spawn_ok(
        &root,
        "alpha",
        "create hello.txt with greeting content",
        &session_pi(),
        &[
            ("FAKE_PI_WRITE_HELLO", "1"),
            ("FAKE_PI_ASK", "1"),
            ("FAKE_PI_DELAY_MS", "4000"),
        ],
        &[],
    );
    assert!(
        regex::Regex::new(r"^alpha-[0-9a-f]{7}$")
            .unwrap()
            .is_match(&run_id),
        "{run_id}"
    );
    let fleet = root
        .canonicalize()
        .unwrap()
        .join(parl::paths::STATE_DIR_NAME);
    let run_dir = fleet.join("runs").join(&run_id);
    let _reap_guard = ReapOnDrop {
        run_json: run_dir.join("run.json"),
    };

    let state = status_json(&root, "alpha");
    assert_eq!(state["id"], run_id.as_str(), "{state}");
    assert_eq!(state["name"], "alpha");
    assert_eq!(state["taskBrief"], "create hello.txt with greeting content");
    let worktree = PathBuf::from(state["worktree"].as_str().unwrap());
    let branch = state["branch"].as_str().unwrap().to_string();
    assert!(branch.starts_with("parl/alpha-"), "{branch}");
    assert!(
        worktree.join("seed.txt").exists(),
        "the worktree is a checkout"
    );
    assert!(
        state["baseCommit"].as_str().is_some(),
        "a worktree run records its base commit: {state}"
    );
    assert_eq!(state["isGit"], json!(true));

    // The fleet table shows the run.
    let (code, stdout, _) = run(&root, &["status"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("NAME") && stdout.contains("STATE"),
        "{stdout}"
    );
    assert!(stdout.contains("alpha"), "{stdout}");

    // Steer while the worker runs (it blocks on its question, so the window
    // is wide), then wait for the question to surface as `blocked`.
    let (code, stdout, stderr) = run(&root, &["send", "alpha", "--", "use tabs not spaces"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), "steer queued for alpha", "{stdout}");
    let blocked = poll_status(&root, "alpha", SETTLE, |state| {
        state["pendingQuestion"].is_object() || state["pendingDialog"].is_object()
    });
    let question_id = blocked["pendingQuestion"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(blocked["status"], "blocked", "{blocked}");
    assert_eq!(blocked["pendingQuestion"]["question"], "bcrypt or argon2?");

    // The worker has not finished, so there is no report yet.
    let (code, _, stderr) = run(&root, &["report", "alpha"]);
    assert_eq!(code, 2, "{stderr}");

    // Answer the question; the worker proceeds and settles.
    let (code, stdout, stderr) = run(&root, &["answer", "alpha", "--", "argon2"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains(&format!("answer queued for alpha (question {question_id})")),
        "{stdout}"
    );
    assert_eq!(
        wait_code(&root, "alpha", 60),
        0,
        "an answered, settled run waits with exit 0"
    );
    let state = settled(&root, "alpha");
    assert_eq!(state["status"], "settled", "{state}");
    assert_eq!(state["pendingQuestion"], json!(null));

    // The report is the worker's file, enriched with the answer and the
    // steering, plus the orchestrator-side appendix.
    let (code, stdout, stderr) = run(&root, &["report", "alpha"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("# Fleet Report:"), "{stdout}");
    assert!(stdout.contains("## Status\ndone"), "{stdout}");
    assert!(stdout.contains("Answer received: argon2"), "{stdout}");
    assert!(
        stdout.contains("## Steering received\n- use tabs not spaces"),
        "{stdout}"
    );
    assert!(
        stdout.contains("## Steering log (orchestrator-side, most recent last)"),
        "{stdout}"
    );
    assert!(stdout.contains("- [orchestrator]"), "{stdout}");
    assert!(stdout.contains("use tabs not spaces"), "{stdout}");

    // Output: the captured last text, or the tool trail with --tail.
    let (code, stdout, _) = run(&root, &["output", "alpha"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "Working: wrote hello.txt");
    let (code, stdout, _) = run(&root, &["output", "alpha", "--tail", "5"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("bash: hi"), "{stdout}");
    assert!(stdout.contains("fleet_ask:"), "{stdout}");
    let (code, stdout, _) = run(&root, &["logs", "alpha", "--tail", "50"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("agent_settled"), "{stdout}");
    let (code, stdout, stderr) = run(&root, &["attach", "alpha"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("▶ task: create hello.txt"), "{stdout}");
    assert!(stdout.contains("● settled"), "{stdout}");
    assert!(
        stdout.contains("▶ orchestrator: use tabs not spaces"),
        "{stdout}"
    );
    assert!(
        stderr.contains("static tail — run `parl` for the live console"),
        "{stderr}"
    );

    // The single-run JSON carries the session file pi wrote.
    let state = status_json(&root, "alpha");
    let session_file = state["sessionFile"].as_str().unwrap_or_default();
    let session_path = Path::new(session_file);
    assert!(
        session_path.starts_with(run_dir.join("session"))
            && session_path.extension() == Some(std::ffi::OsStr::new("jsonl")),
        "{state}"
    );
    assert!(session_path.is_file(), "{session_path:?}");

    // Diff sees the worker's committed work once it is committed.
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-qm", "worker hello"]);
    let (code, stdout, _) = run(&root, &["diff", "alpha"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("hello.txt"), "{stdout}");
    let (code, stdout, _) = run(&root, &["diff", "alpha", "--name-only"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "hello.txt");

    // Merge lands the branch in the recorded checkout.
    let (code, stdout, stderr) = run(&root, &["merge", "alpha"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains(&format!("merged {branch} into")),
        "{stdout}"
    );
    assert!(stdout.contains("Run your integration checks"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hi\n",
        "the branch actually landed"
    );

    // Cleanup archives the run, removes the worktree and deletes the merged
    // branch.
    let (code, stdout, stderr) = run(&root, &["cleanup", "alpha"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(&format!("archived {run_id}")), "{stdout}");
    assert!(!worktree.exists(), "the worktree is gone");
    let listed = StdCommand::new("git")
        .args(["branch", "--list", &branch])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).trim().is_empty(),
        "the merged branch is deleted"
    );
    let (code, stdout, _) = run(&root, &["cleanup", "alpha"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("already archived"), "{stdout}");

    // The run reads archived; the default fleet view hides it.
    assert_eq!(status_json(&root, "alpha")["status"], "archived");
    let (code, stdout, _) = run(&root, &["status", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "[]", "archived runs are hidden by default");
    let (code, stdout, _) = run(&root, &["status", "--all", "--json"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("archived"), "{stdout}");

    reap_monitor(&root, &run_id);

    // ------------------------------------------------------------------
    // The `.parl` layout is what AGENTS.md says it is.
    // ------------------------------------------------------------------
    for path in [
        run_dir.join("run.json"),
        run_dir.join("events.jsonl"),
        run_dir.join("inbox.jsonl"),
        run_dir.join("outbox.jsonl"),
        run_dir.join("report.md"),
        run_dir.join("pi.log"),
        run_dir.join("session"),
        fleet.join("runs"),
        fleet.join("orchestrators"),
        fleet.join("pi").join("extensions").join("fleet-worker.ts"),
        fleet
            .join("pi")
            .join("skills")
            .join("fleet-worker-report")
            .join("SKILL.md"),
    ] {
        assert!(
            path.exists(),
            "documented layout missing: {}",
            path.display()
        );
    }
    for gone in [
        fleet.join("reports"),
        root.join("reports"),
        fleet.join("orchestrator.json"),
        run_dir.join("monitor.log"),
        run_dir.join("progress.md"),
        fleet.join("progress.md"),
        run_dir.join("control.jsonl"),
        fleet.join("tui.lock"),
    ] {
        assert!(
            !path_exists(&gone),
            "removed layout came back: {}",
            gone.display()
        );
    }
    // The mailbox files carry real envelopes.
    let inbox = std::fs::read_to_string(run_dir.join("inbox.jsonl")).unwrap();
    assert!(
        inbox.contains("\"steer\"") && inbox.contains("\"answer\""),
        "{inbox}"
    );
    let outbox = std::fs::read_to_string(run_dir.join("outbox.jsonl")).unwrap();
    assert!(outbox.contains("\"question\""), "{outbox}");
    // events.jsonl captured the run transcript.
    let events = std::fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
    assert!(events.contains("\"task_prompt\""), "{events}");
    assert!(events.contains("\"worker_question\""), "{events}");
    assert!(events.contains("\"agent_settled\""), "{events}");
}

fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

// ---------------------------------------------------------------------------
// 2. The exit-code matrix — the contract scripts depend on.
// ---------------------------------------------------------------------------

/// One slow, running worker drives every refusal exit code: merging an
/// unsettled run (1), answering with nothing pending (1), a wait timeout (3),
/// then the run ends stopped (4 via `wait`), and steering a finished run is
/// refused (1).
#[test]
fn refusal_exit_codes_on_a_running_then_stopped_run() {
    let _serial = serial();
    let (tmp, root) = plain_dir();
    let run_id = spawn_ok(
        &root,
        "slowpoke",
        "long task",
        &fake_pi(),
        &[("FAKE_PI_DELAY_MS", "20000")],
        &["--no-worktree"],
    );
    let _reap_guard = ReapOnDrop {
        run_json: tmp
            .path()
            .join(parl::paths::STATE_DIR_NAME)
            .join("runs")
            .join(&run_id)
            .join("run.json"),
    };

    // `spawn` returns before the monitor has reported its pid, and a run
    // without one still reads as `starting`: wait for the state the
    // assertions below are about.
    poll_status(&root, "slowpoke", SETTLE, |state| {
        state["status"] == "running"
    });

    // A running run cannot be merged (1) and has nothing to answer (1).
    let (code, _, stderr) = run(&root, &["merge", "slowpoke"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("is running"), "{stderr}");
    assert!(
        stderr.contains("only settled runs can be merged"),
        "{stderr}"
    );
    let (code, _, stderr) = run(&root, &["answer", "slowpoke", "--", "argon2"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("no pending question"), "{stderr}");

    // A wait that outlives its timeout is exit 3, not an error.
    assert_eq!(wait_code(&root, "slowpoke", 1), 3, "timeout is exit 3");
    let (code, _, stderr) = run(&root, &["wait", "slowpoke", "--timeout", "1"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("timed out after 1s"), "{stderr}");

    // Stop settles the run as stopped; `wait` then reports the bad end (4).
    let (code, _, stderr) = run(&root, &["stop", "slowpoke"]);
    assert_eq!(code, 0, "{stderr}");
    let state = settled(&root, "slowpoke");
    assert_eq!(state["status"], "stopped", "{state}");
    assert_eq!(
        wait_code(&root, "slowpoke", 30),
        4,
        "a stopped run is exit 4"
    );

    // A finished run refuses steering (1) with the resume hint.
    let (code, _, stderr) = run(&root, &["send", "slowpoke", "--", "too late"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("steering refused"), "{stderr}");
    assert!(
        stderr.contains("parl spawn slowpoke-2 --session"),
        "carries the resume hint: {stderr}"
    );

    reap_monitor(&root, &run_id);
}

/// A worker whose pi dies without settling reads `error`, its reason names
/// the cause, `wait` is exit 4, and with no report and no captured text
/// `report` is exit 2.
#[test]
fn an_error_run_names_its_cause_and_report_is_exit_two() {
    let _serial = serial();
    let (tmp, root) = plain_dir();
    let run_id = spawn_ok(&root, "doomed", "b", &fail_pi(), &[], &["--no-worktree"]);
    let _reap_guard = ReapOnDrop {
        run_json: tmp
            .path()
            .join(parl::paths::STATE_DIR_NAME)
            .join("runs")
            .join(&run_id)
            .join("run.json"),
    };

    assert_eq!(wait_code(&root, "doomed", 30), 4, "an error run is exit 4");
    let state = settled(&root, "doomed");
    assert_eq!(state["status"], "error", "{state}");
    let error = state["error"].as_str().unwrap_or_default();
    assert!(error.contains("exited with code 1"), "{error}");
    assert!(error.contains("model provider unreachable"), "{error}");

    let (code, _, stderr) = run(&root, &["report", "doomed"]);
    assert_eq!(code, 2, "no report and no captured text: {stderr}");
    assert!(
        stderr.contains("no report file and no captured output for doomed"),
        "{stderr}"
    );

    reap_monitor(&root, &run_id);
}

/// A conflicting branch merges with exit 5: the merge is aborted, the
/// checkout is left clean, and the message tells the caller to have the
/// worker rebase — the orchestrator never edits files itself.
#[test]
fn a_conflicting_branch_merges_with_exit_five_and_a_clean_checkout() {
    let _serial = serial();
    let (_tmp, root) = init_repo();
    let run_id = spawn_ok(
        &root,
        "conflicter",
        "write hello.txt",
        &fake_pi(),
        &[("FAKE_PI_WRITE_HELLO", "1")],
        &[],
    );
    let _reap_guard = ReapOnDrop {
        run_json: root
            .canonicalize()
            .unwrap()
            .join(parl::paths::STATE_DIR_NAME)
            .join("runs")
            .join(&run_id)
            .join("run.json"),
    };

    let state = settled(&root, "conflicter");
    assert_eq!(state["status"], "settled", "{state}");
    let branch = state["branch"].as_str().unwrap().to_string();
    let worktree = PathBuf::from(state["worktree"].as_str().unwrap());
    assert!(branch.starts_with("parl/conflicter-"), "{branch}");

    // The fake wrote hello.txt but did not commit; the worker commits.
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-qm", "worker hello"]);
    let base = state["baseCommit"].as_str().unwrap().to_string();

    // A conflicting change lands on the main checkout.
    std::fs::write(root.join("hello.txt"), "different\n").unwrap();
    git(&root, &["add", "hello.txt"]);
    git(&root, &["commit", "-qm", "conflict"]);

    let (code, stdout, stderr) = run(&root, &["merge", "conflicter"]);
    assert_eq!(code, 5, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.is_empty(),
        "a conflict prints to stderr only: {stdout:?}"
    );
    assert!(stderr.contains("conflicts in:\nhello.txt"), "{stderr}");
    assert!(
        stderr.contains("The merge was aborted; the checkout is clean"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("rebase its branch {branch}")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&base[..7]),
        "names the commit the branch was cut from: {stderr}"
    );

    // The abort left the checkout clean and the worker's file untouched.
    let status = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&root)
        .output()
        .unwrap();
    let porcelain_text = String::from_utf8_lossy(&status.stdout).into_owned();
    let porcelain: Vec<&str> = porcelain_text
        .lines()
        .filter(|line| !line.is_empty() && *line != "?? .gitignore")
        .collect();
    assert_eq!(porcelain, Vec::<&str>::new(), "{porcelain:?}");
    assert!(
        !root.join(".git").join("MERGE_HEAD").exists(),
        "the merge abort removed MERGE_HEAD"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "different\n"
    );

    reap_monitor(&root, &run_id);
}
