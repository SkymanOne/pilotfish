//! Read-only fleet queries: the status table, one run's full state, output
//! and log tails, the final report, waiting for a terminal state, and
//! attaching to a transcript. Statuses are the *derived* view — `dead` when
//! the monitor is gone, `blocked` on a pending question or dialog — taken
//! from `fleet::run::derive_view`, never reimplemented here. (Ported from
//! `statusCore`/`waitCore`/`outputCore`/`logsCore`/`reportCore` in the
//! TypeScript `src/commands.ts` and the attach tail of `src/console`.)

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::cli::ExitCode;
use crate::fleet::run::{self, RunStatus};
use crate::util::{first_line, format_age, now_ms, parse_ts_ms, read_jsonl_tail, tail_text};

use super::steer::resolve_run_with_env;
use super::{CommandResult, fail, ok, print_result};

/// Poll interval for `wait`.
const WAIT_POLL: Duration = Duration::from_millis(2_000);

/// Status data: one element when a name was given, the whole fleet otherwise.
#[derive(Debug, Clone, Serialize)]
pub struct StatusData {
    pub runs: Vec<Value>,
    /// The models pi has configured, so a caller choosing one can see what
    /// there is to choose from. Read from the fleet's pi catalogue; empty
    /// when no worker has ever asked pi.
    pub models: Vec<run::WorkerModel>,
}

/// The end state of a `wait`, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitData {
    pub name: String,
    /// The derived status at the end, or `None` when the wait timed out.
    pub status: Option<String>,
}

/// The text a text query produced, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TextData {
    pub text: String,
}

/// What `report` showed, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportData {
    /// `report` (the file) or `fallback` (captured last assistant text).
    pub kind: String,
    pub text: String,
    pub appendix: String,
}

/// The fleet table, or one run's full state as JSON.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn status(
    name: Option<&str>,
    cwd: Option<&Path>,
    json: bool,
    all: bool,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(status_core(name, cwd, json, all).await?))
}

/// The worker's last assistant text, or the last `n` tool results.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn output(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(output_core(name, cwd, tail).await?))
}

/// Tail the captured raw RPC stream (`pi.log`).
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn logs(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(logs_core(name, cwd, tail).await?))
}

/// The worker's final report plus the steering log; exit 2 when there is none.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn report(name: &str, cwd: Option<&Path>) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(report_core(name, cwd).await?))
}

/// Block until the run reaches a terminal state.
/// Exit 3 on timeout, 4 when it ends stopped/error/dead.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn wait(
    name: &str,
    cwd: Option<&Path>,
    timeout_secs: u64,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(wait_core(name, cwd, timeout_secs).await?))
}

/// Print the tail of one worker's transcript (the live console is `pilotfish`).
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn attach(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(attach_core(name, cwd, tail).await?))
}

/// A run state as observers see it: the stored status replaced by the
/// derived view (`dead`/`blocked` included).
fn derived_json(state: &run::RunState) -> Value {
    let mut value = serde_json::to_value(state).unwrap_or(Value::Null);
    let view = run::derive_view(state, run::is_alive, now_ms()).to_string();
    if let Value::Object(map) = &mut value {
        map.insert("status".into(), Value::String(view));
    }
    value
}

/// Every run state under the fleet dir, newest first, archived included —
/// the `--all` whole-fleet diagnostic path. Unreadable `run.json` files are
/// skipped.
fn load_states(fleet_dir: &Path) -> Vec<run::RunState> {
    run::list_runs(fleet_dir)
        .into_iter()
        .filter_map(|r| run::load_state(&r.run_dir).ok())
        .collect()
}

/// The acting session's run states, archived hidden: what the fleet view
/// shows a session of the runs it owns (`--all` is the unfiltered path).
fn load_session_states(fleet_dir: &Path, session: uuid::Uuid) -> Vec<run::RunState> {
    super::runs_for_acting_session(fleet_dir, session)
        .into_iter()
        .filter_map(|r| run::load_state(&r.run_dir).ok())
        .filter(|state| state.status != RunStatus::Archived)
        .collect()
}

/// The status core. One name: always JSON, including the session file so
/// `--session` can resume it. Otherwise the fleet table, or `--json`.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved, or when a given name
/// matches no run.
pub async fn status_core(
    name: Option<&str>,
    cwd: Option<&Path>,
    json: bool,
    all: bool,
) -> anyhow::Result<CommandResult<StatusData>> {
    status_core_with_env(
        name,
        cwd,
        json,
        all,
        super::ambient_pilotfish_dir().as_deref(),
    )
    .await
}

/// [`status_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn status_core_with_env(
    name: Option<&str>,
    cwd: Option<&Path>,
    json: bool,
    all: bool,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<StatusData>> {
    let fleet = super::resolve_fleet_dir_with_env(cwd, pilotfish_dir).await?;
    let fleet_dir = fleet.paths.root().to_path_buf();
    if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
        let target = run::find_run(&fleet_dir, name)?;
        let mut derived = derived_json(&target.state);
        if let Value::Object(map) = &mut derived {
            // The session file is what `spawn --session` / fleet_spawn(session)
            // resumes, so it travels with the single-run state.
            map.insert(
                "sessionFile".to_string(),
                run::find_session_file(&target.run_dir).map_or(Value::Null, |p| {
                    Value::String(p.to_string_lossy().into_owned())
                }),
            );
        }
        let rendered = serde_json::to_string_pretty(&derived).unwrap_or_else(|_| "{}".into());
        return Ok(ok(
            StatusData {
                runs: vec![derived],
                models: worker_models(&fleet_dir).0,
            },
            vec![rendered],
        ));
    }
    let states = if all {
        // `--all` is the whole-fleet diagnostic path: every session's runs,
        // archived included.
        load_states(&fleet_dir)
    } else {
        load_session_states(&fleet_dir, super::acting_session(&fleet_dir))
    };
    let runs: Vec<Value> = states.iter().map(derived_json).collect();
    let (models, total) = worker_models(&fleet_dir);
    if json {
        let rendered = serde_json::to_string_pretty(&runs)?;
        return Ok(ok(StatusData { runs, models }, vec![rendered]));
    }
    let catalogue = models_line(&models, total);
    if states.is_empty() {
        let mut lines = vec!["(no runs)".to_string()];
        lines.extend(catalogue);
        return Ok(ok(StatusData { runs, models }, lines));
    }
    let mut lines = vec![fleet_table(&states)];
    lines.extend(catalogue);
    Ok(ok(StatusData { runs, models }, lines))
}

/// The models worth naming to a caller choosing one: pi's catalogue,
/// narrowed the way routing narrows it — the configured `[worker] provider`
/// and `[routing] models` — and capped. A real pi install lists hundreds,
/// and every `fleet_status` would otherwise pour them all into the
/// orchestrator's context. Returns the models shown and how many matched.
fn worker_models(fleet_dir: &Path) -> (Vec<run::WorkerModel>, usize) {
    let catalogue = run::read_pi_cache(fleet_dir)
        .map(|c| c.available_models)
        .unwrap_or_default();
    // an unreadable config narrows nothing; status is not the place to fail on it
    let config =
        crate::paths::load_user_config(crate::paths::user_dir().as_deref()).unwrap_or_default();
    let mut models = crate::route::narrow(
        &catalogue,
        config.worker_provider(None),
        &config.routing.models,
    );
    let total = models.len();
    models.truncate(MODELS_SHOWN);
    (models, total)
}

/// How many models a status lists before it only counts them.
const MODELS_SHOWN: usize = 40;

/// One `models:` line naming what `fleet_spawn` can be asked for, or nothing
/// when the catalogue is empty.
fn models_line(models: &[run::WorkerModel], total: usize) -> Option<String> {
    if models.is_empty() {
        return None;
    }
    let names: Vec<String> = models.iter().map(run::WorkerModel::key).collect();
    let rest = total.saturating_sub(models.len());
    Some(if rest == 0 {
        format!("models: {}", names.join(", "))
    } else {
        format!(
            "models: {} … and {rest} more — narrow the list with [worker] provider or [routing] models",
            names.join(", ")
        )
    })
}

/// The fleet table, formatted by hand (the crate pins no table dependency):
/// space-padded columns, two-space gutters, no borders.
fn fleet_table(states: &[run::RunState]) -> String {
    let now = now_ms();
    let headers = [
        "NAME",
        "STATE",
        "LAST-ACTIVITY",
        "LAST-TOOL",
        "STEERED",
        "AGE",
    ];
    let rows: Vec<Vec<String>> = states
        .iter()
        .map(|s| {
            let view = run::derive_view(s, run::is_alive, now).to_string();
            let created = parse_ts_ms(&s.created_at).unwrap_or(now);
            vec![
                s.name.clone(),
                view,
                s.last_activity.clone().unwrap_or_else(|| "-".into()),
                s.last_tool.clone().unwrap_or_else(|| "-".into()),
                s.steer_count.to_string(),
                format_age((now - created).max(0)),
            ]
        })
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let render = |cells: &[&str]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let pad = widths[i].saturating_sub(cell.chars().count());
                format!("{cell}{}", " ".repeat(pad))
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    let mut lines = vec![render(&headers)];
    for row in &rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        lines.push(render(&cells));
    }
    lines.join("\n")
}

/// Wait for a terminal state. Exit 0 settled/archived, 3 timeout,
/// 4 stopped/error/dead.
///
/// # Errors
///
/// Fails when the run cannot be resolved.
pub async fn wait_core(
    name: &str,
    cwd: Option<&Path>,
    timeout_secs: u64,
) -> anyhow::Result<CommandResult<WaitData>> {
    wait_core_with_env(
        name,
        cwd,
        timeout_secs,
        super::ambient_pilotfish_dir().as_deref(),
    )
    .await
}

/// [`wait_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn wait_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    timeout_secs: u64,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<WaitData>> {
    let (_paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let timeout = if timeout_secs > 0 { timeout_secs } else { 600 };
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        if let Ok(state) = run::load_state(&target.run_dir) {
            let derived = run::derive_status(&state, run::is_alive, now_ms());
            if derived.is_terminal() {
                let code = if matches!(derived, RunStatus::Settled | RunStatus::Archived) {
                    ExitCode::Ok
                } else {
                    ExitCode::RunEndedBadly
                };
                return Ok(CommandResult {
                    code,
                    out: vec![format!("{} {derived}", state.name)],
                    err: Vec::new(),
                    data: WaitData {
                        name: state.name,
                        status: Some(derived.to_string()),
                    },
                });
            }
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(fail(
                ExitCode::WaitTimeout,
                vec![format!(
                    "wait: timed out after {timeout}s waiting for {}",
                    target.state.name
                )],
            ));
        }
        tokio::time::sleep(WAIT_POLL.min(deadline - now)).await;
    }
}

/// Last assistant text, or with `--tail n` the last `n` tool results.
///
/// # Errors
///
/// Fails when the run cannot be resolved.
pub async fn output_core(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
) -> anyhow::Result<CommandResult<TextData>> {
    output_core_with_env(name, cwd, tail, super::ambient_pilotfish_dir().as_deref()).await
}

/// [`output_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn output_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<TextData>> {
    let (paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let Some(n) = tail.filter(|&n| n > 0) else {
        let text = target
            .state
            .last_assistant_text
            .as_deref()
            .unwrap_or("(no output yet)")
            .to_string();
        let line = vec![text.clone()];
        return Ok(ok(TextData { text }, line));
    };
    let events: Vec<Value> = read_jsonl_tail(&paths.run_events(&target.run_id), 5_000);
    let ends: Vec<&Value> = events
        .iter()
        .filter(|ev| ev.get("type").and_then(Value::as_str) == Some("tool_execution_end"))
        .collect();
    let ends = &ends[ends.len().saturating_sub(n)..];
    let lines: Vec<String> = if ends.is_empty() {
        vec!["(no tool activity yet)".to_string()]
    } else {
        ends.iter()
            .map(|ev| {
                format!(
                    "{}: {}",
                    ev.get("toolName").and_then(Value::as_str).unwrap_or("tool"),
                    first_line(&result_text_of(ev))
                )
            })
            .collect()
    };
    Ok(ok(
        TextData {
            text: lines.join("\n"),
        },
        lines,
    ))
}

/// Tail the captured raw RPC stream (`pi.log`).
///
/// # Errors
///
/// Fails when the run cannot be resolved.
pub async fn logs_core(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
) -> anyhow::Result<CommandResult<TextData>> {
    logs_core_with_env(name, cwd, tail, super::ambient_pilotfish_dir().as_deref()).await
}

/// [`logs_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn logs_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<TextData>> {
    let (paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let n = tail.filter(|&n| n > 0).unwrap_or(50);
    let text = tail_text(&paths.pi_log(&target.run_id), n);
    if text.trim().is_empty() {
        return Ok(ok(TextData::default(), vec!["(no pi.log yet)".to_string()]));
    }
    Ok(ok(TextData { text: text.clone() }, vec![text]))
}

/// The report core: the report file wins, then the captured last assistant
/// text, then exit 2. The steering log is appended either way.
///
/// # Errors
///
/// Fails when the run cannot be resolved; a run with neither report nor
/// captured output is a `fail` (exit 2), not an error.
pub async fn report_core(
    name: &str,
    cwd: Option<&Path>,
) -> anyhow::Result<CommandResult<ReportData>> {
    report_core_with_env(name, cwd, super::ambient_pilotfish_dir().as_deref()).await
}

/// [`report_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn report_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<ReportData>> {
    let (paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let result = crate::fleet::report::read_report(paths.root(), &target.state);
    let Some(text) = result.text().map(str::to_string) else {
        return Ok(fail(
            ExitCode::NoReport,
            vec![format!(
                "report: no report file and no captured output for {}",
                target.state.name
            )],
        ));
    };
    let kind = match result {
        crate::fleet::report::ReportResult::Report(_) => "report",
        _ => "fallback",
    };
    let appendix = crate::fleet::report::build_steering_appendix(&target.state);
    let mut out = vec![text.clone()];
    if !appendix.is_empty() {
        out.push(appendix.clone());
    }
    Ok(ok(
        ReportData {
            kind: kind.to_string(),
            text,
            appendix,
        },
        out,
    ))
}

/// A static tail of one worker's transcript, rebuilt from `events.jsonl`.
/// Live viewing and steering live in the `pilotfish` console.
///
/// # Errors
///
/// Fails when the run cannot be resolved.
pub async fn attach_core(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
) -> anyhow::Result<CommandResult<Vec<String>>> {
    attach_core_with_env(name, cwd, tail, super::ambient_pilotfish_dir().as_deref()).await
}

/// [`attach_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
async fn attach_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    tail: Option<usize>,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<Vec<String>>> {
    let (paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let n = tail.filter(|&n| n > 0).unwrap_or(40);
    let lines = transcript_tail(&paths.run_events(&target.run_id), n);
    if lines.is_empty() {
        return Ok(ok(Vec::new(), vec!["(no events captured yet)".to_string()]));
    }
    Ok(CommandResult {
        code: ExitCode::Ok,
        out: lines.clone(),
        err: vec!["(static tail — run `pilotfish` for the live console)".to_string()],
        data: lines,
    })
}

/// Rebuild the transcript from `events.jsonl`, keeping the last `keep`
/// renderable lines — the same fold the console's live tail applies.
fn transcript_tail(events_path: &Path, keep: usize) -> Vec<String> {
    let events: Vec<Value> = read_jsonl_tail(events_path, usize::MAX);
    let mut lines: Vec<String> = Vec::new();
    for event in &events {
        apply_transcript_event(&mut lines, event);
    }
    if lines.len() > keep {
        let excess = lines.len() - keep;
        lines.drain(..excess);
    }
    lines
}

/// Clip to `n` characters with an ellipsis.
fn clip_line(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let clipped: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{clipped}…")
    } else {
        s.to_string()
    }
}

/// `[a | b]` for a question's options, empty when there are none.
fn options_text(options: &[String]) -> String {
    if options.is_empty() {
        String::new()
    } else {
        format!(" [{}]", options.join(" | "))
    }
}

/// `result.content[*].text` joined, as the TypeScript `resultTextOf` did it.
fn result_text_of(ev: &Value) -> String {
    ev.get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

/// One-line digest of a tool call's arguments: the first of the usual
/// argument names, else the whole object as JSON.
fn summarize_args(args: &Value) -> String {
    let Some(map) = args.as_object() else {
        return String::new();
    };
    let primary = map
        .get("command")
        .or_else(|| map.get("path"))
        .or_else(|| map.get("file_path"))
        .or_else(|| map.get("pattern"))
        .or_else(|| map.get("url"))
        .or_else(|| map.get("name"))
        .or_else(|| map.get("target"));
    let raw = match primary {
        Some(Value::String(s)) => s.clone(),
        _ => serde_json::to_string(args).unwrap_or_default(),
    };
    clip_line(first_line(&raw).trim(), 80)
}

/// Fold one `events.jsonl` entry into transcript lines (the console's own
/// rendering folds the same shapes; the CLI stays plain text).
fn apply_transcript_event(t: &mut Vec<String>, ev: &Value) {
    let str_of = |key: &str| ev.get(key).and_then(Value::as_str).unwrap_or("");
    let kind = ev.get("type").and_then(Value::as_str);
    match kind {
        Some("task_prompt") => {
            t.push(format!(
                "▶ task: {}",
                clip_line(first_line(str_of("brief")), 200)
            ));
        }
        Some("steering_delivered" | "command_delivered" | "answer_delivered") => {
            t.push(format!("▶ {}: {}", str_of("source"), str_of("message")));
        }
        Some("abort_requested") => t.push("■ abort requested".to_string()),
        Some("worker_question") => {
            let options: Vec<String> = ev
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            t.push(format!(
                "? {}{}",
                clip_line(str_of("question"), 300),
                options_text(&options)
            ));
        }
        Some("worker_progress") => t.push(format!("· {}", clip_line(str_of("message"), 200))),
        Some("thinking_requested") => {
            t.push(format!("· thinking level → {}", str_of("level")));
        }
        Some("worker_question_resolved") => match str_of("how") {
            "timeout" => t.push("! no answer in time; worker proceeds on its own judgment".into()),
            "aborted" => t.push("! question aborted".into()),
            _ => {}
        },
        Some("message_update") => {
            // The monitor stores the streaming delta under `ev`; only a
            // committed `text_end` carries the full text worth showing.
            if let Some(full) = ev
                .get("ev")
                .and_then(|a| a.get("content"))
                .and_then(Value::as_str)
            {
                for line in full.split('\n').filter(|l| !l.trim().is_empty()) {
                    t.push(line.to_string());
                }
            }
        }
        Some("tool_execution_start") => {
            let tool = str_of("toolName");
            let args = ev.get("args").cloned().unwrap_or(Value::Null);
            t.push(
                format!("⚙ {tool} {}", summarize_args(&args))
                    .trim_end()
                    .to_string(),
            );
        }
        Some("tool_execution_end") => {
            let body = result_text_of(ev).trim().to_string();
            let body = if body.is_empty() {
                "(no output)".to_string()
            } else {
                body
            };
            for (i, line) in body.lines().take(4).enumerate() {
                if i == 0 {
                    t.push(format!("  ↳ {line}"));
                } else {
                    t.push(format!("  {line}"));
                }
            }
        }
        Some("agent_settled") => t.push("● settled".to_string()),
        Some("run_failed") => {
            let error = ev
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the worker stopped");
            for (i, line) in error.lines().filter(|l| !l.is_empty()).enumerate() {
                if i == 0 {
                    t.push(format!("✖ {line}"));
                } else {
                    t.push(format!("  {line}"));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::RunState;
    use crate::paths::FleetPaths;
    use crate::util::new_id;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fleet dir anchored at `<dir>/.pilotfish` with one run on disk.
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
        run::save_state(&run_dir, &state).unwrap();
        (dir, paths, run_id.to_string())
    }

    #[tokio::test]
    async fn status_scoped_to_session() {
        let dir = tmp_dir("pilotfish-query-scope-");
        let paths = crate::paths::FleetPaths::new(dir.join(crate::paths::STATE_DIR_NAME));
        let other = uuid::Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap();
        let write = |run_id: &str, name: &str, status: RunStatus, owner: Option<uuid::Uuid>| {
            let run_dir = paths.run_dir(run_id);
            std::fs::create_dir_all(&run_dir).unwrap();
            let mut state = RunState::new(
                paths.root().to_string_lossy().as_ref(),
                run_id,
                name,
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
            state.orchestrator_id = owner;
            run::save_state(&run_dir, &state).unwrap();
        };
        // Mine: owned by the (default) session — plus an unowned legacy run
        // the default session inherits. Theirs: another session's run.
        write("mine-60828141530", "mine", RunStatus::Settled, None);
        write("legacy-60828141531", "legacy", RunStatus::Running, None);
        write(
            "theirs-60828141532",
            "theirs",
            RunStatus::Running,
            Some(other),
        );
        write("old-60828141533", "old", RunStatus::Archived, None);

        // The default session's table: its own + inherited legacy runs,
        // archived hidden; the other session's run is invisible.
        let scoped = status_core_with_env(None, Some(&dir), false, false, None)
            .await
            .unwrap();
        let text = scoped.out.join("\n");
        assert!(text.contains("legacy"), "{text}");
        assert!(text.contains("mine"), "{text}");
        assert!(!text.contains("theirs"), "{text}");
        assert!(!text.contains("old-"), "{text}");
        // `--all` is the whole-fleet diagnostic path: every session, every
        // status.
        let all = status_core_with_env(None, Some(&dir), false, true, None)
            .await
            .unwrap();
        let text = all.out.join("\n");
        assert!(text.contains("theirs"), "{text}");
        assert!(text.contains("old") && text.contains("archived"), "{text}");
        // Named status is by name, fleet-wide: another session's run is
        // addressable explicitly.
        let named = status_core_with_env(Some("theirs"), Some(&dir), false, false, None)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&named.out[0]).unwrap();
        assert_eq!(parsed["name"], "theirs");
    }

    #[tokio::test]
    async fn status_views() {
        {
            let dir = tmp_dir("pilotfish-query-");
            let result = status_core_with_env(None, Some(&dir), false, false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Ok);
            assert_eq!(result.out, vec!["(no runs)"]);
            assert!(result.data.runs.is_empty());
            assert!(result.err.is_empty());
        }
        {
            let (dir, paths, run_id) =
                fleet_with_run("pilotfish-query-solo-", RunStatus::Running, Some(1));
            std::fs::create_dir_all(paths.run_session_dir(&run_id)).unwrap();
            std::fs::write(paths.run_session_dir(&run_id).join("s1.jsonl"), "{}\n").unwrap();
            // A pending question makes the derived view `blocked`.
            let mut state = run::load_state(&paths.run_dir(&run_id)).unwrap();
            state.pending_question = Some(crate::fleet::run::PendingQuestion {
                id: "m_q1".into(),
                question: "which?".into(),
                options: None,
                context: None,
                asked_at: crate::util::now_iso(),
            });
            run::save_state(&paths.run_dir(&run_id), &state).unwrap();

            let result = status_core_with_env(Some("auth"), Some(&dir), false, false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Ok);
            let parsed: Value = serde_json::from_str(&result.out[0]).unwrap();
            assert_eq!(parsed["name"], "auth");
            assert_eq!(parsed["status"], "blocked");
            assert!(
                parsed["sessionFile"]
                    .as_str()
                    .is_some_and(|s| s.ends_with("s1.jsonl")),
                "{}",
                parsed["sessionFile"]
            );
            assert_eq!(result.data.runs.len(), 1);
        }
        {
            let (dir, paths, _run_id) =
                fleet_with_run("pilotfish-query-fleet-", RunStatus::Settled, None);
            // A second, archived run.
            let archived_id = "old-20260828141531";
            let old_dir = paths.run_dir(archived_id);
            std::fs::create_dir_all(&old_dir).unwrap();
            let mut old = RunState::new(
                paths.root().to_string_lossy().as_ref(),
                archived_id,
                "old",
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
            old.status = RunStatus::Archived;
            run::save_state(&old_dir, &old).unwrap();

            let table = status_core_with_env(None, Some(&dir), false, false, None)
                .await
                .unwrap();
            assert_eq!(table.code, ExitCode::Ok);
            let header = &table.out[0];
            assert!(
                header.contains("NAME") && header.contains("STATE") && header.contains("AGE"),
                "{header}"
            );
            assert!(
                table
                    .out
                    .iter()
                    .any(|l| l.contains("auth") && l.contains("settled")),
                "{:?}",
                table.out
            );
            assert!(
                !table.out.join("\n").contains("old-"),
                "archived hidden: {:?}",
                table.out
            );

            let all = status_core_with_env(None, Some(&dir), false, true, None)
                .await
                .unwrap();
            assert!(
                all.out
                    .iter()
                    .any(|l| l.contains("old") && l.contains("archived")),
                "archived shown: {:?}",
                all.out
            );

            let json = status_core_with_env(None, Some(&dir), true, false, None)
                .await
                .unwrap();
            let parsed: Value = serde_json::from_str(&json.out[0]).unwrap();
            assert_eq!(parsed.as_array().unwrap().len(), 1);
            assert_eq!(parsed[0]["status"], "settled");

            // An empty fleet in json mode is an empty array.
            let empty_dir = tmp_dir("pilotfish-query-empty-");
            let empty = status_core_with_env(None, Some(&empty_dir), true, false, None)
                .await
                .unwrap();
            assert_eq!(empty.out, vec!["[]"]);
        }
    }

    #[tokio::test]
    async fn output_and_logs() {
        {
            let (dir, paths, run_id) =
                fleet_with_run("pilotfish-query-out-", RunStatus::Settled, None);
            let mut state = run::load_state(&paths.run_dir(&run_id)).unwrap();
            state.last_assistant_text = Some("Working: wrote hello.txt".into());
            run::save_state(&paths.run_dir(&run_id), &state).unwrap();
            crate::util::append_text(
                &paths.run_events(&run_id),
                &format!("{}\n", r#"{"type":"tool_execution_end","toolName":"bash","result":{"content":[{"text":"hi\n"}]}}"#),
            )
            .unwrap();

            let text = output_core_with_env("auth", Some(&dir), None, None)
                .await
                .unwrap();
            assert_eq!(text.out, vec!["Working: wrote hello.txt"]);
            let trail = output_core_with_env("auth", Some(&dir), Some(5), None)
                .await
                .unwrap();
            assert_eq!(trail.out, vec!["bash: hi"]);

            // No events at all: the placeholder, not an error.
            let (dir2, _p2, _r2) =
                fleet_with_run("pilotfish-query-out2-", RunStatus::Running, Some(1));
            let trail2 = output_core_with_env("auth", Some(&dir2), Some(5), None)
                .await
                .unwrap();
            assert_eq!(trail2.out, vec!["(no tool activity yet)"]);
        }
        {
            let (dir, paths, run_id) =
                fleet_with_run("pilotfish-query-logs-", RunStatus::Running, Some(1));
            let log = paths.pi_log(&run_id);
            for i in 0..10 {
                crate::util::append_text(&log, &format!("line {i}\n")).unwrap();
            }
            let result = logs_core_with_env("auth", Some(&dir), Some(3), None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Ok);
            assert_eq!(result.out, vec!["line 7\nline 8\nline 9"]);

            let (dir2, _p2, _r2) =
                fleet_with_run("pilotfish-query-logs2-", RunStatus::Running, Some(1));
            let none = logs_core_with_env("auth", Some(&dir2), None, None)
                .await
                .unwrap();
            assert_eq!(none.out, vec!["(no pi.log yet)"]);
        }
    }

    #[tokio::test]
    async fn report_exit_codes() {
        let (dir, _paths, _run_id) =
            fleet_with_run("pilotfish-query-rep1-", RunStatus::Settled, None);
        let missing = report_core_with_env("auth", Some(&dir), None)
            .await
            .unwrap();
        assert_eq!(missing.code, ExitCode::NoReport);
        assert!(missing.err[0].contains("no report file and no captured output for auth"));

        // With a report file the appendix is appended after it.
        let (dir, paths, run_id) =
            fleet_with_run("pilotfish-query-rep2-", RunStatus::Settled, None);
        std::fs::write(
            crate::fleet::report::report_path(paths.root(), &run_id),
            "# Fleet Report\n\nDone.\n",
        )
        .unwrap();
        let mut state = run::load_state(&paths.run_dir(&run_id)).unwrap();
        crate::fleet::run::record_steering(&mut state, "console", "t1", "try again");
        run::save_state(&paths.run_dir(&run_id), &state).unwrap();
        let result = report_core_with_env("auth", Some(&dir), None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Ok);
        assert_eq!(result.out[0], "# Fleet Report\n\nDone.\n");
        assert!(result.out[1].contains("## Steering log"));
        assert_eq!(result.data.kind, "report");

        // Without a report file, captured output is the fallback.
        std::fs::remove_file(crate::fleet::report::report_path(paths.root(), &run_id)).unwrap();
        let mut state = run::load_state(&paths.run_dir(&run_id)).unwrap();
        state.last_assistant_text = Some("some final text".into());
        run::save_state(&paths.run_dir(&run_id), &state).unwrap();
        let fallback = report_core_with_env("auth", Some(&dir), None)
            .await
            .unwrap();
        assert_eq!(fallback.data.kind, "fallback");
        assert!(fallback.out[0].contains("falling back to last assistant text"));
    }

    #[tokio::test]
    async fn wait_outcomes() {
        // Settle after 300 ms; the pid (our own process) stays alive, so the
        // run stays Running until then.
        let (dir, paths, run_id) =
            fleet_with_run("pilotfish-query-wait-", RunStatus::Running, Some(1));
        let mut state = run::load_state(&paths.run_dir(&run_id)).unwrap();
        state.pid = Some(std::process::id().cast_signed());
        run::save_state(&paths.run_dir(&run_id), &state).unwrap();
        let late = paths.run_dir(&run_id);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Ok(mut s) = run::load_state(&late) {
                s.status = RunStatus::Settled;
                s.settled_at = Some(crate::util::now_iso());
                run::save_state(&late, &s).unwrap();
            }
        });
        let settled = wait_core_with_env("auth", Some(&dir), 10, None)
            .await
            .unwrap();
        assert_eq!(settled.code, ExitCode::Ok);
        assert_eq!(settled.out, vec!["auth settled"]);
        assert_eq!(settled.data.status.as_deref(), Some("settled"));

        // Timeout: run stays running with a live pid.
        let (dir2, paths2, run2) =
            fleet_with_run("pilotfish-query-wait2-", RunStatus::Running, Some(1));
        let mut state = run::load_state(&paths2.run_dir(&run2)).unwrap();
        state.pid = Some(std::process::id().cast_signed());
        run::save_state(&paths2.run_dir(&run2), &state).unwrap();
        let timed_out = wait_core_with_env("auth", Some(&dir2), 1, None)
            .await
            .unwrap();
        assert_eq!(timed_out.code, ExitCode::WaitTimeout);
        assert!(
            timed_out.err[0].contains("timed out after 1s"),
            "{}",
            timed_out.err[0]
        );
        assert_eq!(timed_out.data.status, None);

        // Stopped run: exit 4.
        let (dir3, _p3, _r3) = fleet_with_run("pilotfish-query-wait3-", RunStatus::Stopped, None);
        let stopped = wait_core_with_env("auth", Some(&dir3), 5, None)
            .await
            .unwrap();
        assert_eq!(stopped.code, ExitCode::RunEndedBadly);
        assert_eq!(stopped.out, vec!["auth stopped"]);

        // A dead run (pid gone mid-run) also reads terminal and bad.
        let (dir4, paths4, run4) =
            fleet_with_run("pilotfish-query-wait4-", RunStatus::Running, Some(1));
        let mut state = run::load_state(&paths4.run_dir(&run4)).unwrap();
        state.pid = Some(i32::MAX - 1); // not our pid, not alive
        run::save_state(&paths4.run_dir(&run4), &state).unwrap();
        let dead = wait_core_with_env("auth", Some(&dir4), 5, None)
            .await
            .unwrap();
        assert_eq!(dead.code, ExitCode::RunEndedBadly);
        assert_eq!(dead.out, vec!["auth dead"]);
    }

    #[tokio::test]
    async fn attach_renders_tail() {
        let (dir, paths, run_id) =
            fleet_with_run("pilotfish-query-attach-", RunStatus::Settled, None);
        let lines = [
            r#"{"type":"task_prompt","brief":"make the thing"}"#,
            r#"{"type":"tool_execution_start","toolName":"bash","args":{"command":"echo hi"}}"#,
            r#"{"type":"tool_execution_end","toolName":"bash","result":{"content":[{"text":"hi\n"}]}}"#,
            r#"{"type":"message_update","ev":{"type":"text_end","content":"line one\nline two"}}"#,
            r#"{"type":"agent_settled"}"#,
        ];
        for line in lines {
            crate::util::append_text(&paths.run_events(&run_id), &format!("{line}\n")).unwrap();
        }
        let result = attach_core_with_env("auth", Some(&dir), None, None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Ok);
        let text = result.out.join("\n");
        assert!(text.contains("▶ task: make the thing"), "{text}");
        assert!(text.contains("⚙ bash echo hi"), "{text}");
        assert!(text.contains("↳ hi"), "{text}");
        assert!(text.contains("line one"), "{text}");
        assert!(text.contains("● settled"), "{text}");
        assert!(
            result.err[0].contains("static tail — run `pilotfish` for the live console"),
            "{}",
            result.err[0]
        );
        // --tail 1 keeps exactly one line.
        let short = attach_core_with_env("auth", Some(&dir), Some(1), None)
            .await
            .unwrap();
        assert_eq!(short.out.len(), 1);

        // No events at all: the placeholder note.
        let (dir2, _p2, _r2) =
            fleet_with_run("pilotfish-query-attach2-", RunStatus::Starting, None);
        let none = attach_core_with_env("auth", Some(&dir2), None, None)
            .await
            .unwrap();
        assert_eq!(none.out, vec!["(no events captured yet)"]);
    }

    #[test]
    fn fold_matches_console() {
        let mut lines = Vec::new();
        apply_transcript_event(
            &mut lines,
            &serde_json::json!({"type":"steering_delivered","source":"console","message":"use tabs"}),
        );
        apply_transcript_event(
            &mut lines,
            &serde_json::json!({"type":"worker_question","question":"which?","options":["a","b"]}),
        );
        apply_transcript_event(
            &mut lines,
            &serde_json::json!({"type":"worker_question_resolved","how":"timeout"}),
        );
        apply_transcript_event(
            &mut lines,
            &serde_json::json!({"type":"run_failed","error":"boom\nfast"}),
        );
        apply_transcript_event(&mut lines, &serde_json::json!({"type":"abort_requested"}));
        assert_eq!(
            lines,
            vec![
                "▶ console: use tabs".to_string(),
                "? which? [a | b]".to_string(),
                "! no answer in time; worker proceeds on its own judgment".to_string(),
                "✖ boom".to_string(),
                "  fast".to_string(),
                "■ abort requested".to_string(),
            ]
        );
    }
}
