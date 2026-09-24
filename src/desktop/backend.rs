//! The desktop's tokio half: one [`Driver`] (the same console loop the TUI
//! runs), plus what only the desktop shows — every session, every session's
//! workers for the Board, and the selected worker's patch. The window sends
//! [`UiCmd`]s in and reads the latest [`Snapshot`] out; it never touches a
//! file or git itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::KeyEvent;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use crate::fleet::run::{DerivedView, RunState, derive_view, is_alive};
use crate::orch::protocol::PermissionRequest;
use crate::orch::session::{MonitorHealth, list_sessions, monitor_health};
use crate::patch::Patch;
use crate::paths::{FleetPaths, SessionKey};
use crate::route::ModelQuestion;
use crate::tui::app::{Flash, Overlay, TuiOptions};
use crate::tui::completions::CompletionContext;
use crate::tui::driver::{ConsoleLock, Driver, FEED_MS, HEARTBEAT_MS, TAIL_MS};
use crate::tui::keys::KeyAction;
use crate::tui::model::{DashboardRow, RunRow, SessionTarget, session_display_name, worker_row};
use crate::tui::transcript::Block;
use crate::util::now_ms;

/// Sessions and the Board reread every run.json: slower than the feed.
const FLEET_MS: u64 = 1_000;
/// The selected worker's patch, while it runs.
const PATCH_MS: u64 = 3_000;
/// The clock: flash expiry and the activity line's elapsed seconds.
const CLOCK_MS: u64 = 250;

/// What the window asks for.
#[derive(Debug, Clone)]
pub enum UiCmd {
    /// The composer's line, exactly as the TUI's enter would send it.
    Submit(String),
    /// A key while a sheet (a console overlay) has focus, read the way the
    /// TUI reads it.
    Key(KeyEvent),
    Action(KeyAction),
    Paste(String),
    /// Select a row: `orchestrator` or a run id.
    Select(String),
    /// Follow this worker's patch in the changes pane.
    Diff(Option<String>),
    /// Rename any session; an empty name clears it.
    RenameSession(uuid::Uuid, String),
    /// Merge a finished worker's branch into the checkout it came from.
    Merge(String),
    /// Show a path in Finder, or open a file in its default app.
    Reveal(std::path::PathBuf),
    /// The palette with only the models the selected session can switch to.
    OpenModels,
    /// The selected session's brief.
    OpenBrief,
    /// Reopen a pending approval or model choice.
    ReviewWaiting,
    Quit,
}

/// The four Board lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Started,
    Waiting,
    Failed,
    Finished,
}

impl Lane {
    /// Archived runs are on no lane: cleaned up, off the Board.
    #[must_use]
    pub const fn of(view: DerivedView) -> Option<Self> {
        match view {
            DerivedView::Starting | DerivedView::Running => Some(Self::Started),
            DerivedView::Blocked => Some(Self::Waiting),
            DerivedView::Error | DerivedView::Dead => Some(Self::Failed),
            DerivedView::Settled | DerivedView::Stopped => Some(Self::Finished),
            DerivedView::Archived => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionItem {
    pub key: SessionKey,
    pub name: String,
    pub short: String,
    pub health: MonitorHealth,
    pub lanes: Vec<Lane>,
    pub last_used: String,
}

/// One worker, wherever it lives: the Board's card and the sidebar's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerItem {
    pub session: SessionKey,
    pub session_name: String,
    pub row: DashboardRow,
    pub lane: Lane,
    pub model: Option<String>,
    pub question: Option<String>,
    pub error: Option<String>,
    pub worktree: Option<String>,
    pub thinking: Option<String>,
    pub thinking_levels: Vec<String>,
    /// Settled with a branch: something to merge.
    pub mergeable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatchView {
    pub run_id: String,
    pub name: String,
    pub base: String,
    pub patch: Arc<Patch>,
    pub error: Option<String>,
}

/// The orchestrator's facts for the chat header.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub model: Option<String>,
    pub cost_usd: f64,
    pub effort: Option<String>,
    pub permission_mode: String,
    pub turn_active: bool,
    pub exited: bool,
}

/// Everything the window draws, as of one moment.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub closed: bool,
    /// The open session; none until the user starts or picks one.
    pub key: Option<SessionKey>,
    pub sessions: Vec<SessionItem>,
    /// This session's rows: the orchestrator first, then its workers.
    pub rows: Vec<DashboardRow>,
    pub workers: Vec<WorkerItem>,
    pub selected: String,
    pub blocks: Arc<Vec<Block>>,
    pub partial: Option<String>,
    pub facts: Facts,
    pub overlay: Option<Overlay>,
    pub flash: Option<Flash>,
    pub activity: Option<String>,
    pub prompt: String,
    pub completion: CompletionContext,
    pub requests: Vec<PermissionRequest>,
    pub model_questions: Vec<ModelQuestion>,
    /// Every session's workers.
    pub board: Vec<WorkerItem>,
    pub patch: Option<PatchView>,
}

impl Snapshot {
    /// The open session's uuid, if one is open.
    #[must_use]
    pub fn current(&self) -> Option<uuid::Uuid> {
        self.key.as_ref().map(|key| key.uuid)
    }
}

/// Run the console for the window until it quits or the window goes away.
pub async fn serve(
    fleet: FleetPaths,
    options: TuiOptions,
    lock: ConsoleLock,
    mut cmds: mpsc::UnboundedReceiver<UiCmd>,
    out: watch::Sender<Arc<Snapshot>>,
) {
    let mut driver = Driver::open(fleet, options).await;
    let mut side = Side::default();
    side.reload(driver.fleet());
    side.refresh_patch().await;
    let _ = out.send(Arc::new(side.snapshot(&mut driver)));

    let mut tail = interval(TAIL_MS);
    let mut feed = interval(FEED_MS);
    let mut fleet_tick = interval(FLEET_MS);
    let mut patch_tick = interval(PATCH_MS);
    let mut clock = interval(CLOCK_MS);
    let mut heartbeat = interval(HEARTBEAT_MS);
    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                let before = driver.key().map(|key| key.uuid);
                if side.handle(&mut driver, cmd).await {
                    break;
                }
                // a `/session` switch: the sidebars follow at once
                if driver.key().map(|key| key.uuid) != before {
                    side.reload(driver.fleet());
                }
            }
            _ = tail.tick() => {
                if !driver.tail() {
                    continue;
                }
            }
            _ = feed.tick() => driver.feed().await,
            _ = fleet_tick.tick() => side.reload(driver.fleet()),
            _ = patch_tick.tick() => {
                if !side.patch_is_live() {
                    continue;
                }
                side.refresh_patch().await;
            }
            _ = clock.tick() => driver.console.tick(now_ms()),
            _ = heartbeat.tick() => lock.refresh(),
        }
        let _ = out.send(Arc::new(side.snapshot(&mut driver)));
    }
    driver.close();
    drop(lock);
    let _ = out.send(Arc::new(Snapshot {
        closed: true,
        ..Snapshot::default()
    }));
}

fn interval(ms: u64) -> tokio::time::Interval {
    let mut timer = tokio::time::interval(Duration::from_millis(ms));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    timer
}

/// The desktop's own view of the fleet, beside the driver's.
#[derive(Default)]
struct Side {
    sessions: Vec<SessionItem>,
    board: Vec<WorkerItem>,
    /// Every run's state, by id: where the patch finds its worktree.
    states: HashMap<String, RunState>,
    patch_target: Option<String>,
    patch: Option<PatchView>,
}

impl Side {
    /// Returns whether the window asked to close.
    async fn handle(&mut self, driver: &mut Driver, cmd: UiCmd) -> bool {
        match cmd {
            UiCmd::Submit(text) => {
                let effects = driver.console.submit(&text);
                return driver.apply(effects).await;
            }
            UiCmd::Key(key) => {
                let effects = driver.console.handle_key(key);
                return driver.apply(effects).await;
            }
            UiCmd::Action(action) => {
                let effects = driver.console.handle_action(action);
                return driver.apply(effects).await;
            }
            UiCmd::Paste(text) => driver.console.paste(&text),
            UiCmd::Select(key) => {
                driver.console.select_target(&key);
            }
            UiCmd::Diff(run_id) => {
                self.patch_target = run_id;
                self.refresh_patch().await;
            }
            UiCmd::RenameSession(uuid, name) => {
                match crate::orch::session::rename_session(driver.fleet().root(), uuid, &name) {
                    Ok(session) => driver.console.toast(
                        format!(
                            "· renamed to {}",
                            session_display_name(session.alias.as_deref(), session.uuid)
                        ),
                        false,
                    ),
                    Err(err) => driver.console.notice(format!("! {err:#}"), true),
                }
                // the open session's key follows on the next feed tick
                driver.feed().await;
                self.reload(driver.fleet());
            }
            UiCmd::Merge(run_id) => self.merge(driver, &run_id).await,
            UiCmd::Reveal(path) => {
                // `open` on a directory shows it in Finder; on a file it
                // hands it to its default app
                if let Err(err) = tokio::process::Command::new("open").arg(&path).spawn() {
                    driver
                        .console
                        .notice(format!("! opening {}: {err}", path.display()), true);
                }
            }
            UiCmd::OpenModels => {
                let effects = driver.console.open_model_palette();
                return driver.apply(effects).await;
            }
            UiCmd::ReviewWaiting => driver.console.review_waiting(),
            UiCmd::OpenBrief => {
                let effects = driver.console.open_brief();
                return driver.apply(effects).await;
            }
            UiCmd::Quit => return true,
        }
        false
    }

    /// Merge a finished worker into the checkout it was cut from, the way
    /// `pilotfish merge` does; its words become notices.
    async fn merge(&mut self, driver: &mut Driver, run_id: &str) {
        let root = driver.fleet().root().to_path_buf();
        let repo = root.parent().unwrap_or(&root).to_path_buf();
        let fleet_dir = root.to_string_lossy().into_owned();
        match crate::ops::integrate::merge_core_with_env(
            run_id,
            Some(&repo),
            false,
            Some(fleet_dir.as_str()),
        )
        .await
        {
            Ok(result) => {
                for line in result.out {
                    driver.console.notice(format!("· {line}"), false);
                }
                for line in result.err {
                    driver.console.notice(format!("! {line}"), true);
                }
            }
            Err(err) => driver.console.notice(format!("! merge: {err:#}"), true),
        }
        self.reload(driver.fleet());
        self.refresh_patch().await;
    }

    /// Every session and every run, reread.
    fn reload(&mut self, fleet: &FleetPaths) {
        let now = now_ms();
        self.states = crate::fleet::run::list_runs(fleet.root())
            .into_iter()
            .filter_map(|summary| {
                crate::fleet::run::load_state(&summary.run_dir)
                    .ok()
                    .map(|state| (summary.run_id, state))
            })
            .collect();
        let sessions = list_sessions(fleet.root());
        let names: HashMap<uuid::Uuid, (SessionKey, String)> = sessions
            .iter()
            .map(|s| {
                (
                    s.uuid,
                    (s.key(), session_display_name(s.alias.as_deref(), s.uuid)),
                )
            })
            .collect();
        let mut board: Vec<WorkerItem> = self
            .states
            .iter()
            .filter_map(|(run_id, state)| {
                let (session, session_name) = state
                    .orchestrator_id
                    .and_then(|owner| names.get(&owner).cloned())?;
                worker_item(run_id, state, session, session_name, now)
            })
            .collect();
        board.sort_by(|a, b| b.row.key.cmp(&a.row.key));
        self.sessions = sessions
            .iter()
            .map(|session| SessionItem {
                key: session.key(),
                name: session_display_name(session.alias.as_deref(), session.uuid),
                short: session.uuid.to_string().chars().take(7).collect(),
                health: monitor_health(session, is_alive, now),
                lanes: board
                    .iter()
                    .filter(|item| item.session.uuid == session.uuid)
                    .map(|item| item.lane)
                    .collect(),
                last_used: crate::util::parse_ts_ms(&session.last_used_at)
                    .map(|at| crate::util::format_age((now - at).max(0)))
                    .unwrap_or_default(),
            })
            .collect();
        self.board = board;
    }

    fn patch_is_live(&self) -> bool {
        self.patch_target
            .as_ref()
            .and_then(|run_id| self.states.get(run_id))
            .is_some_and(|state| !state.status.is_terminal())
    }

    async fn refresh_patch(&mut self) {
        let Some(run_id) = self.patch_target.clone() else {
            self.patch = None;
            return;
        };
        let Some(state) = self.states.get(&run_id) else {
            self.patch = None;
            return;
        };
        let base = state.diff_base().to_string();
        let (patch, error) = match state
            .worktree
            .as_deref()
            .map(std::path::PathBuf::from)
            .filter(|path| path.exists())
        {
            Some(worktree) => match crate::patch::load(&worktree, &base).await {
                Ok(patch) => (patch, None),
                Err(err) => (Patch::default(), Some(format!("{err:#}"))),
            },
            None => (
                Patch::default(),
                Some("This worker has no worktree of its own, so it has no diff.".into()),
            ),
        };
        let patch = Arc::new(patch);
        // an unchanged patch keeps its Arc: the pane's folds and scroll hold
        if let Some(current) = &self.patch
            && current.run_id == run_id
            && current.patch == patch
            && current.error == error
        {
            return;
        }
        self.patch = Some(PatchView {
            run_id,
            name: state.name.clone(),
            base: base.chars().take(7).collect(),
            patch,
            error,
        });
    }

    fn snapshot(&self, driver: &mut Driver) -> Snapshot {
        let now = now_ms();
        let key = driver.key().cloned();
        let orch = driver.orch().clone();
        let console = &mut driver.console;
        let rows = console.rows().to_vec();
        // the console's rows carry the diff stats its feed computed
        let workers = self
            .board
            .iter()
            .filter(|item| {
                key.as_ref()
                    .is_some_and(|key| item.session.uuid == key.uuid)
            })
            .map(|item| {
                let mut item = item.clone();
                if let Some(row) = rows.iter().find(|row| row.key == item.row.key) {
                    item.row = row.clone();
                }
                item
            })
            .collect();
        let selected = match console.selected_target() {
            SessionTarget::Orchestrator(_) => "orchestrator".to_string(),
            SessionTarget::Worker { run_id } => run_id,
        };
        let facts = Facts {
            model: console
                .orchestrator_transcript()
                .model()
                .map(str::to_string)
                .or(orch.model.clone()),
            cost_usd: orch.cost_usd,
            effort: console.effort().map(str::to_string),
            permission_mode: orch.permission_mode.clone(),
            turn_active: orch.turn_active,
            exited: orch.exited.is_some(),
        };
        let transcript = console.open_transcript();
        let blocks = Arc::new(transcript.blocks().to_vec());
        let partial = transcript.partial();
        Snapshot {
            closed: false,
            sessions: self.sessions.clone(),
            rows,
            workers,
            selected,
            blocks,
            partial,
            facts,
            overlay: console.overlay().cloned(),
            flash: console.chrome_flash().cloned(),
            activity: console.activity_line(now),
            prompt: console.composer_prompt(),
            completion: console.completion_context(),
            requests: orch.pending_requests,
            model_questions: console.model_questions().to_vec(),
            board: self.board.clone(),
            patch: self.patch.clone(),
            key,
        }
    }
}

fn worker_item(
    run_id: &str,
    state: &RunState,
    session: SessionKey,
    session_name: String,
    now: i64,
) -> Option<WorkerItem> {
    let view = derive_view(state, is_alive, now);
    let lane = Lane::of(view)?;
    let row = worker_row(
        &RunRow {
            run_id,
            state,
            diff_stat: None,
        },
        now,
    )?;
    Some(WorkerItem {
        session,
        session_name,
        mergeable: view == DerivedView::Settled && state.branch.is_some(),
        worktree: state.worktree.clone(),
        thinking: state.thinking_level.clone(),
        thinking_levels: crate::tui::app::worker_thinking_levels(state)
            .into_iter()
            .map(str::to_string)
            .collect(),
        row,
        lane,
        model: state.model_label().map(str::to_string),
        question: state
            .pending_question
            .as_ref()
            .map(|q| q.question.clone())
            .or_else(|| state.pending_dialog.as_ref().map(|d| d.question.clone())),
        error: state.error.clone(),
    })
}
