//! Run state -> fleet events: the watcher tails every run's state and events
//! and turns changes into `<fleet-event>` batches for the orchestrator,
//! keeping its cursors in `fleet.json` so a restarted watcher does not replay
//! what it already reported. A fresh watcher starts at the current end of
//! each file it has never seen.
//!
//! Ported from the TypeScript `src/fleet/watcher.ts`. Provenance is the
//! point: only console-originated answers and steers become
//! `answered_by_console` / `console_steer` events (the `from` field of the
//! run's `events.jsonl` records says who delivered them), so the orchestrator
//! reconciles the human's work instead of undoing it. One addition since the
//! TypeScript: pending pi extension dialogs (`worker_dialog` records) surface
//! to the orchestrator the same way a `fleet_ask` question does.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;
use uuid::Uuid;

use crate::fleet::event::{FleetEvent, FleetEventKind, last_line};
use crate::fleet::run::{self, DerivedView, RunState};
use crate::orch::session::RunCursor;
use crate::util::{now_ms, read_new_lines};

/// Terminal views: running/starting/blocked flapping is noise, so only these
/// transitions are reported.
const TERMINAL_VIEWS: [DerivedView; 4] = [
    DerivedView::Settled,
    DerivedView::Stopped,
    DerivedView::Error,
    DerivedView::Dead,
];

const fn kind_by_view(view: DerivedView) -> Option<FleetEventKind> {
    match view {
        DerivedView::Settled => Some(FleetEventKind::Settled),
        DerivedView::Stopped => Some(FleetEventKind::Stopped),
        DerivedView::Error => Some(FleetEventKind::Error),
        DerivedView::Dead => Some(FleetEventKind::Dead),
        _ => None,
    }
}

/// What the watcher needs beyond the fleet dir.
#[derive(Debug, Clone)]
pub struct FleetWatcherOptions {
    pub fleet_dir: PathBuf,
    /// Only the runs this session owns (`run.json`'s orchestratorId) are
    /// watched and reported. None watches every run — the single-session
    /// shape, kept for callers that predate multi-session fleets.
    pub owner: Option<Uuid>,
    /// Cursor state saved by an earlier watcher (from the session record), so
    /// a resumed watcher continues where it left off.
    pub cursors: HashMap<String, RunCursor>,
    /// Forward `worker_progress` notes (off by default; throttled when on).
    pub progress_events: bool,
    pub progress_throttle_ms: u64,
    /// Events per batch, applied by the caller that formats the batch.
    pub max_per_batch: usize,
}

impl Default for FleetWatcherOptions {
    fn default() -> Self {
        Self {
            fleet_dir: PathBuf::new(),
            owner: None,
            cursors: HashMap::new(),
            progress_events: false,
            progress_throttle_ms: 60_000,
            max_per_batch: 10,
        }
    }
}

/// One live run, as the watcher sees it.
#[derive(Debug, Clone)]
pub struct RunSnapshot {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub state: RunState,
    pub view: DerivedView,
}

/// The watcher: turns run state and run events into fleet events. Drive it
/// with [`FleetWatcher::tick`] on a poll interval, then drain
/// [`FleetWatcher::take_batch`] and send the rendered batch to the
/// orchestrator as one user message.
pub struct FleetWatcher {
    fleet_dir: PathBuf,
    owner: Option<Uuid>,
    cursors: HashMap<String, RunCursor>,
    progress_events: bool,
    progress_throttle_ms: u64,
    max_per_batch: usize,
    /// Events waiting to be batched.
    queued: Vec<FleetEvent>,
    /// Last forwarded progress note per run, for the throttle window.
    last_progress_at: HashMap<String, i64>,
}

impl FleetWatcher {
    /// A watcher over `fleet_dir`, continuing from saved cursors.
    #[must_use]
    pub fn new(options: FleetWatcherOptions) -> Self {
        Self {
            fleet_dir: options.fleet_dir,
            owner: options.owner,
            cursors: options.cursors,
            progress_events: options.progress_events,
            progress_throttle_ms: options.progress_throttle_ms,
            max_per_batch: options.max_per_batch,
            queued: Vec::new(),
            last_progress_at: HashMap::new(),
        }
    }

    /// The cursors to save with the session record.
    #[must_use]
    pub fn cursors(&self) -> HashMap<String, RunCursor> {
        self.cursors.clone()
    }

    /// Events per batch, for whoever renders the batch.
    #[must_use]
    pub const fn batch_limit(&self) -> usize {
        self.max_per_batch
    }

    /// Live runs right now, for the rail and for the `snapshot` event:
    /// every run when no owner is set, otherwise only the runs whose
    /// `run.json` records this session as their orchestrator.
    #[must_use]
    pub fn runs(&self) -> Vec<RunSnapshot> {
        let summaries = match self.owner {
            Some(owner) => run::list_runs_for_owner(&self.fleet_dir, owner),
            None => run::list_runs(&self.fleet_dir),
        };
        summaries
            .into_iter()
            .filter_map(|summary| {
                let state = run::load_state(&summary.run_dir).ok()?;
                if state.status == run::RunStatus::Archived {
                    return None;
                }
                Some(RunSnapshot {
                    view: run::derive_view(&state, run::is_alive, now_ms()),
                    run_id: summary.run_id,
                    run_dir: summary.run_dir,
                    state,
                })
            })
            .collect()
    }

    /// Begin watching. Runs the watcher has never seen start at the *current
    /// end* of their events file, so history is not replayed; known runs
    /// continue from their cursor. `snapshot` reports live runs on a resume.
    pub fn start(&mut self, snapshot: bool) {
        let live = self.runs();
        for run in &live {
            if self.cursors.contains_key(&run.run_id) {
                continue;
            }
            let size =
                std::fs::metadata(run.run_dir.join("events.jsonl")).map_or(0, |meta| meta.len());
            self.cursors.insert(
                run.run_id.clone(),
                RunCursor {
                    events_offset: size,
                    last_view: Some(run.view.to_string()),
                },
            );
        }
        if snapshot && !live.is_empty() {
            let summary = live
                .iter()
                .map(|r| {
                    Self::asking_question(&r.state).map_or_else(
                        || format!("{} ({})", r.state.name, r.view),
                        |question| format!("{} ({}, asking: {question})", r.state.name, r.view),
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            let count = live.len().to_string();
            self.queued.push(FleetEvent::new(
                FleetEventKind::Snapshot,
                "-",
                "fleet",
                vec![
                    ("runs".to_string(), Some(summary)),
                    ("count".to_string(), Some(count)),
                ],
            ));
        }
    }

    /// What a blocked run is waiting on: a `fleet_ask` question or a pi
    /// extension dialog. Either way the orchestrator should know.
    fn asking_question(state: &RunState) -> Option<String> {
        state
            .pending_question
            .as_ref()
            .map(|q| q.question.clone())
            .or_else(|| state.pending_dialog.as_ref().map(|d| d.question.clone()))
    }

    /// One poll pass over every run. Cursor-based: what was consumed is not
    /// replayed, and only arrivals in a terminal view are reported.
    pub fn tick(&mut self) {
        for run in self.runs() {
            // Work on a local copy: the cursor map is only touched between
            // reads, never held across a borrow of self.
            let mut cursor = self.cursors.entry(run.run_id.clone()).or_default().clone();
            let (events, offset) =
                read_new_lines(&self.events_path(&run.run_id), cursor.events_offset);
            cursor.events_offset = offset;
            for event in events {
                let Ok(event) = serde_json::from_str::<Value>(&event) else {
                    continue;
                };
                self.on_run_event(&run.run_id, &run.state, &event);
            }
            if Some(run.view.to_string()) != cursor.last_view {
                // Report only arrivals in a terminal view; running/starting/
                // blocked flapping is noise.
                if let Some(kind) = kind_by_view(run.view)
                    && TERMINAL_VIEWS.contains(&run.view)
                {
                    self.push(self.status_event(&run.run_id, &run.state, kind));
                }
                cursor.last_view = Some(run.view.to_string());
            }
            self.cursors.insert(run.run_id.clone(), cursor);
        }
    }

    /// Drain everything queued since the last drain: the batch. The caller
    /// renders it with `format_fleet_batch(events, watcher.batch_limit())`.
    pub fn take_batch(&mut self) -> Vec<FleetEvent> {
        std::mem::take(&mut self.queued)
    }

    fn events_path(&self, run_id: &str) -> PathBuf {
        self.fleet_dir
            .join("runs")
            .join(run_id)
            .join("events.jsonl")
    }

    fn push(&mut self, event: FleetEvent) {
        self.queued.push(event);
    }

    fn status_event(&self, run_id: &str, state: &RunState, kind: FleetEventKind) -> FleetEvent {
        let mut event = FleetEvent::new(
            kind,
            run_id,
            state.name.clone(),
            vec![
                ("status".to_string(), Some(kind.to_string())),
                ("branch".to_string(), state.branch.clone()),
                ("worktree".to_string(), state.worktree.clone()),
                ("error".to_string(), state.error.clone()),
                (
                    "last".to_string(),
                    last_line(state.last_assistant_text.as_deref()),
                ),
            ],
        );
        if kind == FleetEventKind::Settled {
            // the path and whether it is there: a settled worker without a
            // report is worth noticing before anyone runs fleet_report
            let report = crate::fleet::report::report_path(&self.fleet_dir, &state.id);
            event.set(
                "report",
                format!(
                    "{} ({})",
                    report.display(),
                    if report.is_file() {
                        "present"
                    } else {
                        "missing"
                    }
                ),
            );
        }
        event
    }

    /// One `events.jsonl` line from a run, turned into fleet events.
    fn on_run_event(&mut self, run_id: &str, state: &RunState, ev: &Value) {
        let Some(kind) = ev.get("type").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "worker_question" => {
                let question_id = string_field(ev, "questionId");
                let question = string_field(ev, "question");
                let options = ev
                    .get("options")
                    .and_then(Value::as_array)
                    .filter(|options| !options.is_empty())
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    });
                let context = ev
                    .get("context")
                    .filter(|v| !v.is_null())
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let mut event = FleetEvent::new(
                    FleetEventKind::Question,
                    run_id,
                    state.name.clone(),
                    vec![
                        ("question-id".to_string(), Some(question_id)),
                        ("question".to_string(), Some(question)),
                        ("options".to_string(), options),
                        ("context".to_string(), context),
                    ],
                );
                event.set(
                    "next",
                    crate::fleet::event::describe_next_step(FleetEventKind::Question, &state.name),
                );
                self.push(event);
            }
            "worker_dialog" => {
                // A pi extension dialog blocks the worker exactly like a
                // `fleet_ask`; surface it as a question so the orchestrator
                // reconciles instead of wondering why the worker stalled.
                let question_id = string_field(ev, "questionId");
                let question = string_field(ev, "question");
                let options = ev
                    .get("options")
                    .and_then(Value::as_array)
                    .filter(|options| !options.is_empty())
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    });
                let mut event = FleetEvent::new(
                    FleetEventKind::Question,
                    run_id,
                    state.name.clone(),
                    vec![
                        ("question-id".to_string(), Some(question_id)),
                        (
                            "question".to_string(),
                            Some(format!(
                                "{} ({})",
                                string_field(ev, "question"),
                                string_field(ev, "method")
                            )),
                        ),
                        ("options".to_string(), options),
                        ("context".to_string(), None),
                    ],
                );
                event.set(
                    "next",
                    crate::fleet::event::describe_next_step(FleetEventKind::Question, &state.name),
                );
                let _ = &question;
                self.push(event);
            }
            "answer_delivered" => {
                // Only the human's answers are news; the orchestrator knows its own.
                if string_field(ev, "source") != "console" {
                    return;
                }
                self.push(FleetEvent::new(
                    FleetEventKind::AnsweredByConsole,
                    run_id,
                    state.name.clone(),
                    vec![
                        (
                            "question-id".to_string(),
                            Some(string_field(ev, "questionId")),
                        ),
                        ("answer".to_string(), Some(string_field(ev, "message"))),
                    ],
                ));
            }
            "steering_delivered" => {
                if string_field(ev, "source") != "console" {
                    return;
                }
                self.push(FleetEvent::new(
                    FleetEventKind::ConsoleSteer,
                    run_id,
                    state.name.clone(),
                    vec![("message".to_string(), Some(string_field(ev, "message")))],
                ));
            }
            "worker_question_resolved" => {
                if string_field(ev, "how") != "timeout" {
                    return;
                }
                self.push(FleetEvent::new(
                    FleetEventKind::QuestionResolved,
                    run_id,
                    state.name.clone(),
                    vec![
                        ("question-id".to_string(), Some(string_field(ev, "questionId"))),
                        (
                            "how".to_string(),
                            Some(
                                "timeout — nobody answered; the worker proceeded on its own judgment"
                                    .to_string(),
                            ),
                        ),
                    ],
                ));
            }
            "worker_progress" => {
                if !self.progress_events {
                    return;
                }
                let now = now_ms();
                let last = self.last_progress_at.get(run_id).copied().unwrap_or(0);
                let throttle = i64::try_from(self.progress_throttle_ms).unwrap_or(i64::MAX);
                if now - last < throttle {
                    return;
                }
                self.last_progress_at.insert(run_id.to_string(), now);
                self.push(FleetEvent::new(
                    FleetEventKind::Progress,
                    run_id,
                    state.name.clone(),
                    vec![("message".to_string(), Some(string_field(ev, "message")))],
                ));
            }
            _ => {}
        }
    }
}

fn string_field(ev: &Value, key: &str) -> String {
    ev.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A fixture fleet dir whose runs are plain files (no monitor involved).
    fn mk_fleet(name: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir_in(std::env::temp_dir())
            .unwrap_or_else(|_| tempfile::tempdir().unwrap());
        let fleet_dir = tmp.path().join(format!(".pilotfish-{name}"));
        std::fs::create_dir_all(fleet_dir.join("runs")).unwrap();
        (tmp, fleet_dir)
    }

    fn add_run(fleet_dir: &std::path::Path, name: &str, seq: usize) -> (String, PathBuf, RunState) {
        let run_id = format!("{name}-20260829000000{seq:02}");
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
        state.status = run::RunStatus::Running;
        state.pid = Some(i32::try_from(std::process::id()).unwrap_or(1));
        run::save_state(&run_dir, &state).unwrap();
        std::fs::write(run_dir.join("events.jsonl"), "").unwrap();
        (run_id, run_dir, state)
    }

    fn add_running_run(fleet_dir: &std::path::Path, name: &str) -> (String, PathBuf, RunState) {
        let (run_id, run_dir, state) = add_run(fleet_dir, name, 0);
        (run_id, run_dir, state)
    }

    fn append_event(run_dir: &std::path::Path, event: &Value) {
        crate::util::append_json_line(&run_dir.join("events.jsonl"), event).unwrap();
    }

    fn watcher(fleet_dir: &std::path::Path) -> FleetWatcher {
        FleetWatcher::new(FleetWatcherOptions {
            fleet_dir: fleet_dir.to_path_buf(),
            ..FleetWatcherOptions::default()
        })
    }

    fn kinds(events: &[FleetEvent]) -> Vec<FleetEventKind> {
        events.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn terminal_reported_once() {
        let (tmp, fleet_dir) = mk_fleet("terminal");
        let (run_id, run_dir, _state) = add_running_run(&fleet_dir, "add-auth");
        let mut watcher = FleetWatcher::new(FleetWatcherOptions {
            fleet_dir: fleet_dir.clone(),
            ..FleetWatcherOptions::default()
        });
        watcher.start(false);
        watcher.tick();
        assert!(watcher.take_batch().is_empty(), "a running run is not news");

        let mut state = run::load_state(&run_dir).unwrap();
        state.status = run::RunStatus::Settled;
        state.last_assistant_text = Some("Working: wrote hello.txt\nsecond line".into());
        run::save_state(&run_dir, &state).unwrap();
        let report = crate::fleet::report::report_path(&fleet_dir, &run_id);
        std::fs::create_dir_all(report.parent().unwrap()).unwrap();
        std::fs::write(&report, "# report").unwrap();
        watcher.tick();
        watcher.tick();
        watcher.tick();
        let events = watcher.take_batch();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, FleetEventKind::Settled);
        assert_eq!(events[0].name, "add-auth");
        assert_eq!(events[0].run_id, run_id);
        let text = crate::fleet::event::format_fleet_event(&events[0]);
        assert!(text.starts_with(&format!(
            "<fleet-event kind=\"settled\" run=\"{run_id}\" name=\"add-auth\""
        )));
        assert!(text.contains("status: settled"), "{text}");
        assert!(
            text.contains("report: ") && text.contains("(present)"),
            "{text}"
        );
        assert!(
            text.contains("branch: pilotfish/add-auth-1234567"),
            "{text}"
        );
        assert!(text.contains("last: Working: wrote hello.txt"), "{text}");
        assert!(
            text.contains("next: fleet_report name=\"add-auth\""),
            "{text}"
        );
        assert!(text.ends_with("</fleet-event>"), "{text}");

        // already reported: no duplicates
        watcher.tick();
        assert!(watcher.take_batch().is_empty());
        drop(tmp);
    }

    #[test]
    fn question_events() {
        {
            let (tmp, fleet_dir) = mk_fleet("provenance");
            let (_run_id, run_dir, _state) = add_running_run(&fleet_dir, "db");
            let mut w = watcher(&fleet_dir);
            w.start(false);
            append_event(
                &run_dir,
                &json!({"type":"worker_question","questionId":"q_1","question":"bcrypt or argon2?","options":["bcrypt","argon2"],"context":"brief says secure only"}),
            );
            append_event(
                &run_dir,
                &json!({"type":"answer_delivered","questionId":"q_1","source":"orchestrator","message":"argon2"}),
            );
            append_event(
                &run_dir,
                &json!({"type":"steering_delivered","source":"orchestrator","message":"use tabs"}),
            );
            append_event(
                &run_dir,
                &json!({"type":"worker_progress","message":"tests pass"}),
            );
            append_event(
                &run_dir,
                &json!({"type":"worker_question_resolved","questionId":"q_1","how":"answered"}),
            );
            append_event(
                &run_dir,
                &json!({"type":"tool_execution_end","toolName":"bash"}),
            );
            w.tick();
            assert_eq!(kinds(&w.take_batch()), vec![FleetEventKind::Question]);

            // the human's interventions are the news
            append_event(
                &run_dir,
                &json!({"type":"answer_delivered","questionId":"q_2","source":"console","message":"argon2"}),
            );
            append_event(
                &run_dir,
                &json!({"type":"steering_delivered","source":"console","message":"use spaces"}),
            );
            append_event(
                &run_dir,
                &json!({"type":"worker_question_resolved","questionId":"q_3","how":"timeout"}),
            );
            w.tick();
            let events = w.take_batch();
            assert_eq!(
                kinds(&events),
                vec![
                    FleetEventKind::AnsweredByConsole,
                    FleetEventKind::ConsoleSteer,
                    FleetEventKind::QuestionResolved,
                ]
            );
            let answered = crate::fleet::event::format_fleet_event(&events[0]);
            assert!(answered.contains("answer: argon2"), "{answered}");
            assert!(answered.contains("question-id: q_2"), "{answered}");
            let steered = crate::fleet::event::format_fleet_event(&events[1]);
            assert!(
                steered.contains("message: use spaces") || steered.contains("message: use tabs"),
                "{steered}"
            );
            let resolved = crate::fleet::event::format_fleet_event(&events[2]);
            assert!(resolved.contains("how: timeout"), "{resolved}");

            // already-consumed lines are not replayed
            w.tick();
            assert!(w.take_batch().is_empty());
            drop(tmp);
        }
        {
            let (tmp, fleet_dir) = mk_fleet("dialogs");
            let (_run_id, run_dir, mut state) = add_running_run(&fleet_dir, "dialog-run");
            state.pending_dialog = Some(crate::fleet::run::PendingDialog {
                id: "u-1".into(),
                method: "select".into(),
                question: "Pick one".into(),
                options: Some(vec!["a".into(), "b".into()]),
                context: None,
                asked_at: "t".into(),
            });
            run::save_state(&run_dir, &state).unwrap();
            // start first: a dialog appended afterwards is news, not history
            let mut w = watcher(&fleet_dir);
            w.start(true);
            append_event(
                &run_dir,
                &json!({"type":"worker_dialog","questionId":"u-1","method":"select","question":"Pick one","options":["a","b"]}),
            );
            w.tick();
            let events = w.take_batch();
            // the snapshot mentions the dialog like a question
            let snapshot = events
                .iter()
                .find(|e| e.kind == FleetEventKind::Snapshot)
                .expect("a snapshot was queued");
            let snap_text = crate::fleet::event::format_fleet_event(snapshot);
            assert!(snap_text.contains("asking: Pick one"), "{snap_text}");
            // and the dialog itself surfaces as a question
            let question = events
                .iter()
                .find(|e| e.kind == FleetEventKind::Question)
                .expect("the dialog became a question event");
            let text = crate::fleet::event::format_fleet_event(question);
            assert!(text.contains("question-id: u-1"), "{text}");
            assert!(text.contains("(select)"), "{text}");
            drop(tmp);
        }
    }

    #[test]
    fn cursors_resume() {
        let (tmp, fleet_dir) = mk_fleet("cursors");
        let (run_id, run_dir, mut state) = add_running_run(&fleet_dir, "old");
        state.pending_question = Some(crate::fleet::run::PendingQuestion {
            id: "q_9".into(),
            question: "which db?".into(),
            options: None,
            context: None,
            asked_at: "t".into(),
        });
        run::save_state(&run_dir, &state).unwrap();
        append_event(
            &run_dir,
            &json!({"type":"worker_question","questionId":"q_9","question":"which db?"}),
        );

        let mut first = watcher(&fleet_dir);
        first.start(true);
        first.tick();
        let events = first.take_batch();
        assert_eq!(kinds(&events), vec![FleetEventKind::Snapshot]);
        let snap = crate::fleet::event::format_fleet_event(&events[0]);
        assert!(snap.contains("run=\"-\" name=\"fleet\""), "{snap}");
        assert!(
            snap.contains("runs: old (blocked, asking: which db?)"),
            "{snap}"
        );
        assert!(snap.contains("count: 1"), "{snap}");
        let cursors = first.cursors();
        assert!(cursors[&run_id].events_offset > 0);
        assert_eq!(cursors[&run_id].last_view.as_deref(), Some("blocked"));

        append_event(
            &run_dir,
            &json!({"type":"worker_question","questionId":"q_10","question":"which cache?"}),
        );
        let mut second = FleetWatcher::new(FleetWatcherOptions {
            fleet_dir: fleet_dir.clone(),
            cursors: first.cursors(),
            ..FleetWatcherOptions::default()
        });
        second.start(false);
        second.tick();
        let resumed = second.take_batch();
        assert_eq!(
            resolved_question_ids(&resumed),
            vec!["q_10"],
            "continues from the saved cursor"
        );

        // a third watcher with the second's cursors sees only the snapshot
        let mut third = FleetWatcher::new(FleetWatcherOptions {
            fleet_dir,
            cursors: second.cursors(),
            ..FleetWatcherOptions::default()
        });
        third.start(true);
        third.tick();
        assert_eq!(kinds(&third.take_batch()), vec![FleetEventKind::Snapshot]);
        drop(tmp);
    }

    fn resolved_question_ids(events: &[FleetEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| {
                e.fields
                    .iter()
                    .find(|(k, _)| k == "question-id")
                    .and_then(|(_, v)| v.clone())
            })
            .collect()
    }

    #[test]
    fn progress_throttled() {
        let (tmp, fleet_dir) = mk_fleet("progress");
        let (_run_id, run_dir, _state) = add_running_run(&fleet_dir, "slow");
        let mut off = watcher(&fleet_dir);
        off.start(false);
        append_event(&run_dir, &json!({"type":"worker_progress","message":"one"}));
        off.tick();
        assert!(off.take_batch().is_empty());
        let cursors = off.cursors();

        let mut on = FleetWatcher::new(FleetWatcherOptions {
            fleet_dir,
            cursors,
            progress_events: true,
            progress_throttle_ms: 10_000,
            ..FleetWatcherOptions::default()
        });
        on.start(false);
        append_event(&run_dir, &json!({"type":"worker_progress","message":"two"}));
        append_event(
            &run_dir,
            &json!({"type":"worker_progress","message":"three"}),
        );
        on.tick();
        let events = on.take_batch();
        let messages: Vec<String> = events
            .iter()
            .filter_map(|e| {
                e.fields
                    .iter()
                    .find(|(k, _)| k == "message")
                    .and_then(|(_, v)| v.clone())
            })
            .collect();
        assert_eq!(
            messages,
            vec!["two".to_string()],
            "throttled to one per window"
        );
        drop(tmp);
    }

    /// The core multi-session guarantee: a watcher pinned to one session
    /// reports only the runs that session owns, so a worker settling in
    /// session B can never produce a `<fleet-event>` in session A's
    /// transcript.
    #[test]
    fn reports_owned_runs() {
        let (tmp, fleet_dir) = mk_fleet("owner");
        // Two live sessions sharing one store.
        let a = crate::orch::session::create_session(&fleet_dir, Some("session-a")).unwrap();
        let b = crate::orch::session::create_session(&fleet_dir, Some("session-b")).unwrap();

        let (b_run, b_run_dir, _) = add_run(&fleet_dir, "db", 0);
        let (a_run, a_run_dir, _) = add_run(&fleet_dir, "auth", 1);
        for (run_dir, owner) in [(&b_run_dir, b.uuid), (&a_run_dir, a.uuid)] {
            let mut state = run::load_state(run_dir).unwrap();
            state.orchestrator_id = Some(owner);
            state.status = run::RunStatus::Running;
            run::save_state(run_dir, &state).unwrap();
        }

        let mut watcher_a = FleetWatcher::new(FleetWatcherOptions {
            fleet_dir: fleet_dir.clone(),
            owner: Some(a.uuid),
            ..FleetWatcherOptions::default()
        });
        let mut watcher_b = FleetWatcher::new(FleetWatcherOptions {
            fleet_dir: fleet_dir.clone(),
            owner: Some(b.uuid),
            ..FleetWatcherOptions::default()
        });
        watcher_a.start(false);
        watcher_b.start(false);
        watcher_a.tick();
        watcher_b.tick();
        assert!(watcher_a.take_batch().is_empty());
        assert!(watcher_b.take_batch().is_empty());

        // B's run settles while both watchers watch: only B's watcher sees it.
        {
            let mut state = run::load_state(&b_run_dir).unwrap();
            state.status = run::RunStatus::Settled;
            run::save_state(&b_run_dir, &state).unwrap();
        }
        watcher_a.tick();
        watcher_a.tick();
        assert!(
            watcher_a.take_batch().is_empty(),
            "a run of session B must never produce a fleet event in session A's watcher"
        );
        watcher_b.tick();
        let events = watcher_b.take_batch();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, FleetEventKind::Settled);
        assert_eq!(events[0].run_id, b_run);

        // A's own run settling reaches A (and, already reported, not B).
        {
            let mut state = run::load_state(&a_run_dir).unwrap();
            state.status = run::RunStatus::Settled;
            run::save_state(&a_run_dir, &state).unwrap();
        }
        watcher_a.tick();
        let events = watcher_a.take_batch();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, FleetEventKind::Settled);
        assert_eq!(events[0].run_id, a_run);
        watcher_b.tick();
        assert!(watcher_b.take_batch().is_empty());
        drop(tmp);
    }
}
