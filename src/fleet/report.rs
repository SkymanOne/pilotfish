//! Reading a worker's final report: the report file wins, then the captured
//! last assistant text, then nothing. Ported from the TypeScript
//! `src/report.ts` and relocated into the run's own directory
//! (`runs/<id>/report.md`).

use std::path::Path;

use crate::fleet::run::RunState;
use crate::paths::FleetPaths;

/// `runs/<runId>/report.md` under the fleet dir.
#[must_use]
pub fn report_path(fleet_dir: &Path, run_id: &str) -> std::path::PathBuf {
    FleetPaths::new(fleet_dir).run_report(run_id)
}

/// Orchestrator-side steering log, appended after the worker's own report.
#[must_use]
pub fn build_steering_appendix(state: &RunState) -> String {
    if state.steer_count == 0 || state.steering_log.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = state
        .steering_log
        .iter()
        .map(|s| format!("- [{}] {} {}", s.source, s.ts, s.message))
        .collect();
    format!(
        "\n---\n## Steering log (orchestrator-side, most recent last)\n{}\n",
        lines.join("\n")
    )
}

/// What [`read_report`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportResult {
    /// The worker wrote `report.md`.
    Report(String),
    /// No report file; fall back to the captured last assistant text.
    Fallback(String),
    /// Nothing to show.
    Missing,
}

impl ReportResult {
    /// The text to print, if any.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Report(text) | Self::Fallback(text) => Some(text),
            Self::Missing => None,
        }
    }
}

/// The report file wins; else the captured last assistant text; else nothing.
#[must_use]
pub fn read_report(fleet_dir: &Path, state: &RunState) -> ReportResult {
    let path = report_path(fleet_dir, &state.id);
    if let Ok(text) = std::fs::read_to_string(&path) {
        return ReportResult::Report(text);
    }
    if let Some(last) = &state.last_assistant_text {
        return ReportResult::Fallback(format!(
            "[No report file — falling back to last assistant text]\n\n{last}"
        ));
    }
    ReportResult::Missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::{RunState, SteeringEntry};

    fn state_with_steering(entries: Vec<SteeringEntry>) -> RunState {
        let mut state = RunState::new(
            "/tmp/x/.pilotfish",
            "auth-20260828141530",
            "auth",
            "/tmp/x",
            "b",
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
        state.steer_count = u32::try_from(entries.len()).unwrap();
        state.steering_log = entries;
        state
    }

    #[test]
    fn steering_appendix_is_empty_without_steering() {
        let state = state_with_steering(Vec::new());
        assert_eq!(build_steering_appendix(&state), "");
        let mut zero_but_logged = state_with_steering(vec![SteeringEntry {
            source: "console".into(),
            ts: "t".into(),
            message: "m".into(),
        }]);
        zero_but_logged.steer_count = 0;
        assert_eq!(build_steering_appendix(&zero_but_logged), "");
    }

    #[test]
    fn steering_appendix_lists_entries_in_order() {
        let state = state_with_steering(vec![
            SteeringEntry {
                source: "orchestrator".into(),
                ts: "t1".into(),
                message: "first".into(),
            },
            SteeringEntry {
                source: "console".into(),
                ts: "t2".into(),
                message: "second".into(),
            },
        ]);
        let appendix = build_steering_appendix(&state);
        assert!(
            appendix.starts_with("\n---\n## Steering log (orchestrator-side, most recent last)\n"),
            "{appendix}"
        );
        assert!(
            appendix.ends_with("- [orchestrator] t1 first\n- [console] t2 second\n"),
            "{appendix}"
        );
    }

    #[test]
    fn report_file_wins_then_fallback_then_missing() {
        let fleet = std::env::temp_dir().join(format!(
            "pilotfish-report-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(fleet.join("runs/auth-20260828141530")).unwrap();
        let report = report_path(&fleet, "auth-20260828141530");
        std::fs::write(&report, "# Fleet Report\n## Status\ndone\n").unwrap();

        let mut state = state_with_steering(Vec::new());
        assert_eq!(report_path(&fleet, "auth-20260828141530"), report);
        match read_report(&fleet, &state) {
            ReportResult::Report(text) => assert!(text.contains("## Status")),
            other => panic!("{other:?}"),
        }

        std::fs::remove_file(&report).unwrap();
        state.last_assistant_text = Some("some final text".into());
        match read_report(&fleet, &state) {
            ReportResult::Fallback(text) => {
                assert!(text.ends_with("falling back to last assistant text]\n\nsome final text"));
            }
            other => panic!("{other:?}"),
        }

        state.last_assistant_text = None;
        assert_eq!(read_report(&fleet, &state), ReportResult::Missing);
    }
}
