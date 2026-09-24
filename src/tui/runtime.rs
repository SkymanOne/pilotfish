//! Crossterm runtime and terminal setup: raw mode, the alternate screen, the
//! event stream, restore on panic and on every exit path — a console that
//! dies in raw mode leaves the user's shell unusable, so teardown outranks
//! every feature here. Also the single-instance console lock (a second
//! console must not fight the first over the terminal state) and the feed
//! loop that polls `.parl` into the state machine.

use std::collections::HashMap;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::id as process_id;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use tokio::time::MissedTickBehavior;

use crate::cli::ExitCode;
use crate::orch::records::{EventRecord, OrchestratorState};
use crate::orch::session::{self, OrchestratorSession};
use crate::orch::watcher::{FleetWatcher, FleetWatcherOptions};
use crate::paths::{FleetPaths, SessionKey};
use crate::tui::app::{Console, Effect, RunEntry, TuiOptions};
use crate::tui::completions::list_repo_files;
use crate::tui::theme::Palette;
use crate::tui::view::{self, Feeds};
use crate::util::{now_iso, now_ms, read_new_lines};

/// The repo the console is opened on: the fleet dir's parent (`.parl` lives
/// at the repo root).
fn repo_cwd(fleet: &FleetPaths) -> String {
    fleet
        .root()
        .parent()
        .unwrap_or_else(|| fleet.root())
        .to_string_lossy()
        .into_owned()
}

/// The dashboard's diff-stat refresh cadence: `diff` shells out to git once
/// per run, so this is a background nicety the poll loop catches up on, not
/// a per-tick duty.
/// The clock: ages, elapsed counters and spinners move on this tick even
/// when no file changed.
const TICK_MS: u64 = 250;
/// How often the transcript tail is read. Two offset reads, so it is cheap
/// enough to run at this rate — end to end a token now reaches the screen in
/// about a quarter of a second rather than the better part of one.
const TAIL_MS: u64 = 120;
/// How often run state, capabilities and diff stats are reloaded. Every
/// run.json is read, so this stays slow.
const FEED_MS: u64 = 400;
const DIFF_STAT_MS: i64 = 10_000;

// ---------------------------------------------------------------------------
// Terminal install / teardown

/// Is this an interactive terminal? Both ends must be a TTY, the way the
/// TypeScript console asked (`process.stdin.isTTY && process.stdout.isTTY`).
/// A crossterm size query lies here: its `tput` fallback answers even on a
/// worker session with no controlling terminal, and raw mode would then die
/// later on the missing `/dev/tty` with a bare io error.
#[must_use]
pub fn is_interactive() -> bool {
    use crossterm::tty::IsTty as _;
    io::stdin().is_tty() && io::stdout().is_tty()
}

/// Install the terminal: raw mode, alternate screen, mouse capture, the
/// kitty keyboard protocol, backend.
///
/// # Errors
/// Raw mode or the screen switch failing — there is nothing to restore yet.
pub fn enter() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Without bracketed paste a pasted brief arrives as keystrokes, and its
    // first newline is an enter that sends the rest half-typed. With it the
    // terminal hands over the whole paste as one event.
    let _ = execute!(stdout, EnableBracketedPaste);
    // Without this a terminal cannot tell shift-enter from enter — both are
    // a bare CR — so the composer could never bind the newline everyone
    // reaches for. `DISAMBIGUATE_ESCAPE_CODES` makes the terminal send
    // `CSI 13;2u` instead. Pushed blind and best effort: a terminal that
    // does not speak the protocol ignores both the push and the pop, while
    // `supports_keyboard_enhancement` costs a 2 s round trip on exactly
    // those terminals, and a slow console start is worse than a key that
    // falls back to alt-enter.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    Terminal::new(CrosstermBackend::new(stdout))
}

/// Undo [`enter`]: release the mouse, leave the alternate screen, drop raw
/// mode, flush. Best effort and idempotent — called from the panic hook and
/// every exit path, and it must never mask the error that brought us here.
pub fn restore() {
    use std::io::Write as _;
    // the enhancement flags pop first: leaving them on would outlive the
    // console and confuse whatever the shell runs next
    let _ = execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = io::stdout().flush();
}

/// Take the mouse for the console, or hand it back to the terminal so its
/// own click-and-drag selection works. Best effort: a terminal that ignores
/// the sequence simply keeps whatever it was doing, and the console's
/// keyboard scrolling is unaffected either way.
pub fn set_mouse_capture(on: bool) {
    let _ = if on {
        execute!(io::stdout(), EnableMouseCapture)
    } else {
        execute!(io::stdout(), DisableMouseCapture)
    };
}

/// Restore the terminal when the process panics anywhere.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

// ---------------------------------------------------------------------------
// The single-instance lock (`console.lock`, same shape the TypeScript wrote)

/// The console's hold on the fleet: one live console per `.parl`.
pub struct ConsoleLock {
    path: PathBuf,
}

impl ConsoleLock {
    /// Take the lock, refusing when another live console holds it.
    ///
    /// # Errors
    /// A live lock (fresh ts, foreign pid) — the refusal names its pid.
    pub fn acquire(fleet: &FleetPaths) -> anyhow::Result<Self> {
        let path = fleet.console_lock();
        if let Some(pid) = active_lock(&path) {
            anyhow::bail!(
                "another console (pid {pid}) is already open on {}",
                fleet.root().display()
            );
        }
        write_lock(&path)?;
        Ok(Self { path })
    }

    /// The heartbeat: a stale lock reads as a crashed console, so a
    /// long-lived console keeps stamping its own.
    pub fn refresh(&self) {
        let _ = write_lock(&self.path);
    }
}

impl Drop for ConsoleLock {
    fn drop(&mut self) {
        // remove only if still ours: a takeover already owns the file
        if let Ok(raw) = std::fs::read_to_string(&self.path)
            && let Ok(value) = serde_json::from_str::<Value>(&raw)
            && value.get("pid").and_then(Value::as_u64) == Some(u64::from(process_id()))
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Another live console's pid, or none (missing, malformed, stale, or ours).
fn active_lock(path: &Path) -> Option<u64> {
    crate::paths::console_holder(path).filter(|pid| *pid != u64::from(process_id()))
}

fn write_lock(path: &Path) -> std::io::Result<()> {
    std::fs::write(
        path,
        json!({ "pid": process_id(), "ts": now_iso() }).to_string(),
    )
}

// ---------------------------------------------------------------------------
// Feeds: what `.parl` says, folded into the state machine

/// The session this console serves: the most recently used one, or — on a
/// fleet without a store yet — a fresh row this console writes under the
/// store's lock (the monitor boots into the most recently used, which that
/// row now is). A store this code cannot parse is left alone, and the
/// default session key is used read-only: nothing a console does may clobber
/// a newer writer's store.
#[must_use]
pub(crate) fn resolve_console_key(fleet: &FleetPaths) -> SessionKey {
    let store = session::load(fleet.root()).unwrap_or_default();
    match store.last_used() {
        Some(record) => record.key(),
        None if !fleet.fleet_json().exists() => {
            let record = OrchestratorSession::new(&repo_cwd(fleet));
            let key = record.key();
            let fleet_dir = fleet.root().to_path_buf();
            let _ = crate::orch::session::with_store_mutation(&fleet_dir, |store| {
                store.upsert(record);
            });
            key
        }
        None => SessionKey::default(),
    }
}

/// The runtime's polled view of `.parl`, kept beside the `Console`: the
/// renderer reads the orchestrator state and run entries directly (they carry
/// the permission mode, pending approvals and worker facts the status line
/// needs), while the `Console` gets the same facts through its feeds.
struct Poll {
    fleet: FleetPaths,
    /// The session this console serves; every `orchestrators/` read derives
    /// from it.
    key: SessionKey,
    runs: Vec<RunEntry>,
    orch: OrchestratorState,
    /// What the orchestrator's agent currently offers, re-read every poll.
    caps: crate::orch::records::Capabilities,
    orch_offset: u64,
    /// The inode `orch_offset` is an offset into; a trim replaces the file,
    /// and an offset into the old one means nothing in the new.
    orch_inode: Option<u64>,
    worker_offsets: HashMap<String, u64>,
    /// The fleet-event watcher, owned for the console's lifetime: its
    /// cursors are the memory that keeps a reopened console from replaying
    /// fleet events the orchestrator has already heard.
    watcher: FleetWatcher,
    /// The diff stats the runtime last fed the console; the mirror is what
    /// decides whether a row needs redrawing.
    diff_stats: HashMap<String, String>,
    /// When the diff stats were last computed (the throttle).
    diff_at: i64,
}

impl Poll {
    fn new(fleet: FleetPaths, key: SessionKey, watcher: FleetWatcher) -> Self {
        Self {
            fleet,
            key,
            runs: Vec::new(),
            orch: OrchestratorState::default(),
            caps: crate::orch::records::Capabilities::default(),
            orch_offset: 0,
            orch_inode: None,
            worker_offsets: HashMap::new(),
            watcher,
            diff_stats: HashMap::new(),
            diff_at: 0,
        }
    }

    fn reload_runs(&mut self) {
        // the rail shows THIS session's workers only: other sessions' runs
        // are filtered out by ownership, the same predicate that scopes the
        // event routing
        self.runs = crate::fleet::run::list_runs_for_owner(self.fleet.root(), self.key.uuid)
            .into_iter()
            .filter_map(|summary| {
                crate::fleet::run::load_state(&summary.run_dir)
                    .ok()
                    .map(|state| RunEntry {
                        run_id: summary.run_id,
                        state,
                    })
            })
            .collect();
    }

    /// Follow the session row: when the orchestrator derives an alias (or a
    /// row disappears), the key every `orchestrators/` read derives from
    /// follows it, so the console's inbox writes and transcript reads land
    /// wherever the session currently lives.
    fn reconcile_session(&mut self) {
        if let Some(session) =
            crate::orch::session::session_by_key(self.fleet.root(), &self.key.uuid.to_string())
        {
            self.key = session.key();
        }
    }

    fn reload_orchestrator(&mut self) {
        let Ok(raw) = std::fs::read_to_string(self.fleet.orchestrator_state(&self.key)) else {
            return;
        };
        if let Ok(state) = serde_json::from_str::<OrchestratorState>(&raw) {
            self.orch = state;
        }
        self.caps = crate::orch::records::read_capabilities(
            &self.fleet.orchestrator_capabilities(&self.key),
        );
    }

    /// Fold everything new into the console: the orchestrator's transcript
    /// and every worker's events. Offsets start at zero, so the first poll
    /// *is* the replay on console open — the same ingest path a live tail
    /// uses.
    /// Returns whether anything new was folded in, so the caller can skip a
    /// redraw that would paint the same frame again.
    fn tail_events(&mut self, console: &mut Console) -> bool {
        let mut ingested = false;
        // The monitor caps the file while it runs, so a file shorter than
        // our cursor means it was trimmed, not that it went backwards: start
        // the transcript again from what is left.
        // A trim renames a new file over the old one, so a changed inode is
        // the reliable sign; a shorter file catches anything that rewrote it
        // in place. Size alone misses a trim followed by enough new bytes to
        // pass the old offset, and then reads from the middle of a record.
        let events_path = self.fleet.orchestrator_events(&self.key);
        if let Ok(meta) = std::fs::metadata(&events_path) {
            use std::os::unix::fs::MetadataExt as _;
            let replaced = self.orch_inode.is_some_and(|inode| inode != meta.ino());
            if replaced || meta.len() < self.orch_offset {
                self.orch_offset = 0;
                console.reset_orchestrator_transcript();
                ingested = true;
            }
            self.orch_inode = Some(meta.ino());
        }
        let (lines, offset) = read_new_lines(&events_path, self.orch_offset);
        self.orch_offset = offset;
        ingested |= !lines.is_empty();
        for line in &lines {
            if let Ok(record) = serde_json::from_str::<EventRecord>(line) {
                console.ingest_orchestrator_record(&record);
            }
        }
        // offsets for runs that vanished are dead weight
        self.worker_offsets
            .retain(|run_id, _| self.runs.iter().any(|run| &run.run_id == run_id));
        for run in &self.runs {
            let offset = self.worker_offsets.entry(run.run_id.clone()).or_insert(0);
            let (lines, next) = read_new_lines(&self.fleet.run_events(&run.run_id), *offset);
            *offset = next;
            ingested |= !lines.is_empty();
            for line in &lines {
                if let Ok(event) = serde_json::from_str::<Value>(line) {
                    console.ingest_worker_event(&run.run_id, &event);
                }
            }
        }
        ingested
    }

    /// The watcher seam (`orch::watcher`): one poll pass, then everything
    /// queued goes to the orchestrator as one `<fleet-event>` batch. Cursors
    /// ride along to the session record after every forwarded batch, so a
    /// console that dies right after telling the orchestrator something does
    /// not tell it again on the next open.
    async fn forward_fleet_events(&mut self, console: &mut Console) {
        self.watcher.tick();
        let events = self.watcher.take_batch();
        if events.is_empty() {
            return;
        }
        let batch = crate::fleet::event::format_fleet_batch(&events, self.watcher.batch_limit());
        let effects = console.ingest_fleet_events(&events, &batch);
        console.execute_all(effects).await;
        self.save_cursors();
    }

    /// Save the watcher's cursors into this console's session row
    /// (`fleet.json`) under the store's lock. A store this code cannot
    /// parse — a newer writer's — is left alone rather than clobbered with
    /// a fresh one; the mutation also round-trips the console prefs key and
    /// every session row it did not touch.
    fn save_cursors(&self) {
        let cursors = self.watcher.cursors();
        let key = self.key.clone();
        let fleet_dir = self.fleet.root().to_path_buf();
        let _ = crate::orch::session::with_store_mutation(&fleet_dir, |store| {
            let Some(record) = store.sessions.get_mut(&key.uuid) else {
                return; // the row this console serves is gone: nothing to save into
            };
            record.watcher.cursors = cursors;
        });
    }

    /// Refresh the dashboard's diff stats, on the [`DIFF_STAT_MS`] cadence:
    /// one `ops::integrate::diff` per live run (it shells out to git), the
    /// result compacted to the `+12 −3` a row can carry, and only real
    /// changes pushed at the console.
    async fn refresh_diff_stats(&mut self, console: &mut Console) {
        let now = now_ms();
        if now - self.diff_at < DIFF_STAT_MS {
            return;
        }
        self.diff_at = now;
        let repo_root = repo_cwd(&self.fleet);
        // Diff against THIS console's anchored fleet, pinned: a changed
        // ambient PARL_DIR must not divert the stat to another fleet.
        let fleet_dir = self.fleet.root().to_string_lossy().into_owned();
        for run in &self.runs {
            if run.state.status == crate::fleet::run::RunStatus::Archived {
                continue;
            }
            let stat = match crate::ops::integrate::diff_core_with_env(
                &run.run_id,
                Some(repo_root.as_ref()),
                false,
                Some(fleet_dir.as_str()),
            )
            .await
            {
                Ok(result) if result.code == ExitCode::Ok => compact_stat(&result.data.text),
                _ => None,
            };
            match stat {
                Some(stat) if self.diff_stats.get(&run.run_id) != Some(&stat) => {
                    console.set_diff_stat(&run.run_id, stat.clone());
                    self.diff_stats.insert(run.run_id.clone(), stat);
                }
                Some(_) => {}
                None => {
                    if self.diff_stats.remove(&run.run_id).is_some() {
                        console.clear_diff_stat(&run.run_id);
                    }
                }
            }
        }
    }
}

/// `git diff --stat` ends with a summary like
/// ` 1 file changed, 12 insertions(+), 3 deletions(-)`; the dashboard row
/// shows that as `+12 −3`. None when nothing was inserted or deleted — no
/// worktree, no changes, or an unreadable diff all leave the row clean.
fn compact_stat(stat: &str) -> Option<String> {
    let summary = stat.lines().last()?.trim();
    let mut plus = None;
    let mut minus = None;
    for part in summary.split(',') {
        if let Some((count, what)) = part.trim().split_once(' ')
            && let Ok(count) = count.parse::<u64>()
        {
            if what.starts_with("insertion") {
                plus = Some(count);
            } else if what.starts_with("deletion") {
                minus = Some(count);
            }
        }
    }
    match (plus, minus) {
        (None, None) => None,
        (plus, minus) => Some(format!("+{} −{}", plus.unwrap_or(0), minus.unwrap_or(0))),
    }
}

/// The orchestrator monitor keeps the claude child alive across consoles;
/// the console only makes sure one is running, detached, with its output on
/// the session's `claude.log`.
///
/// Returns whether a monitor was started (`false`: one was already running
/// and this console is attaching). The `orchestrator-monitor` CLI takes
/// `--fleet-dir` and — since the monitor slice — `--session <uuid>`; the
/// console passes the anchored session's uuid so the spawned monitor serves
/// exactly that row, not whichever happened to be most recently stamped.
/// The console's launch flags are recorded in the session store
/// ([`crate::orch::session::LaunchOptions`]) where the monitor's boot reads
/// them; on attach they are left alone so a running monitor keeps whatever
/// it was launched or live-changed to. The user config dir is injectable so
/// tests never resolve a real home.
///
/// # Errors
///
/// Returns an error when the user config is malformed or the monitor cannot
/// be spawned.
fn ensure_orchestrator(
    fleet: &FleetPaths,
    options: &TuiOptions,
    user_config_dir: Option<&Path>,
    key: &SessionKey,
) -> anyhow::Result<bool> {
    let state = std::fs::read_to_string(fleet.orchestrator_state(key))
        .ok()
        .and_then(|raw| serde_json::from_str::<OrchestratorState>(&raw).ok());
    if let Some(pid) = state.as_ref().and_then(|s| s.pid)
        && crate::fleet::run::is_alive(Some(pid))
    {
        return Ok(false);
    }
    record_launch_options(fleet, options, user_config_dir, key)?;
    let exe = std::env::current_exe().context("finding the parl binary")?;
    // The session's directory is created lazily by whoever owns the key;
    // the monitor's log must exist before the monitor itself does.
    std::fs::create_dir_all(fleet.orchestrator_dir(key))
        .context("creating the session directory")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fleet.claude_log(key))
        .context("opening orchestrator/claude.log")?;
    let err = log.try_clone().context("cloning the log handle")?;
    std::process::Command::new(exe)
        .args(monitor_argv(fleet, key))
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err)
        .process_group(0) // detached: outlives the console, no signal relay
        .spawn()
        .context("spawning the orchestrator monitor")?;
    Ok(true)
}

/// The `orchestrator-monitor` argv for the anchored session: `--session`
/// pins the monitor to exactly that row — not whichever happened to be
/// most recently stamped, which two consoles anchored on different
/// sessions would race over. The nil uuid (the legacy default session) is
/// left to the monitor's own resolution, so a bare `--fleet-dir` spawn
/// keeps working everywhere.
#[must_use]
fn monitor_argv(fleet: &FleetPaths, key: &SessionKey) -> Vec<String> {
    let mut args = vec![
        "orchestrator-monitor".to_string(),
        "--fleet-dir".to_string(),
        fleet.root().to_string_lossy().into_owned(),
    ];
    if !key.uuid.is_nil() {
        args.push("--session".to_string());
        args.push(key.uuid.to_string());
    }
    args
}

/// Record the console's launch flags for the monitor it is about to spawn,
/// under the store's lock so an interleaving heartbeat cannot be lost.
fn record_launch_options(
    fleet: &FleetPaths,
    options: &TuiOptions,
    user_config_dir: Option<&Path>,
    key: &SessionKey,
) -> anyhow::Result<()> {
    let config = crate::paths::load_user_config(user_config_dir)?;
    if session::load(fleet.root()).is_none() {
        return Ok(()); // unreadable or foreign: a monitor would boot its own row
    }
    let launch = crate::orch::session::LaunchOptions {
        model: config
            .orchestrator_model(options.model.as_deref(), None)
            .map(str::to_string),
        budget_usd: options
            .budget
            .as_deref()
            .and_then(|budget| budget.trim().parse::<f64>().ok())
            .filter(|usd| *usd > 0.0),
        permission_mode: options.permission_mode.clone(),
        remote_control: options.remote_control.clone(),
        fresh: Some(options.fresh),
        auto_compact_turns: config.auto_compact_turns(),
    };
    // Opening this session makes it the one a reopened console resumes.
    let now = crate::util::now_iso();
    let uuid = key.uuid;
    let fleet_dir = fleet.root().to_path_buf();
    let _ = crate::orch::session::with_store_mutation(&fleet_dir, |store| {
        let Some(record) = store.sessions.get_mut(&uuid) else {
            return;
        };
        record.launch = launch;
        record.last_used_at = now;
    });
    Ok(())
}

/// Raw mode means ctrl-c never becomes SIGINT: the runtime reads it as the
/// console's own quit, the way the old ink app's `exitOnCtrlC` did.
fn is_interrupt(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
}

// ---------------------------------------------------------------------------
// Anchoring the console on a session

/// Point the console and its poll at `key` and bring that session's facts
/// in: its runs (by ownership, so the rail shows only this session's
/// workers), its orchestrator state and transcripts, a watcher anchored on
/// the session row's saved cursors, and its monitor running. Shared by the
/// open path (the resumed session) and every `/session` switch.
async fn anchor_console(
    fleet: &FleetPaths,
    options: &TuiOptions,
    user_dir: Option<&Path>,
    console: &mut Console,
    key: SessionKey,
) -> Poll {
    let cursors = session::load(fleet.root())
        .and_then(|store| {
            store
                .sessions
                .get(&key.uuid)
                .map(|record| record.watcher.cursors.clone())
        })
        .unwrap_or_default();
    let mut poll = Poll::new(
        fleet.clone(),
        key.clone(),
        FleetWatcher::new(FleetWatcherOptions {
            fleet_dir: fleet.root().to_path_buf(),
            cursors,
            progress_events: options.progress_events,
            ..FleetWatcherOptions::default()
        }),
    );
    // the old session's texts and selections do not bleed into the new one
    console.begin_session(&key);
    poll.reload_runs();
    poll.reload_orchestrator();
    console.set_runs(poll.runs.clone());
    console.set_orchestrator_state(poll.orch.clone());
    console.set_capabilities(poll.caps.clone());
    poll.tail_events(console);
    let started = match ensure_orchestrator(fleet, options, user_dir, &key) {
        // the monitor announces itself in the transcript ("· new orchestrator
        // session", "· resumed …"), so a line of our own would say it twice
        Ok(true) => true,
        Ok(false) => {
            console.toast("· attached to the orchestrator already running here", false);
            false
        }
        Err(err) => {
            console.notice(format!("! orchestrator: {err:#}"), true);
            false
        }
    };
    // Attaching to a live orchestrator means the fleet is mid-flight: tell
    // it what is running. A freshly spawned monitor learns the fleet itself.
    poll.watcher.start(!started);
    poll
}

// ---------------------------------------------------------------------------
// The event loop

/// Run the console until the user quits: one draw per pass, key events
/// through the state machine and its effects, `.parl` polled into the feeds
/// on a timer, the lock heartbeating. Workers keep running afterwards.
///
/// # Errors
/// Terminal draw failures only; console errors surface as notices.
pub async fn run_console(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    fleet: FleetPaths,
    lock: &ConsoleLock,
    options: TuiOptions,
) -> anyhow::Result<ExitCode> {
    let repo_root = fleet
        .root()
        .parent()
        .unwrap_or_else(|| fleet.root())
        .to_path_buf();
    let mut console = Console::new(fleet.clone());
    console.load_prefs();

    // The session this console serves; its row (and cursors) may already be
    // on disk from an earlier console. A remembered session — the uuid the
    // `lastSessionUuid` preference holds — re-anchors the console before the
    // first draw; the row it left open (`lastSession`) is restored within
    // whichever session opens.
    let mut key = resolve_console_key(&fleet);
    let remembered = console.prefs().last_session_uuid.clone();
    if let Some(remembered) = &remembered
        && let Some(session) = crate::orch::session::session_by_key(fleet.root(), remembered)
    {
        key = session.key();
    }
    let mut poll = anchor_console(
        &fleet,
        &options,
        crate::paths::user_dir().as_deref(),
        &mut console,
        key,
    )
    .await;
    console.set_files(list_repo_files(&repo_root).await);

    // the row that was open when the console last closed; unknown keys fall
    // back to the orchestrator row, as ever
    if let Some(row) = console.prefs().last_session.clone() {
        console.select_target(&row);
    }

    poll.save_cursors();

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    // the transcript tail is two file reads at an offset, so it can run often;
    // reloading every run.json and the diff stats cannot
    let mut tail = tokio::time::interval(Duration::from_millis(TAIL_MS));
    let mut feed = tokio::time::interval(Duration::from_millis(FEED_MS));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    for timer in [&mut tick, &mut tail, &mut feed, &mut heartbeat] {
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    }
    // Reading `$NO_COLOR` and friends once per frame told us the same thing
    // every time; the environment does not change under a running console.
    let palette = Palette::detect();
    // Skipped only for a tail that found nothing: every other wake-up changes
    // something on screen, and a missed redraw is worse than a wasted one.
    let mut draw = true;

    loop {
        // the flash's expiry is a clock matter, not an event matter
        console.tick(now_ms());
        if draw {
            let feeds = Feeds {
                orch: &poll.orch,
                runs: &poll.runs,
            };
            terminal.draw(|frame| view::draw(frame, &mut console, &feeds, &palette))?;
        }
        draw = true;

        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(Event::Key(key))) => {
                    if is_interrupt(&key) {
                        break;
                    }
                    let effects = console.handle_key(key);
                    let switch = effects.iter().find_map(|effect| match effect {
                        Effect::SwitchSession(key) => Some(key.clone()),
                        _ => None,
                    });
                    let quit = effects.iter().any(|effect| matches!(effect, Effect::Quit));
                    let mouse = effects.iter().find_map(|effect| match effect {
                        Effect::SetMouseCapture(on) => Some(*on),
                        _ => None,
                    });
                    if let Some(on) = mouse {
                        set_mouse_capture(on);
                    }
                    console.execute_all(effects).await;
                    if let Some(new_key) = switch {
                        // the session we are leaving keeps its watcher
                        // cursors; the console re-anchors on the new one
                        poll.save_cursors();
                        poll = anchor_console(
                            &fleet,
                            &options,
                            crate::paths::user_dir().as_deref(),
                            &mut console,
                            new_key,
                        )
                        .await;
                    }
                    if quit {
                        break;
                    }
                }
                Some(Ok(Event::Mouse(mouse))) => {
                    // the wheel scrolls half a viewport, in both modes and
                    // inside the brief popup; other buttons stay unbound
                    let action = crate::tui::keys::map_mouse(mouse);
                    if action != crate::tui::keys::KeyAction::Ignored {
                        let effects = console.handle_action(action);
                        console.execute_all(effects).await;
                    }
                }
                Some(Ok(Event::Paste(text))) => {
                    console.paste(&text);
                }
                // resize redraws on the next pass; focus is unused
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            // the clock: ages, activity lines and the elapsed counters move
            _ = tick.tick() => {}
            _ = tail.tick() => {
                draw = poll.tail_events(&mut console);
            }
            _ = feed.tick() => {
                poll.reconcile_session();
                poll.reload_runs();
                poll.reload_orchestrator();
                // the session may have gained an alias (the orchestrator
                // derives one from its first prompt): the console follows
                // where it lives now
                console.orch_key = poll.key.clone();
                console.set_runs(poll.runs.clone());
                console.set_orchestrator_state(poll.orch.clone());
                console.set_capabilities(poll.caps.clone());
                console.set_model_questions(crate::route::pending_questions(&fleet));
                poll.tail_events(&mut console);
                poll.forward_fleet_events(&mut console).await;
                poll.refresh_diff_stats(&mut console).await;
            }
            _ = heartbeat.tick() => lock.refresh(),
        }
    }
    // the cursors outlive the console: a restart picks up where this left off
    poll.save_cursors();
    Ok(ExitCode::Ok)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_fleet() -> (std::path::PathBuf, FleetPaths) {
        let dir = std::env::temp_dir().join(format!(
            "parl-tui-runtime-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), FleetPaths::new(dir))
    }

    #[test]
    fn the_lock_refuses_a_live_second_console_and_expires_a_stale_one() {
        let (_dir, fleet) = tmp_fleet();
        let path = fleet.console_lock();

        // no lock at all: acquirable
        assert_eq!(active_lock(&path), None);

        // a fresh foreign lock is refused
        std::fs::write(
            &path,
            json!({ "pid": u64::from(std::process::id()) + 1, "ts": now_iso() }).to_string(),
        )
        .unwrap();
        assert_eq!(active_lock(&path), Some(u64::from(std::process::id()) + 1));

        // a stale lock is a crashed console, not a live one
        let stale = json!({
            "pid": u64::from(std::process::id()) + 1,
            "ts": crate::util::now_iso(),
        });
        let stale = match &stale {
            Value::Object(map) => {
                let mut map = map.clone();
                map.insert("ts".into(), json!("2026-01-01T00:00:00.000Z"));
                Value::Object(map)
            }
            _ => unreachable!(),
        };
        std::fs::write(&path, stale.to_string()).unwrap();
        assert_eq!(active_lock(&path), None);

        // a malformed lock does not wedge the console either
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(active_lock(&path), None);
    }

    #[test]
    fn acquiring_writes_the_lock_and_dropping_removes_it_if_ours() {
        let (_dir, fleet) = tmp_fleet();
        let path = fleet.console_lock();
        {
            let lock = ConsoleLock::acquire(&fleet).unwrap();
            lock.refresh();
            let raw = std::fs::read_to_string(&path).unwrap();
            let value: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                value.get("pid").unwrap().as_u64(),
                Some(u64::from(process_id()))
            );
        }
        assert!(!path.exists(), "our own lock is removed on drop");
    }

    #[test]
    fn dropping_leaves_a_taken_over_lock_alone() {
        let (_dir, fleet) = tmp_fleet();
        let path = fleet.console_lock();
        let foreign = u64::from(process_id()) + 1;
        std::fs::write(
            &path,
            json!({ "pid": foreign, "ts": now_iso() }).to_string(),
        )
        .unwrap();
        // simulate a stale lock being taken over: active_lock says none
        assert_eq!(
            active_lock(&path),
            Some(foreign),
            "fresh foreign lock is live"
        );
        // ...but a ConsoleLock dropped over a foreign file must not delete it
        let lock = ConsoleLock { path: path.clone() };
        drop(lock);
        assert!(path.exists());
    }

    #[test]
    fn ctrl_c_is_the_interrupt_and_plain_c_is_not() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_interrupt(&ctrl_c));
        assert!(!is_interrupt(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        // release events never count
        let released = KeyEvent {
            kind: KeyEventKind::Release,
            ..KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        };
        assert!(!is_interrupt(&released));
    }

    #[test]
    fn attaching_leaves_the_running_monitors_launch_record_alone() {
        let (_dir, fleet) = tmp_fleet();
        // a live monitor that was launched with its own flags: the session
        // row the console resolves is that one
        let mut store = crate::orch::session::FleetSessions::new();
        let mut record = crate::orch::session::OrchestratorSession::new("/repo");
        record.launch.model = Some("sonnet".into());
        record.pid = Some(std::process::id() as i32);
        let key = record.key();
        let key_uuid = key.uuid;
        store.upsert(record);
        crate::orch::session::save(fleet.root(), &mut store).unwrap();
        std::fs::create_dir_all(fleet.orchestrator_dir(&key)).unwrap();
        let state = OrchestratorState {
            pid: Some(std::process::id() as i32),
            ..OrchestratorState::default()
        };
        crate::util::atomic_write_json(&fleet.orchestrator_state(&key), &state).unwrap();

        // our own pid is alive: attach, and the recorded flags are untouched
        // — even though this console was opened with a different model
        assert!(!ensure_orchestrator(&fleet, &tui_options(Some("fable")), None, &key).unwrap());
        let store = crate::orch::session::load(fleet.root()).unwrap();
        let record = &store.sessions[&key_uuid];
        assert_eq!(record.launch.model.as_deref(), Some("sonnet"));
    }

    /// A `TuiOptions` with just the fields a test names; `main.rs` builds it
    /// verbatim, so the field set is the frozen contract.
    fn tui_options(model: Option<&str>) -> TuiOptions {
        TuiOptions {
            cwd: None,
            model: model.map(str::to_string),
            permission_mode: None,
            remote_control: None,
            fresh: false,
            budget: None,
            progress_events: false,
        }
    }

    #[test]
    fn the_spawned_monitor_is_pinned_to_the_anchored_session_not_the_mru() {
        let (_dir, fleet) = tmp_fleet();
        // two sessions; alpha is the most recently stamped row
        let alpha = crate::orch::session::create_session(fleet.root(), Some("alpha")).unwrap();
        let beta = crate::orch::session::create_session(fleet.root(), Some("beta")).unwrap();
        let mut store = crate::orch::session::load(fleet.root()).unwrap();
        store.sessions.get_mut(&alpha.uuid).unwrap().last_used_at =
            "2099-01-01T00:00:00.000Z".into();
        crate::orch::session::save(fleet.root(), &mut store).unwrap();
        assert_eq!(
            resolve_console_key(&fleet).uuid,
            alpha.uuid,
            "alpha is the most recently used row"
        );

        // a console anchored on beta spawns a monitor for beta, not for
        // whichever row the stamping race happened to leave newest
        let args = monitor_argv(&fleet, &beta.key());
        let at = args
            .iter()
            .position(|arg| arg == "--session")
            .expect("the spawn names a session");
        assert_eq!(
            args[at + 1],
            beta.uuid.to_string(),
            "pinned to the anchored session: {args:?}"
        );
        // the legacy default key spawns without the flag (the monitor
        // resolves the most recently used row on its own)
        let args = monitor_argv(&fleet, &SessionKey::default());
        assert!(!args.iter().any(|arg| arg == "--session"), "{args:?}");
    }

    #[test]
    fn spawning_a_monitor_records_the_launch_flags_in_the_session_store() {
        let (_dir, fleet) = tmp_fleet();
        // no monitor alive: the spawn path records the flags into the
        // session row the console serves
        let key = resolve_console_key(&fleet);
        let mut options = tui_options(Some("fable"));
        options.budget = Some(" 2.5 ".into());
        options.permission_mode = Some("acceptEdits".into());
        options.remote_control = Some(String::new());
        options.fresh = true;
        assert!(ensure_orchestrator(&fleet, &options, None, &key).unwrap());
        let store = crate::orch::session::load(fleet.root()).unwrap();
        let record = &store.sessions[&key.uuid];
        assert_eq!(record.launch.model.as_deref(), Some("fable"));
        assert_eq!(record.launch.budget_usd, Some(2.5));
        assert_eq!(
            record.launch.permission_mode.as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(record.launch.remote_control.as_deref(), Some(""));
        assert_eq!(record.launch.fresh, Some(true));
    }

    #[test]
    fn a_launch_record_without_flags_reads_as_claude_defaults() {
        let (_dir, fleet) = tmp_fleet();
        let key = resolve_console_key(&fleet);
        assert!(ensure_orchestrator(&fleet, &tui_options(None), None, &key).unwrap());
        let store = crate::orch::session::load(fleet.root()).unwrap();
        let record = &store.sessions[&key.uuid];
        assert_eq!(record.launch.model, None);
        assert_eq!(record.launch.budget_usd, None, "no budget: no dollars");
        assert_eq!(record.launch.fresh, Some(false));
    }

    #[test]
    fn the_user_config_supplies_the_orchestrator_model_unless_an_explicit_flag_wins() {
        let (_dir, fleet) = tmp_fleet();
        let key = resolve_console_key(&fleet);
        // A fabricated `~/.parl` with an `[orchestrator] model`; injected, so
        // nothing resolves the machine's real home.
        let user_root = std::env::temp_dir().join(format!(
            "parl-tui-user-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        let user_dir = user_root.join(".parl");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("config.toml"),
            "[orchestrator]\nmodel = \"claude-fable-5\"\n",
        )
        .unwrap();

        // No explicit flag: the config model reaches the launch record.
        assert!(ensure_orchestrator(&fleet, &tui_options(None), Some(&user_dir), &key).unwrap());
        let store = crate::orch::session::load(fleet.root()).unwrap();
        let record = &store.sessions[&key.uuid];
        assert_eq!(record.launch.model.as_deref(), Some("claude-fable-5"));
        // An explicit flag still wins over the config.
        assert!(
            ensure_orchestrator(&fleet, &tui_options(Some("opus")), Some(&user_dir), &key).unwrap()
        );
        let store = crate::orch::session::load(fleet.root()).unwrap();
        let record = &store.sessions[&key.uuid];
        assert_eq!(record.launch.model.as_deref(), Some("opus"));
    }

    // -- the watcher seam ---------------------------------------------------

    /// A temp fleet with one live worker run: `run.json`, an empty
    /// `events.jsonl`, and the session row the console writes into.
    fn fleet_with_run(name: &str) -> (tempfile::TempDir, FleetPaths, String, SessionKey) {
        let tmp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let fleet = FleetPaths::new(tmp.path().join(".parl"));
        let key = resolve_console_key(&fleet);
        std::fs::create_dir_all(fleet.orchestrator_dir(&key)).unwrap();
        let run_id = format!("{name}-20260830000000");
        let run_dir = fleet.root().join("runs").join(&run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut state = crate::fleet::run::RunState::new(
            fleet.root().to_string_lossy().as_ref(),
            &run_id,
            name,
            tmp.path().to_string_lossy().as_ref(),
            "brief",
            None,
            Some(format!("parl/{name}-1234567")),
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
        state.status = crate::fleet::run::RunStatus::Running;
        state.pid = Some(std::process::id() as i32);
        // the run belongs to the session the console serves, so the rail's
        // ownership filter keeps it visible
        state.orchestrator_id = Some(key.uuid);
        crate::fleet::run::save_state(&run_dir, &state).unwrap();
        std::fs::write(run_dir.join("events.jsonl"), "").unwrap();
        (tmp, fleet, run_id, key)
    }

    fn poll_for(
        fleet: &FleetPaths,
        key: &SessionKey,
        cursors: HashMap<String, crate::orch::session::RunCursor>,
    ) -> Poll {
        Poll::new(
            fleet.clone(),
            key.clone(),
            FleetWatcher::new(FleetWatcherOptions {
                fleet_dir: fleet.root().to_path_buf(),
                cursors,
                ..FleetWatcherOptions::default()
            }),
        )
    }

    fn settle(fleet: &FleetPaths, run_id: &str, question: &serde_json::Value) {
        let run_dir = fleet.root().join("runs").join(run_id);
        let mut state = crate::fleet::run::load_state(&run_dir).unwrap();
        state.status = crate::fleet::run::RunStatus::Settled;
        state.last_assistant_text = Some("Done: wrote the auth module".into());
        crate::fleet::run::save_state(&run_dir, &state).unwrap();
        crate::util::append_json_line(&run_dir.join("events.jsonl"), question).unwrap();
    }

    fn inbox_lines(fleet: &FleetPaths, key: &SessionKey) -> Vec<String> {
        std::fs::read_to_string(fleet.orchestrator_inbox(key))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn count_kind(fleet: &FleetPaths, key: &SessionKey, kind: &str) -> usize {
        // the payload's quotes are JSON-escaped on disk; unescape before
        // matching so the count is of what the orchestrator reads
        inbox_lines(fleet, key)
            .iter()
            .filter(|line| {
                line.replace("\\\"", "\"")
                    .contains(&format!("<fleet-event kind=\"{kind}\""))
            })
            .count()
    }

    #[tokio::test]
    async fn fleet_events_forward_as_one_batch_and_cursors_persist() {
        let (_tmp, fleet, run_id, key) = fleet_with_run("auth");
        let mut console = Console::new(fleet.clone());
        console.orch_key = key.clone();
        let mut poll = poll_for(&fleet, &key, HashMap::new());
        poll.watcher.start(false);

        // a running worker is not news
        poll.forward_fleet_events(&mut console).await;
        assert!(inbox_lines(&fleet, &key).is_empty());

        // the worker settles and asks a question: one batch, forwarded
        settle(
            &fleet,
            &run_id,
            &json!(
                {"type":"worker_question","questionId":"q_1","question":"bcrypt or argon2?"}
            ),
        );
        poll.forward_fleet_events(&mut console).await;
        let lines = inbox_lines(&fleet, &key);
        assert_eq!(count_kind(&fleet, &key, "question"), 1, "{lines:?}");
        assert_eq!(count_kind(&fleet, &key, "settled"), 1, "{lines:?}");
        // the transcript shows the batch as the ⚑ block the renderer draws
        assert!(
            console
                .orchestrator_transcript()
                .blocks()
                .iter()
                .any(|block| block.text.starts_with('⚑') && block.text.contains("question auth"))
        );

        // the forwarded cursors are durable: a restarted console continues
        let store = crate::orch::session::load(fleet.root()).unwrap();
        let cursor = &store.sessions[&key.uuid].watcher.cursors[&run_id];
        assert!(
            cursor.events_offset > 0,
            "the consumed events are remembered"
        );
        assert_eq!(cursor.last_view.as_deref(), Some("settled"));
    }

    #[tokio::test]
    async fn a_reopened_console_with_saved_cursors_does_not_replay() {
        let (_tmp, fleet, run_id, key) = fleet_with_run("db");
        let mut console = Console::new(fleet.clone());
        console.orch_key = key.clone();
        let mut poll = poll_for(&fleet, &key, HashMap::new());
        poll.watcher.start(true); // attaching: a snapshot goes out
        settle(
            &fleet,
            &run_id,
            &json!({"type":"worker_question","questionId":"q_1","question":"which db?"}),
        );
        poll.forward_fleet_events(&mut console).await;
        assert_eq!(count_kind(&fleet, &key, "question"), 1);

        // a fresh console over the persisted cursors: the snapshot may go
        // out again, but what the orchestrator already heard does not repeat
        let cursors = crate::orch::session::load(fleet.root()).unwrap().sessions[&key.uuid]
            .watcher
            .cursors
            .clone();
        let mut reopened = poll_for(&fleet, &key, cursors);
        reopened.watcher.start(true);
        let mut fresh_console = Console::new(fleet.clone());
        fresh_console.orch_key = key.clone();
        reopened.forward_fleet_events(&mut fresh_console).await;
        assert_eq!(count_kind(&fleet, &key, "question"), 1, "no replay");
        // the snapshot is the only new message, and it names the live run
        let lines = inbox_lines(&fleet, &key);
        assert_eq!(count_kind(&fleet, &key, "snapshot"), 2, "{lines:?}");
        assert!(
            lines
                .last()
                .is_some_and(|line| line.contains("db (settled)")),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn watcher_batches_keep_order_across_polls() {
        let (_tmp, fleet, run_id, key) = fleet_with_run("api");
        let mut console = Console::new(fleet.clone());
        console.orch_key = key.clone();
        let mut poll = poll_for(&fleet, &key, HashMap::new());
        poll.watcher.start(false);
        settle(
            &fleet,
            &run_id,
            &json!({"type":"worker_question","questionId":"q_1","question":"rest or grpc?"}),
        );
        poll.forward_fleet_events(&mut console).await;
        // nothing new: the same consumed lines are not queued twice
        poll.forward_fleet_events(&mut console).await;
        assert_eq!(count_kind(&fleet, &key, "question"), 1);
        assert_eq!(count_kind(&fleet, &key, "settled"), 1);
    }

    // -- the dashboard's diff stat -------------------------------------------

    #[test]
    fn compact_stat_reduces_git_stat_output_to_plus_minus() {
        let multiline =
            " hello.rs | 12 +++++++-----\n 1 file changed, 12 insertions(+), 3 deletions(-)";
        assert_eq!(compact_stat(multiline).as_deref(), Some("+12 −3"));
        assert_eq!(
            compact_stat(" 2 files changed, 1 insertion(+), 5 deletions(-)").as_deref(),
            Some("+1 −5")
        );
        assert_eq!(
            compact_stat(" 1 file changed, 4 insertions(+)").as_deref(),
            Some("+4 −0")
        );
        assert_eq!(
            compact_stat(" 1 file changed, 2 deletions(-)").as_deref(),
            Some("+0 −2")
        );
        // nothing to show: no changes, no worktree, empty
        assert_eq!(compact_stat("(no changes)"), None);
        assert_eq!(
            compact_stat("not applicable (run has no isolated worktree)"),
            None
        );
        assert_eq!(compact_stat(""), None);
    }

    // -- sessions ------------------------------------------------------------

    /// A flock: two sessions, one run owned by each, both monitors attached
    /// (state.json carries this process's pid, so anchoring never spawns).
    async fn session_flock() -> (
        tempfile::TempDir,
        FleetPaths,
        (OrchestratorSession, String),
        (OrchestratorSession, String),
    ) {
        let tmp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let fleet = FleetPaths::new(tmp.path().join(".parl"));
        let first = crate::orch::session::create_session(fleet.root(), Some("alpha")).unwrap();
        let second = crate::orch::session::create_session(fleet.root(), Some("beta")).unwrap();
        for session in [&first, &second] {
            std::fs::create_dir_all(fleet.orchestrator_dir(&session.key())).unwrap();
            let state = OrchestratorState {
                pid: Some(std::process::id() as i32),
                ..OrchestratorState::default()
            };
            crate::util::atomic_write_json(&fleet.orchestrator_state(&session.key()), &state)
                .unwrap();
        }
        let run = |run_id: &str, name: &str, owner: uuid::Uuid| {
            let mut state = crate::fleet::run::RunState::new(
                fleet.root().to_string_lossy().as_ref(),
                run_id,
                name,
                tmp.path().to_string_lossy().as_ref(),
                "brief",
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
            state.status = crate::fleet::run::RunStatus::Running;
            state.pid = Some(std::process::id() as i32);
            state.orchestrator_id = Some(owner);
            let dir = fleet.root().join("runs").join(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            crate::fleet::run::save_state(&dir, &state).unwrap();
        };
        run("alpha-20260830000000", "auth", first.uuid);
        run("beta-20260830000000", "db", second.uuid);
        (
            tmp,
            fleet,
            (first, "alpha-20260830000000".into()),
            (second, "beta-20260830000000".into()),
        )
    }

    #[tokio::test]
    async fn reload_runs_shows_only_the_active_sessions_workers() {
        let (_tmp, fleet, first, second) = session_flock().await;
        let key = first.0.key();
        let mut poll = poll_for(&fleet, &key, HashMap::new());
        poll.reload_runs();
        assert_eq!(
            poll.runs.len(),
            1,
            "the other session's run stays off the rail"
        );
        assert_eq!(poll.runs[0].run_id, first.1);

        // the same poll pointed at the other session sees only its own
        poll.key = second.0.key();
        poll.reload_runs();
        assert_eq!(poll.runs.len(), 1);
        assert_eq!(poll.runs[0].run_id, "beta-20260830000000");
    }

    #[tokio::test]
    async fn reconcile_session_follows_a_newly_derived_alias() {
        let (_tmp, fleet, _run_id, key) = fleet_with_run("auth");
        let mut poll = poll_for(&fleet, &key, HashMap::new());
        assert_eq!(poll.key.alias, None);
        // the orchestrator derives an alias and saves the row
        let mut store = crate::orch::session::load(fleet.root()).unwrap();
        store.sessions.get_mut(&key.uuid).unwrap().alias = Some("add-auth".into());
        crate::orch::session::save(fleet.root(), &mut store).unwrap();
        poll.reconcile_session();
        assert_eq!(poll.key.alias.as_deref(), Some("add-auth"));
        assert_eq!(poll.key.uuid, key.uuid);
    }

    #[tokio::test]
    async fn anchoring_switches_the_console_between_sessions_scoping_the_rail() {
        let (_tmp, fleet, first, _second) = session_flock().await;
        let mut console = Console::new(fleet.clone());
        let first_key = first.0.key();
        let second_key = crate::orch::session::session_by_key(fleet.root(), "beta")
            .unwrap()
            .key();
        anchor_console(
            &fleet,
            &tui_options(None),
            None,
            &mut console,
            first_key.clone(),
        )
        .await;
        assert_eq!(console.orch_key, first_key);
        let names: Vec<String> = console.rows().iter().map(|row| row.name.clone()).collect();
        assert_eq!(
            names,
            vec!["orchestrator · alpha", "auth"],
            "only alpha's own worker"
        );

        // a switch re-anchors on the other session: fresh row set, fresh
        // transcript, and the console follows
        console.ingest_orchestrator_record(
            &crate::orch::records::OrchestratorEvent::Notice {
                text: "line from alpha".into(),
                error: None,
            }
            .to_record(),
        );
        anchor_console(
            &fleet,
            &tui_options(None),
            None,
            &mut console,
            second_key.clone(),
        )
        .await;
        assert_eq!(console.orch_key, second_key);
        let names: Vec<String> = console.rows().iter().map(|row| row.name.clone()).collect();
        assert_eq!(
            names,
            vec!["orchestrator · beta", "db"],
            "only beta's own worker"
        );
        assert!(
            console
                .orchestrator_transcript()
                .blocks()
                .iter()
                .all(|block| !block.text.contains("line from alpha")),
            "alpha's transcript does not bleed into beta"
        );
        assert!(
            console
                .orchestrator_transcript()
                .blocks()
                .iter()
                .all(|b| { b.text.contains("attaching") || b.text.contains("monitor started") }),
            "only the anchor notice, nothing from alpha"
        );
    }

    #[tokio::test]
    async fn diff_stats_reach_the_dashboard_throttled() {
        let tmp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let root = tmp.path().to_path_buf();
        let git = |args: &[&str], cwd: &std::path::Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"], &root);
        std::fs::write(root.join(".gitignore"), ".parl/\n").unwrap();
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git(&["add", "."], &root);
        git(&["commit", "-qm", "seed"], &root);

        let fleet = FleetPaths::new(root.join(".parl"));
        let key = resolve_console_key(&fleet);
        std::fs::create_dir_all(fleet.orchestrator_dir(&key)).unwrap();
        let run_id = "auth-20260830000000";
        let info = crate::git::ensure_worktree(
            &root,
            &fleet.root().join("worktrees"),
            run_id,
            "auth",
            None,
        )
        .await
        .unwrap();
        let run_dir = fleet.root().join("runs").join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut state = crate::fleet::run::RunState::new(
            fleet.root().to_string_lossy().as_ref(),
            run_id,
            "auth",
            root.to_string_lossy().as_ref(),
            "brief",
            None,
            Some(info.branch.clone()),
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
        state.status = crate::fleet::run::RunStatus::Running;
        state.pid = Some(std::process::id() as i32);
        state.worktree = Some(info.worktree_path.to_string_lossy().into_owned());
        state.base_commit = Some(info.base_commit.clone());
        state.orchestrator_id = Some(key.uuid);
        crate::fleet::run::save_state(&run_dir, &state).unwrap();

        // the worker commits one file: the row carries +1 −0
        std::fs::write(info.worktree_path.join("auth.rs"), "fn main() {}\n").unwrap();
        git(&["add", "."], &info.worktree_path);
        git(&["commit", "-qm", "auth"], &info.worktree_path);

        let mut console = Console::new(fleet.clone());
        let mut poll = poll_for(&fleet, &key, HashMap::new());
        // the run belongs to this session: the rail's ownership filter is
        // precisely what this test exercises
        state.orchestrator_id = Some(key.uuid);
        crate::fleet::run::save_state(&run_dir, &state).unwrap();
        poll.reload_runs();
        console.set_runs(poll.runs.clone());
        poll.refresh_diff_stats(&mut console).await;
        let row = console.rows().iter().find(|row| row.key == run_id).unwrap();
        assert_eq!(row.diff_stat.as_deref(), Some("+1 −0"));

        // more committed work inside the throttle window: not recomputed yet
        std::fs::write(info.worktree_path.join("more.rs"), "fn more() {}\n").unwrap();
        git(&["add", "."], &info.worktree_path);
        git(&["commit", "-qm", "more"], &info.worktree_path);
        poll.refresh_diff_stats(&mut console).await;
        let row = console.rows().iter().find(|row| row.key == run_id).unwrap();
        assert_eq!(row.diff_stat.as_deref(), Some("+1 −0"), "throttled");
    }

    #[tokio::test]
    async fn a_run_without_a_worktree_shows_no_diff_stat() {
        let (_tmp, fleet, _run_id, key) = fleet_with_run("bare");
        let mut console = Console::new(fleet.clone());
        let mut poll = poll_for(&fleet, &key, HashMap::new());
        poll.reload_runs();
        console.set_runs(poll.runs.clone());
        poll.refresh_diff_stats(&mut console).await;
        let row = console.rows().last().unwrap();
        assert_eq!(row.diff_stat, None);
    }
}
