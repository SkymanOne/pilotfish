//! Steering a running worker: send, follow-up, answer a pending question or
//! dialog, abort. Every action appends an envelope to the run's
//! `inbox.jsonl` (builders in `fleet::envelope`); the `from` party is the
//! provenance the fleet watcher reads — console-originated steering becomes
//! an event the orchestrator reconciles with instead of undoing. (Ported
//! from `sendCore`/`followupCore`/`answerCore`/`stopCore` in the TypeScript
//! `src/commands.ts`.)

use std::path::Path;

use serde::Serialize;

use crate::cli::ExitCode;
use crate::fleet::envelope::{Envelope, Party, append_envelope};
use crate::fleet::run::{self, RunRef};
use crate::paths::FleetPaths;
use crate::util::now_ms;

use super::{CommandResult, fail, ok, print_result, resolve_fleet_dir_with_env};

/// Locate the fleet dir for `cwd` and the newest non-archived run matching
/// `name` (a name or a full run id), with the `$PILOTFISH_DIR` value injected
/// (production passes the real environment; tests pass `None`). Shared by
/// the whole ops layer.
///
/// # Errors
///
/// Fails on an empty name, an unresolvable fleet dir, or a run that does
/// not match `name`.
pub(crate) async fn resolve_run_with_env(
    name: &str,
    cwd: Option<&Path>,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<(FleetPaths, RunRef)> {
    if name.trim().is_empty() {
        anyhow::bail!("<name> required");
    }
    let fleet = resolve_fleet_dir_with_env(cwd, pilotfish_dir).await?;
    let target = run::find_run(fleet.paths.root(), name)?;
    Ok((fleet.paths, target))
}

/// What a steering action did, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlData {
    pub name: String,
    /// The envelope type: `steer`, `follow_up`, `answer` or `abort`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
}

/// Which steering action to take; picks the envelope type and the refusal copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerKind {
    /// `steer` — delivered after the worker's current tool call.
    Send,
    /// `follow_up` — queued until the current work finishes.
    FollowUp,
    /// `answer` — resolve the question or dialog the worker is blocked on.
    Answer,
    /// `abort` — stop the worker.
    Stop,
}

impl SteerKind {
    /// The envelope `type` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Send => "steer",
            Self::FollowUp => "follow_up",
            Self::Answer => "answer",
            Self::Stop => "abort",
        }
    }

    const fn refusal(self) -> &'static str {
        match self {
            Self::Stop => "nothing to stop",
            Self::Answer => "nothing is waiting for an answer",
            Self::Send | Self::FollowUp => "steering refused",
        }
    }
}

/// The orchestrator party of a fleet's acting session: the session a
/// console or orchestrator monitor most recently recorded in `fleet.json`,
/// or the default session when the fleet has no session rows yet. The
/// default's canonical on-wire spelling (bare `"orchestrator"`) keeps the
/// wire shape identical to what older writers produced.
pub(crate) fn session_orchestrator_party(fleet_dir: &Path) -> Party {
    Party::Orchestrator(super::acting_session(fleet_dir))
}

/// The orchestrator party the CLI attributes steering to, resolved from the
/// same `cwd`/`$PILOTFISH_DIR` the cores use — so provenance can never split
/// from where the envelope lands — falling back to the default session when
/// the fleet cannot be resolved.
async fn cli_orchestrator_party(cwd: Option<&Path>) -> Party {
    cli_orchestrator_party_with_env(cwd, super::ambient_pilotfish_dir().as_deref()).await
}

/// [`cli_orchestrator_party`] with the `$PILOTFISH_DIR` value injected, so tests
/// never resolve an ambient variable into an unrelated fleet.
async fn cli_orchestrator_party_with_env(cwd: Option<&Path>, pilotfish_dir: Option<&str>) -> Party {
    let fleet = resolve_fleet_dir_with_env(cwd, pilotfish_dir).await;
    fleet
        .map(|f| session_orchestrator_party(f.paths.root()))
        .unwrap_or(Party::Orchestrator(
            crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION,
        ))
}

/// Steer a running worker (delivered after its current tool calls).
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved; a terminal run or an empty
/// message is a refusal through the printed exit code.
pub async fn send(
    name: &str,
    cwd: Option<&Path>,
    message: &str,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(
        send_core(name, cwd, message, cli_orchestrator_party(cwd).await).await?,
    ))
}

/// Queue a message for after the worker finishes its current work.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn followup(
    name: &str,
    cwd: Option<&Path>,
    message: &str,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(
        followup_core(name, cwd, message, cli_orchestrator_party(cwd).await).await?,
    ))
}

/// Answer the worker's pending `fleet_ask` question or extension dialog.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn answer(
    name: &str,
    cwd: Option<&Path>,
    question_id: Option<&str>,
    message: &str,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(
        answer_core(
            name,
            cwd,
            question_id,
            message,
            cli_orchestrator_party(cwd).await,
        )
        .await?,
    ))
}

/// Abort a running worker (state becomes stopped).
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn stop(name: &str, cwd: Option<&Path>) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(
        stop_core(name, cwd, cli_orchestrator_party(cwd).await).await?,
    ))
}

/// The CLI entry points attribute steering to the orchestrator — the agent
/// driving these tools. The console passes [`Party::Console`] through the
/// `_core` variants so the watcher can tell the two apart.
///
/// # Errors
///
/// Fails when the run cannot be resolved or the message is empty.
pub async fn send_core(
    name: &str,
    cwd: Option<&Path>,
    message: &str,
    source: Party,
) -> anyhow::Result<CommandResult<ControlData>> {
    send_core_with_env(
        name,
        cwd,
        message,
        source,
        super::ambient_pilotfish_dir().as_deref(),
    )
    .await
}

/// [`send_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn send_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    message: &str,
    source: Party,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<ControlData>> {
    if message.trim().is_empty() {
        anyhow::bail!("send: message required after \"--\"");
    }
    control_core_with_env(
        SteerKind::Send,
        name,
        cwd,
        Some(message),
        None,
        source,
        pilotfish_dir,
    )
    .await
}

/// Queue a message for after the worker finishes its current work.
///
/// # Errors
///
/// Fails when the run cannot be resolved or the message is empty.
pub async fn followup_core(
    name: &str,
    cwd: Option<&Path>,
    message: &str,
    source: Party,
) -> anyhow::Result<CommandResult<ControlData>> {
    followup_core_with_env(
        name,
        cwd,
        message,
        source,
        super::ambient_pilotfish_dir().as_deref(),
    )
    .await
}

/// [`followup_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn followup_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    message: &str,
    source: Party,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<ControlData>> {
    if message.trim().is_empty() {
        anyhow::bail!("followup: message required after \"--\"");
    }
    control_core_with_env(
        SteerKind::FollowUp,
        name,
        cwd,
        Some(message),
        None,
        source,
        pilotfish_dir,
    )
    .await
}

/// Answer the run's pending question or dialog. An explicit `question_id`
/// wins even when it is not the pending one — the monitor routes by id.
///
/// # Errors
///
/// Fails when the run cannot be resolved or the message is empty.
pub async fn answer_core(
    name: &str,
    cwd: Option<&Path>,
    question_id: Option<&str>,
    message: &str,
    source: Party,
) -> anyhow::Result<CommandResult<ControlData>> {
    answer_core_with_env(
        name,
        cwd,
        question_id,
        message,
        source,
        super::ambient_pilotfish_dir().as_deref(),
    )
    .await
}

/// [`answer_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn answer_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    question_id: Option<&str>,
    message: &str,
    source: Party,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<ControlData>> {
    if message.trim().is_empty() {
        anyhow::bail!("answer: message required after \"--\"");
    }
    control_core_with_env(
        SteerKind::Answer,
        name,
        cwd,
        Some(message),
        question_id,
        source,
        pilotfish_dir,
    )
    .await
}

/// Abort a running worker (state becomes stopped).
///
/// # Errors
///
/// Fails when the run cannot be resolved.
pub async fn stop_core(
    name: &str,
    cwd: Option<&Path>,
    source: Party,
) -> anyhow::Result<CommandResult<ControlData>> {
    stop_core_with_env(name, cwd, source, super::ambient_pilotfish_dir().as_deref()).await
}

/// [`stop_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn stop_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    source: Party,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<ControlData>> {
    control_core_with_env(
        SteerKind::Stop,
        name,
        cwd,
        None,
        None,
        source,
        pilotfish_dir,
    )
    .await
}

/// The question id an `answer` targets: the explicit id, else the pending
/// question, else the pending dialog.
fn answer_target_id(state: &run::RunState, question_id: Option<&str>) -> Option<String> {
    question_id
        .map(str::to_string)
        .or_else(|| state.pending_question.as_ref().map(|q| q.id.clone()))
        .or_else(|| state.pending_dialog.as_ref().map(|d| d.id.clone()))
}

/// The shared steering path: refuse terminal runs with the resume hint,
/// target `answer` at the explicit or pending question/dialog id, append the
/// envelope, and report the queueing. The `$PILOTFISH_DIR` value is injected by
/// the per-kind wrappers; `None` means the variable is unset.
async fn control_core_with_env(
    kind: SteerKind,
    name: &str,
    cwd: Option<&Path>,
    message: Option<&str>,
    question_id: Option<&str>,
    source: Party,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<ControlData>> {
    let (paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let state = &target.state;
    let derived = run::derive_status(state, run::is_alive, now_ms());
    if derived.is_terminal() {
        return Ok(fail(
            ExitCode::Error,
            vec![format!(
                "{}: run {} is {} — {}.\nAnswer its open questions in a new brief and resume with:\n  {}",
                kind.as_str(),
                state.name,
                derived,
                kind.refusal(),
                run::resume_hint(state, &target.run_dir),
            )],
        ));
    }
    let envelope = match kind {
        SteerKind::Answer => {
            let Some(message) = message else {
                anyhow::bail!("answer: message required after \"--\"");
            };
            let Some(id) = answer_target_id(state, question_id) else {
                return Ok(fail(
                    ExitCode::Error,
                    vec![format!(
                        "answer: {} has no pending question — use send to steer it instead.",
                        state.name
                    )],
                ));
            };
            Envelope::answer(source, target.worker_party(), message, Some(id))
        }
        SteerKind::Stop => Envelope::abort(source, target.worker_party()),
        SteerKind::Send => {
            let Some(message) = message else {
                anyhow::bail!("send: message required after \"--\"");
            };
            Envelope::steer(source, target.worker_party(), message)
        }
        SteerKind::FollowUp => {
            let Some(message) = message else {
                anyhow::bail!("followup: message required after \"--\"");
            };
            Envelope::follow_up(source, target.worker_party(), message)
        }
    };
    append_envelope(&paths.run_inbox(&target.run_id), &envelope)?;
    let line = match kind {
        SteerKind::Answer => format!(
            "answer queued for {} (question {})",
            state.name,
            answer_target_id(state, question_id).unwrap_or_default()
        ),
        SteerKind::Stop => format!("abort requested for {}", state.name),
        other => format!("{} queued for {}", other.as_str(), state.name),
    };
    let data = ControlData {
        name: state.name.clone(),
        kind: kind.as_str().to_string(),
        question_id: match kind {
            SteerKind::Answer => Some(
                envelope
                    .payload
                    .get("questionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
            _ => None,
        },
    };
    Ok(ok(data, vec![line]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::envelope::{DEFAULT_ORCHESTRATOR_SESSION, Decoded};
    use crate::fleet::run::{PendingDialog, PendingQuestion, RunState, RunStatus};
    use crate::util::new_id;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// The default orchestrator party, as the CLI and MCP attribute steering.
    fn orch() -> Party {
        Party::Orchestrator(DEFAULT_ORCHESTRATOR_SESSION)
    }

    /// The state's uuid for an envelope's `to` address.
    fn run_uuid(paths: &FleetPaths, run_id: &str) -> Uuid {
        crate::fleet::run::load_state(&paths.run_dir(run_id))
            .unwrap()
            .uuid
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fleet dir with one run whose state is already on disk. The fleet
    /// dir anchors at `<dir>/.pilotfish`, exactly like a non-git target.
    fn fleet_with_run(
        name: &str,
        status: RunStatus,
        pid: Option<i32>,
    ) -> (PathBuf, FleetPaths, String) {
        let dir = tmp_dir(name);
        let paths = FleetPaths::new(dir.join(crate::paths::STATE_DIR_NAME));
        let run_id = "auth-20260828141530";
        let run_dir = paths.run_dir(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut state = RunState::new(
            paths.root().to_string_lossy().as_ref(),
            run_id,
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
        state.status = status;
        state.pid = pid;
        crate::fleet::run::save_state(&run_dir, &state).unwrap();
        (dir, paths, run_id.to_string())
    }

    fn inbox_lines(paths: &FleetPaths, run_id: &str) -> Vec<Envelope> {
        let raw = std::fs::read_to_string(paths.run_inbox(run_id)).unwrap_or_default();
        raw.lines()
            .filter(|l| !l.is_empty())
            .map(Envelope::parse_line)
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn send_appends_a_steer_envelope_and_queues() {
        let (dir, paths, run_id) = fleet_with_run("pilotfish-steer-", RunStatus::Running, Some(1));
        let result = send_core_with_env("auth", Some(&dir), "use tabs", orch(), None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Ok);
        assert_eq!(result.out, vec!["steer queued for auth"]);
        let envelopes = inbox_lines(&paths, &run_id);
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].from, orch());
        assert_eq!(envelopes[0].to, Party::worker(run_uuid(&paths, &run_id)));
        assert_eq!(envelopes[0].decode(), Some(Decoded::Steer("use tabs")));
        assert_eq!(
            result.data,
            ControlData {
                name: "auth".into(),
                kind: "steer".into(),
                question_id: None,
            }
        );
    }

    #[tokio::test]
    async fn followup_and_stop_append_their_envelope_types() {
        let (dir, paths, run_id) = fleet_with_run("pilotfish-steer-", RunStatus::Running, Some(1));
        followup_core_with_env("auth", Some(&dir), "then fmt", orch(), None)
            .await
            .unwrap();
        stop_core_with_env("auth", Some(&dir), Party::Console, None)
            .await
            .unwrap();
        let envelopes = inbox_lines(&paths, &run_id);
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].kind, "follow_up");
        assert_eq!(envelopes[1].kind, "abort", "stop appends an abort envelope");
        // Provenance is threaded honestly.
        assert_eq!(envelopes[0].from, orch());
        assert_eq!(envelopes[1].from, Party::Console);
        let payload = &envelopes[1].payload;
        assert!(
            payload.as_object().is_some_and(serde_json::Map::is_empty),
            "abort carries exactly {{}}: {payload}"
        );
    }

    #[tokio::test]
    async fn empty_messages_are_refused_before_touching_the_run() {
        let (dir, paths, run_id) = fleet_with_run("pilotfish-steer-", RunStatus::Running, Some(1));
        for (core, expect) in [
            (
                send_core_with_env("auth", Some(&dir), "  ", orch(), None).await,
                "send: message",
            ),
            (
                followup_core_with_env("auth", Some(&dir), "", orch(), None).await,
                "followup: message",
            ),
            (
                answer_core_with_env("auth", Some(&dir), None, "", orch(), None).await,
                "answer: message",
            ),
        ] {
            let err = core.unwrap_err().to_string();
            assert!(err.contains(expect), "{expect} <- {err}");
        }
        // Nothing was appended.
        assert!(inbox_lines(&paths, &run_id).is_empty());
    }

    #[tokio::test]
    async fn steering_a_terminal_run_refuses_with_the_resume_hint() {
        let (dir, paths, run_id) = fleet_with_run("pilotfish-steer-", RunStatus::Settled, None);
        for core in [
            send_core_with_env("auth", Some(&dir), "m", orch(), None).await,
            followup_core_with_env("auth", Some(&dir), "m", orch(), None).await,
        ] {
            let result = core.unwrap();
            assert_eq!(result.code, ExitCode::Error);
            let err = result.err.join("\n");
            assert!(err.contains("is settled — steering refused"), "{err}");
            assert!(
                err.contains("pilotfish spawn auth-2 --session"),
                "carries the copy-pasteable resume command: {err}"
            );
        }
        let stop = stop_core_with_env("auth", Some(&dir), orch(), None)
            .await
            .unwrap();
        assert_eq!(stop.code, ExitCode::Error);
        assert!(stop.err[0].contains("nothing to stop"), "{}", stop.err[0]);
        let answer = answer_core_with_env("auth", Some(&dir), None, "x", orch(), None)
            .await
            .unwrap();
        assert!(answer.err[0].contains("nothing is waiting for an answer"));
        // Nothing was written.
        assert!(inbox_lines(&paths, &run_id).is_empty());
    }

    #[tokio::test]
    async fn answer_needs_a_question_dialog_or_explicit_id() {
        let (dir, paths, run_id) = fleet_with_run("pilotfish-steer-", RunStatus::Running, Some(1));
        let refused = answer_core_with_env("auth", Some(&dir), None, "argon2", orch(), None)
            .await
            .unwrap();
        assert_eq!(refused.code, ExitCode::Error);
        assert!(
            refused.err[0].contains("no pending question — use send"),
            "{}",
            refused.err[0]
        );
        assert!(inbox_lines(&paths, &run_id).is_empty());
    }

    #[tokio::test]
    async fn answer_targets_the_pending_question_by_default() {
        let (dir, paths, run_id) = fleet_with_run("pilotfish-steer-", RunStatus::Running, Some(1));
        let run_dir = paths.run_dir(&run_id);
        let mut state = crate::fleet::run::load_state(&run_dir).unwrap();
        state.pending_question = Some(PendingQuestion {
            id: "m_q1".into(),
            question: "which fixture?".into(),
            options: None,
            context: None,
            asked_at: crate::util::now_iso(),
        });
        crate::fleet::run::save_state(&run_dir, &state).unwrap();

        let result = answer_core_with_env("auth", Some(&dir), None, "argon2", Party::Console, None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Ok);
        assert_eq!(result.out, vec!["answer queued for auth (question m_q1)"]);
        assert_eq!(result.data.question_id.as_deref(), Some("m_q1"));
        let envelopes = inbox_lines(&paths, &run_id);
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].from, Party::Console);
        assert_eq!(
            envelopes[0].decode(),
            Some(Decoded::Answer {
                message: Some("argon2"),
                question_id: Some("m_q1")
            })
        );
    }

    #[tokio::test]
    async fn answer_falls_back_to_the_pending_dialog_and_explicit_ids_win() {
        let (dir, paths, run_id) = fleet_with_run("pilotfish-steer-", RunStatus::Running, Some(1));
        let run_dir = paths.run_dir(&run_id);
        let mut state = crate::fleet::run::load_state(&run_dir).unwrap();
        state.pending_dialog = Some(PendingDialog {
            id: "ui-9".into(),
            method: "confirm".into(),
            question: "overwrite?".into(),
            options: None,
            context: None,
            asked_at: crate::util::now_iso(),
        });
        crate::fleet::run::save_state(&run_dir, &state).unwrap();

        let result = answer_core_with_env("auth", Some(&dir), None, "yes", orch(), None)
            .await
            .unwrap();
        assert_eq!(result.data.question_id.as_deref(), Some("ui-9"));
        let envelopes = inbox_lines(&paths, &run_id);
        assert_eq!(
            envelopes[0].decode(),
            Some(Decoded::Answer {
                message: Some("yes"),
                question_id: Some("ui-9")
            })
        );

        // An explicit id wins even when it is not the pending one.
        let result = answer_core_with_env("auth", Some(&dir), Some("q_other"), "no", orch(), None)
            .await
            .unwrap();
        assert_eq!(result.data.question_id.as_deref(), Some("q_other"));
        let envelopes = inbox_lines(&paths, &run_id);
        assert_eq!(envelopes.len(), 2);
        assert_eq!(
            envelopes[1].decode(),
            Some(Decoded::Answer {
                message: Some("no"),
                question_id: Some("q_other")
            })
        );
    }

    #[tokio::test]
    async fn unknown_runs_and_empty_names_are_errors() {
        let dir = tmp_dir("pilotfish-steer-none-");
        let err = send_core_with_env("ghost", Some(&dir), "m", orch(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("No run found"), "{err}");
        let err = send_core_with_env("  ", Some(&dir), "m", orch(), None)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "<name> required");
    }

    #[tokio::test]
    async fn the_cli_attributes_steering_to_the_fleets_acting_session() {
        let dir = tmp_dir("pilotfish-steer-session-");
        let paths = FleetPaths::new(dir.join(crate::paths::STATE_DIR_NAME));
        let run_id = "auth-20260828141530";
        let run_dir = paths.run_dir(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut state = RunState::new(
            paths.root().to_string_lossy().as_ref(),
            run_id,
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
        state.status = RunStatus::Running;
        state.pid = Some(std::process::id().cast_signed());
        crate::fleet::run::save_state(&run_dir, &state).unwrap();
        // fleet.json names the fleet's session; the wrapper attributes
        // the envelope to *it*, not the default. The `$PILOTFISH_DIR` value is
        // injected as `None`, so the ambient environment can never redirect
        // this test's resolution.
        let mut store = crate::orch::session::FleetSessions::new();
        store.upsert(crate::orch::session::OrchestratorSession::new("/repo"));
        let session = store.last_used().unwrap().uuid;
        crate::orch::session::save(paths.root(), &mut store).unwrap();

        let party = cli_orchestrator_party_with_env(Some(&dir), None).await;
        assert_eq!(party, Party::Orchestrator(session));
        send_core_with_env("auth", Some(&dir), "use tabs", party, None)
            .await
            .unwrap();
        let envelopes = inbox_lines(&paths, run_id);
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].from, Party::Orchestrator(session));
        // Without a fleet.json the wrapper falls back to the default
        // session's canonical on-wire spelling.
        let dir2 = tmp_dir("pilotfish-steer-nosession-");
        let paths2 = FleetPaths::new(dir2.join(crate::paths::STATE_DIR_NAME));
        let run_dir2 = paths2.run_dir(run_id);
        std::fs::create_dir_all(&run_dir2).unwrap();
        let mut state2 = state.clone();
        state2.orchestrator_id = None;
        crate::fleet::run::save_state(&run_dir2, &state2).unwrap();
        let party = cli_orchestrator_party_with_env(Some(&dir2), None).await;
        assert_eq!(party, orch());
        stop_core_with_env("auth", Some(&dir2), party, None)
            .await
            .unwrap();
        let envelopes = inbox_lines(&paths2, run_id);
        assert_eq!(envelopes[0].from, orch());
    }
}
