//! The view model: everything the console shows, derived on demand from
//! fleet state and never stored. The app layer builds these rows and the
//! renderer draws them; nothing here touches the terminal.
//!
//! Ported from the TypeScript `src/tui/model.ts` (rail half), reshaped for
//! the dashboard-and-drill-down console: the old rail's rows *are* the
//! dashboard's rows, the orchestrator first, then every live worker.

use uuid::Uuid;

use crate::fleet::run::{DerivedView, RunState, derive_view};
use crate::orch::records::{Activity, ActivityKind};
use crate::util::{first_line, format_age, parse_ts_ms};

/// Which session a row (and therefore a command) is aimed at. The
/// orchestrator row carries the session it serves — the `Party::Orchestrator`
/// identity, and the key every console action (`Console.orch_key`) derives
/// from. The nil uuid is the legacy default session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTarget {
    Orchestrator(Uuid),
    Worker { run_id: String },
}

impl SessionTarget {
    /// The key this target answers to in `Console` maps: `orchestrator` or
    /// the run id.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Orchestrator(_) => "orchestrator",
            Self::Worker { run_id } => run_id,
        }
    }

    /// True when the target is a worker run.
    #[must_use]
    pub const fn is_worker(&self) -> bool {
        matches!(self, Self::Worker { .. })
    }
}

/// What to call a session in the console: its alias, or — until the
/// orchestrator derives one from its first prompt — its short uuid. The nil
/// uuid is the legacy default session, and keeps the old `orchestrator`
/// spelling.
#[must_use]
pub fn session_display_name(alias: Option<&str>, uuid: Uuid) -> String {
    alias.map(str::to_string).unwrap_or_else(|| {
        if uuid.is_nil() {
            "orchestrator".to_string()
        } else {
            crate::util::short_uuid(&uuid)
        }
    })
}

/// What the console calls the session it is serving: always `orchestrator`,
/// with the session's alias appended when it has one. The rail row and the
/// composer prompt both use this, so the row that owns the conversation is
/// never a bare hex id — [`session_display_name`] stays the identifier for
/// naming *other* sessions, where the uuid still has to disambiguate.
#[must_use]
pub fn session_label(alias: Option<&str>) -> String {
    match alias.map(str::trim).filter(|a| !a.is_empty()) {
        Some(alias) => format!("orchestrator · {alias}"),
        None => "orchestrator".to_string(),
    }
}

/// One dashboard row: a session at a glance — state glyph, name, what it is
/// doing right now, age, and for a worker its branch and diff stat if known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardRow {
    /// `orchestrator` or the run id.
    pub key: String,
    pub glyph: &'static str,
    pub name: String,
    /// What it is doing right now; the glyph already carries the state, so
    /// this is the operation, not the state.
    pub detail: String,
    /// How long the session has been alive (orchestrator: empty).
    pub age: String,
    pub target: SessionTarget,
    /// Needs the human: a pending question or dialog, an approval, or an
    /// exited orchestrator.
    pub attention: bool,
    /// The worker's branch, when it has one.
    pub branch: Option<String>,
    /// Diff stat against the base, when the console has one.
    pub diff_stat: Option<String>,
}

/// The orchestrator's summary, as the console sees it (`OrchestratorState`
/// plus the approval count the overlay holds, plus the served session's
/// identity for the row's target and name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchSummary {
    pub turn_active: bool,
    pub exited: bool,
    pub pending_approvals: usize,
    /// The session this row stands for; nil on a legacy/default console.
    pub session_uuid: Uuid,
    /// The row's name — alias, short uuid, or empty for the legacy
    /// `orchestrator` spelling.
    pub session_name: String,
}

impl Default for OrchSummary {
    fn default() -> Self {
        Self {
            turn_active: false,
            exited: false,
            pending_approvals: 0,
            session_uuid: crate::util::nil_uuid(),
            session_name: String::new(),
        }
    }
}

/// One worker row's inputs: its state and, when known, its diff stat.
#[derive(Debug, Clone, Copy)]
pub struct RunRow<'a> {
    pub run_id: &'a str,
    pub state: &'a RunState,
    pub diff_stat: Option<&'a str>,
}

/// What the orchestrator is doing when it emits a thinking block: a factory
/// for a fresh [`Activity`], stamped with `since_ms`.
#[must_use]
pub const fn activity(kind: ActivityKind, label: Option<String>, since_ms: i64) -> Activity {
    Activity {
        kind,
        label,
        since: since_ms,
    }
}

/// The state glyphs. `○` idle, `●` working, `?` needs you, `✓` done,
/// `■` stopped, `!` failed or dead, `…` starting, `·` archived.
#[must_use]
pub const fn worker_glyph(view: DerivedView) -> &'static str {
    match view {
        DerivedView::Starting => "…",
        DerivedView::Running => "●",
        DerivedView::Blocked => "?",
        DerivedView::Settled => "✓",
        DerivedView::Stopped => "■",
        DerivedView::Error | DerivedView::Dead => "!",
        DerivedView::Archived => "·",
    }
}

/// What a worker is doing, for the line under its name. The glyph already
/// says running/blocked/settled, so this is the operation, not the state.
#[must_use]
pub fn worker_detail(state: &RunState, view: DerivedView) -> String {
    match view {
        DerivedView::Blocked => "needs an answer".to_string(),
        DerivedView::Running => match state.activity {
            Some(crate::fleet::run::WorkerActivity::Thinking) => "✻ thinking…".to_string(),
            Some(crate::fleet::run::WorkerActivity::Text) => "✎ replying…".to_string(),
            Some(crate::fleet::run::WorkerActivity::Tool) | None => state
                .last_tool
                .as_deref()
                .map_or_else(|| "working…".to_string(), |tool| format!("⚙ {tool}")),
        },
        DerivedView::Starting => "starting…".to_string(),
        DerivedView::Settled => "done".to_string(),
        DerivedView::Error => state.error.as_deref().map_or_else(
            || "failed".to_string(),
            |error| first_line(error).to_string(),
        ),
        DerivedView::Dead => "monitor gone".to_string(),
        DerivedView::Stopped => "stopped".to_string(),
        DerivedView::Archived => "archived".to_string(),
    }
}

/// The orchestrator's dashboard row. The name is the served session's
/// alias, its short uuid, or — for the legacy default session — the old
/// `orchestrator` spelling; the target carries the session uuid.
#[must_use]
pub fn orchestrator_row(orch: &OrchSummary) -> DashboardRow {
    let glyph = if orch.exited {
        "!"
    } else if orch.pending_approvals > 0 {
        "?"
    } else if orch.turn_active {
        "●"
    } else {
        "○"
    };
    let detail = if orch.exited {
        "exited".to_string()
    } else if orch.pending_approvals > 0 {
        format!(
            "{} to approve",
            count_noun(orch.pending_approvals, "approval")
        )
    } else if orch.turn_active {
        "working…".to_string()
    } else {
        "idle".to_string()
    };
    let name = if orch.session_name.is_empty() {
        "orchestrator".to_string()
    } else {
        orch.session_name.clone()
    };
    DashboardRow {
        key: "orchestrator".to_string(),
        glyph,
        name,
        detail,
        age: String::new(),
        target: SessionTarget::Orchestrator(orch.session_uuid),
        attention: orch.pending_approvals > 0 || orch.exited,
        branch: None,
        diff_stat: None,
    }
}

/// `1 approval` / `3 approvals`.
#[must_use]
pub fn count_noun(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// One worker's dashboard row, or `None` when the run is archived (cleaned
/// up; the dashboard shows live sessions, not the graveyard).
#[must_use]
pub fn worker_row(row: &RunRow<'_>, now_ms: i64) -> Option<DashboardRow> {
    let view = derive_view(row.state, crate::fleet::run::is_alive, now_ms);
    if view == DerivedView::Archived {
        return None;
    }
    let created = parse_ts_ms(&row.state.created_at).unwrap_or(now_ms);
    Some(DashboardRow {
        key: row.run_id.to_string(),
        glyph: worker_glyph(view),
        name: row.state.name.clone(),
        detail: worker_detail(row.state, view),
        age: format_age((now_ms - created).max(0)),
        target: SessionTarget::Worker {
            run_id: row.run_id.to_string(),
        },
        attention: view == DerivedView::Blocked,
        branch: row.state.branch.clone(),
        diff_stat: row.diff_stat.map(str::to_string),
    })
}

/// The dashboard: the orchestrator first, then every live worker, in the
/// order given. Archived runs are skipped; rows are keyed for selection.
#[must_use]
pub fn build_rows(orch: &OrchSummary, runs: &[RunRow<'_>], now_ms: i64) -> Vec<DashboardRow> {
    let mut rows = Vec::with_capacity(runs.len() + 1);
    rows.push(orchestrator_row(orch));
    for run in runs {
        if let Some(row) = worker_row(run, now_ms) {
            rows.push(row);
        }
    }
    rows
}

/// `✻ thinking… 8s` — what the orchestrator is doing, and for how long.
#[must_use]
pub fn activity_line(activity: Option<&Activity>, now_ms: i64) -> Option<String> {
    let activity = activity?;
    let seconds = ((now_ms - activity.since) / 1000).max(0);
    let elapsed = if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    };
    match activity.kind {
        ActivityKind::Thinking => Some(format!("✻ thinking… {elapsed}")),
        ActivityKind::Responding => Some(format!("✎ replying… {elapsed}")),
        ActivityKind::Tool => Some(format!(
            "⚙ {}… {elapsed}",
            activity.label.as_deref().unwrap_or("tool")
        )),
    }
}

/// `✻ thinking… 12s` for a worker: its activity, aged from its last movement.
#[must_use]
pub fn worker_activity_line(state: &RunState, view: DerivedView, now_ms: i64) -> Option<String> {
    if view != DerivedView::Running {
        return None;
    }
    let base = worker_detail(state, view);
    let since =
        parse_ts_ms(state.last_activity.as_deref().unwrap_or(&state.created_at)).unwrap_or(now_ms);
    let age = format_age((now_ms - since).max(0));
    Some(format!("{base} {age}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::{RunState, WorkerActivity};

    fn state(name: &str, run_id: &str) -> RunState {
        RunState::new(
            "/f", run_id, name, "/repo", "brief", None, None, None, None, None, None, None, None,
            None, None, None,
        )
    }

    // 2026-09-30T12:00:00Z, so hand-written test timestamps land near it.
    const NOW: i64 = 1_790_769_600_000;
    /// A pid that is alive no matter what (this process), for `Running` rows.
    fn alive_pid() -> i32 {
        std::process::id() as i32
    }
    /// A pid that cannot exist, for dead rows.
    const DEAD_PID: i32 = i32::MAX;

    #[test]
    fn orchestrator_row_reflects_activity_approvals_and_exit() {
        let row = orchestrator_row(&OrchSummary::default());
        assert_eq!(row.glyph, "○");
        assert_eq!(row.detail, "idle");
        assert!(!row.attention);

        let row = orchestrator_row(&OrchSummary {
            turn_active: true,
            ..OrchSummary::default()
        });
        assert_eq!(row.glyph, "●");
        assert_eq!(row.detail, "working…");

        let row = orchestrator_row(&OrchSummary {
            pending_approvals: 1,
            ..OrchSummary::default()
        });
        assert_eq!(row.glyph, "?");
        assert_eq!(row.detail, "1 approval to approve");
        assert!(row.attention);

        let row = orchestrator_row(&OrchSummary {
            pending_approvals: 3,
            ..OrchSummary::default()
        });
        assert_eq!(row.detail, "3 approvals to approve");

        let row = orchestrator_row(&OrchSummary {
            exited: true,
            ..OrchSummary::default()
        });
        assert_eq!(row.glyph, "!");
        assert_eq!(row.detail, "exited");
        assert!(row.attention);
    }

    #[test]
    fn the_orchestrator_row_uses_the_session_name_and_uuid() {
        let uuid = uuid::Uuid::new_v4();
        let row = orchestrator_row(&OrchSummary {
            session_uuid: uuid,
            session_name: "add-auth".into(),
            ..OrchSummary::default()
        });
        assert_eq!(row.name, "add-auth");
        assert_eq!(row.target, SessionTarget::Orchestrator(uuid));
        assert_eq!(row.target.key(), "orchestrator");
        // the legacy default session keeps the old spelling
        let row = orchestrator_row(&OrchSummary::default());
        assert_eq!(row.name, "orchestrator");
        assert_eq!(
            row.target,
            SessionTarget::Orchestrator(crate::util::nil_uuid())
        );
    }

    #[test]
    fn the_session_label_always_says_orchestrator_and_carries_the_alias() {
        assert_eq!(session_label(None), "orchestrator");
        assert_eq!(session_label(Some("db")), "orchestrator · db");
        assert_eq!(
            session_label(Some("   ")),
            "orchestrator",
            "a blank alias is no alias"
        );
    }

    #[test]
    fn session_display_name_uses_alias_then_short_uuid_then_legacy_spelling() {
        let uuid = uuid::Uuid::new_v4();
        assert_eq!(session_display_name(Some("db"), uuid), "db");
        assert_eq!(
            session_display_name(None, uuid),
            crate::util::short_uuid(&uuid),
            "an alias-less session is shown by its short uuid until one appears"
        );
        assert_eq!(
            session_display_name(None, crate::util::nil_uuid()),
            "orchestrator"
        );
    }

    #[test]
    fn worker_rows_follow_the_orchestrator_and_skip_archived() {
        let mut running = state("add-auth", "add-auth-20260829120000");
        running.status = crate::fleet::run::RunStatus::Running;
        running.pid = Some(alive_pid());
        running.branch = Some("pilotfish/add-auth-9120000".into());
        let mut settled = state("add-tests", "add-tests-20260829120001");
        settled.status = crate::fleet::run::RunStatus::Settled;
        let mut gone = state("merged", "merged-20260829120002");
        gone.status = crate::fleet::run::RunStatus::Archived;
        let runs = [
            RunRow {
                run_id: "add-auth-20260829120000",
                state: &running,
                diff_stat: Some("+12 −3"),
            },
            RunRow {
                run_id: "add-tests-20260829120001",
                state: &settled,
                diff_stat: None,
            },
            RunRow {
                run_id: "merged-20260829120002",
                state: &gone,
                diff_stat: None,
            },
        ];
        let rows = build_rows(&OrchSummary::default(), &runs, NOW);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[0].name, "orchestrator");
        assert_eq!(rows[1].name, "add-auth");
        assert_eq!(rows[2].name, "add-tests");
        assert_eq!(rows[1].glyph, "●");
        assert_eq!(
            rows[1].branch.as_deref(),
            Some("pilotfish/add-auth-9120000")
        );
        assert_eq!(rows[1].diff_stat.as_deref(), Some("+12 −3"));
        assert_eq!(rows[2].glyph, "✓");
        assert_eq!(rows[2].diff_stat, None);
        assert!(rows[1].target.is_worker());
        assert_eq!(rows[1].target.key(), "add-auth-20260829120000");
    }

    #[test]
    fn worker_details_describe_the_operation_not_the_state() {
        let mut s = state("db", "db-20260829120000");
        s.status = crate::fleet::run::RunStatus::Running;
        s.pid = Some(alive_pid());
        let view = derive_view(&s, |_| true, NOW);
        assert_eq!(worker_detail(&s, view), "working…");

        s.activity = Some(WorkerActivity::Thinking);
        assert_eq!(worker_detail(&s, view), "✻ thinking…");
        s.activity = Some(WorkerActivity::Text);
        assert_eq!(worker_detail(&s, view), "✎ replying…");
        s.activity = Some(WorkerActivity::Tool);
        assert_eq!(worker_detail(&s, view), "working…", "no tool known yet");
        s.last_tool = Some("bash".into());
        assert_eq!(worker_detail(&s, view), "⚙ bash");

        s.pending_question = Some(crate::fleet::run::PendingQuestion {
            id: "q_1".into(),
            question: "which fixture?".into(),
            options: None,
            context: None,
            asked_at: crate::util::now_iso(),
        });
        let blocked = derive_view(&s, |_| true, NOW);
        assert_eq!(blocked, DerivedView::Blocked);
        assert_eq!(worker_detail(&s, blocked), "needs an answer");

        s.status = crate::fleet::run::RunStatus::Starting;
        s.pid = None;
        s.pending_question = None;
        assert_eq!(worker_detail(&s, DerivedView::Starting), "starting…");

        s.status = crate::fleet::run::RunStatus::Settled;
        assert_eq!(worker_detail(&s, DerivedView::Settled), "done");

        s.status = crate::fleet::run::RunStatus::Error;
        s.error = Some("boom\nsecond line".into());
        assert_eq!(
            worker_detail(&s, DerivedView::Error),
            "boom",
            "first line only"
        );

        s.error = None;
        assert_eq!(worker_detail(&s, DerivedView::Error), "failed");
        assert_eq!(worker_detail(&s, DerivedView::Dead), "monitor gone");
        assert_eq!(worker_detail(&s, DerivedView::Stopped), "stopped");
    }

    #[test]
    fn blocked_workers_and_pending_dialogs_flag_attention() {
        let mut s = state("db", "db-20260829120000");
        s.status = crate::fleet::run::RunStatus::Running;
        s.pid = Some(alive_pid());
        s.created_at = "2026-09-30T11:59:00.000Z".into();
        s.pending_dialog = Some(crate::fleet::run::PendingDialog {
            id: "u-1".into(),
            method: "select".into(),
            question: "Pick one".into(),
            options: Some(vec!["a".into()]),
            context: None,
            asked_at: crate::util::now_iso(),
        });
        let row = worker_row(
            &RunRow {
                run_id: "db-20260829120000",
                state: &s,
                diff_stat: None,
            },
            NOW,
        )
        .unwrap();
        assert_eq!(row.glyph, "?");
        assert_eq!(row.detail, "needs an answer");
        assert!(row.attention);
        assert!(!row.age.is_empty());
    }

    #[test]
    fn dead_workers_show_the_monitor_gone_glyph() {
        let mut s = state("db", "db-20260829120000");
        s.status = crate::fleet::run::RunStatus::Running;
        s.pid = Some(DEAD_PID);
        let row = worker_row(
            &RunRow {
                run_id: "db-20260829120000",
                state: &s,
                diff_stat: None,
            },
            NOW,
        )
        .unwrap();
        assert_eq!(row.glyph, "!", "the pid is not alive");
        assert_eq!(row.detail, "monitor gone");
    }

    #[test]
    fn activity_lines_carry_the_elapsed_time() {
        let since = NOW - 8_000;
        let thinking = activity(ActivityKind::Thinking, None, since);
        assert_eq!(
            activity_line(Some(&thinking), NOW).as_deref(),
            Some("✻ thinking… 8s")
        );
        let responding = activity(ActivityKind::Responding, None, since);
        assert_eq!(
            activity_line(Some(&responding), NOW).as_deref(),
            Some("✎ replying… 8s")
        );
        let tool = activity(ActivityKind::Tool, Some("Bash".into()), since);
        assert_eq!(
            activity_line(Some(&tool), NOW).as_deref(),
            Some("⚙ Bash… 8s")
        );
        // minutes roll over
        let tool = activity(ActivityKind::Tool, Some("Bash".into()), NOW - 95_000);
        assert_eq!(
            activity_line(Some(&tool), NOW).as_deref(),
            Some("⚙ Bash… 1m35s")
        );
        assert_eq!(activity_line(None, NOW), None);
    }

    #[test]
    fn worker_activity_line_ages_from_the_last_movement() {
        let mut s = state("db", "db-20260829120000");
        s.status = crate::fleet::run::RunStatus::Running;
        s.pid = Some(alive_pid());
        s.created_at = "2026-09-30T11:59:50.000Z".into();
        s.last_activity = Some("2026-09-30T11:59:52.000Z".into());
        // NOW - since = 8s
        let line = worker_activity_line(&s, DerivedView::Running, NOW).unwrap();
        assert_eq!(line, "working… 8s");
        // Not running: no line at all.
        assert_eq!(worker_activity_line(&s, DerivedView::Blocked, NOW), None);
    }
}
