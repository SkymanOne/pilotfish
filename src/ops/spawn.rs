//! Spawning a worker: validate the brief and model, create the worktree,
//! write `run.json`, boot the detached monitor. (Ported from the TypeScript
//! `src/spawn.ts` and `spawnCore` in `src/commands.ts`.)

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;

use crate::cli::ExitCode;
use crate::fleet::run::{self, RunRef, RunState, RunStatus};
use crate::git;
use crate::paths::FleetPaths;
use crate::util::{now_ms, run_id_for, sanitize_name};
use crate::worker::models::{check_model, pi_bin_spec};

use super::{CommandResult, fail, print_result, resolve_fleet_dir_with_env};

/// Everything `spawn` needs to know; constructed verbatim by `main.rs` from
/// the parsed CLI, so the field set is a frozen contract.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub name: String,
    pub brief: String,
    pub cwd: Option<std::path::PathBuf>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub thinking: Option<String>,
    /// `false` for `--no-worktree`: run in place, read-only tasks.
    pub worktree: bool,
    pub base: Option<String>,
    pub skill: Option<String>,
    pub append_system_prompt: Option<String>,
    pub session: Option<String>,
    pub tools: Option<String>,
    pub exclude_tools: Option<String>,
    /// Override `[routing] enabled` for this spawn: `--route` / `--no-route`.
    pub route: Option<bool>,
}

/// What [`spawn_core`] did, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnData {
    pub run_id: String,
    pub run_dir: String,
    pub fleet_dir: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
}

/// A run [`create_run`] materialised on disk.
#[derive(Debug, Clone)]
pub struct CreatedRun {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub paths: FleetPaths,
    pub state: RunState,
    pub worktree_path: Option<PathBuf>,
}

/// Spawn one worker: the CLI entry point. Prints the core's lines and hands
/// back its exit code; hard errors (no brief, no name, bad cwd) surface
/// through `main` as `pilotfish: …` and exit 1.
///
/// # Errors
///
/// Fails on a missing brief or name, a bad `cwd`, or a name that already
/// has a live run; model refusal comes back through the printed exit code.
pub async fn spawn_run(request: SpawnRequest) -> anyhow::Result<ExitCode> {
    Ok(print_result(spawn_core(request).await?))
}

/// The spawn core: the model is checked before a worktree and a branch exist,
/// so a wrong name costs a second. The unknown-model refusal is a `fail`,
/// not an error, and keeps the TypeScript exit code (`2`).
///
/// # Errors
///
/// Fails on a missing brief, an unresolvable `cwd`, a same-second name
/// collision, or a name whose previous run is still live.
pub async fn spawn_core(request: SpawnRequest) -> anyhow::Result<CommandResult<SpawnData>> {
    spawn_core_with_env(request, super::ambient_pilotfish_dir().as_deref()).await
}

/// [`spawn_core`] with the `$PILOTFISH_DIR` value injected (tests and the MCP
/// server pass it; MCP passes its own captured value).
pub(crate) async fn spawn_core_with_env(
    request: SpawnRequest,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<SpawnData>> {
    spawn_core_with_dirs(request, pilotfish_dir, crate::paths::user_dir().as_deref()).await
}

/// [`spawn_core_with_env`] with the user config dir injected too, so tests
/// never resolve an ambient `~/.pilotfish`.
pub(crate) async fn spawn_core_with_dirs(
    request: SpawnRequest,
    pilotfish_dir: Option<&str>,
    user_config_dir: Option<&Path>,
) -> anyhow::Result<CommandResult<SpawnData>> {
    if request.brief.trim().is_empty() {
        anyhow::bail!("spawn: task brief required after \"--\"");
    }
    let config = crate::paths::load_user_config(user_config_dir)?;
    // The per-session cap is enforced before a worktree, a branch or a
    // monitor exist: `[limits] max_workers_per_session` (default 3) limits
    // how many *live* workers one session may hold — settled, archived and
    // dead runs free their slot. Zero means no spawning at all.
    let fleet_dir = resolve_fleet_dir_with_env(request.cwd.as_deref(), pilotfish_dir)
        .await?
        .paths
        .root()
        .to_path_buf();
    let session = super::acting_session(&fleet_dir);
    // Routing goes first. It can wait on the network, and between counting
    // the live workers and creating the new one nothing slow may happen, or
    // two parallel spawns both pass a cap only one of them fits under.
    // What it picks is what gets validated below.
    let mut request = request;
    let in_flight = super::live_runs_for_session(&fleet_dir, session);
    let routing = route_request(
        &mut request,
        &config,
        &fleet_dir,
        &in_flight,
        model_ask_timeout_ms(),
    )
    .await;
    let cap = config.max_workers_per_session();
    let live = super::live_runs_for_session(&fleet_dir, session);
    if live.len() >= cap {
        let holders = live
            .iter()
            .map(|s| {
                format!(
                    "  {} ({}) — {}",
                    s.id,
                    s.name,
                    run::derive_view(s, run::is_alive, crate::util::now_ms())
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let hint = if live.is_empty() {
            "the cap is set to 0 — raise [limits] max_workers_per_session in the \
user config (~/.pilotfish/config.toml) to spawn at all."
        } else {
            "finish or clean up one of these before spawning another."
        };
        return Ok(fail(
            ExitCode::Error,
            vec![format!(
                "spawn: refused — this session already has {} live worker(s), the \
per-session cap is {cap} ([limits] max_workers_per_session):\n{holders}\n{hint}",
                live.len()
            )],
        ));
    }
    // The resolved model is the one the monitor will actually run, so a bad
    // name from the config is refused here too — before a worktree exists.
    let model = config.worker_model(request.model.as_deref());
    let pi_bin = pi_bin_spec();
    if let Some(bad) = check_model(&pi_bin, model).await? {
        return Ok(fail(ExitCode::NoReport, vec![format!("spawn: {bad}")]));
    }
    let mut created = create_run_with_env(&request, pilotfish_dir, user_config_dir).await?;
    if let Some(routing) = routing.clone() {
        created.state.routing = Some(routing);
        run::save_state(&created.run_dir, &created.state)?;
    }
    let mut err: Vec<String> = Vec::new();
    if !created.state.is_git && request.worktree {
        err.push("warning: target is not a git repo — running in place without a worktree".into());
    }
    launch_monitor(&created.paths, &created.run_id)?;
    let run_dir = created.run_dir.to_string_lossy().into_owned();
    let mut out = vec![
        format!("Spawned {}", created.run_id),
        format!("  state:    {run_dir}/run.json"),
        format!("  logs:     {run_dir}/{{events.jsonl,inbox.jsonl,outbox.jsonl,pi.log}}"),
        format!("  fleet dir: {}", created.paths.root().display()),
    ];
    if let Some(worktree) = &created.worktree_path {
        out.push(format!("  worktree: {}", worktree.display()));
    }
    if let Some(branch) = &created.state.branch {
        out.push(format!("  branch:   {branch}"));
    }
    if let Some(routing) = &routing {
        out.push(format!("  routing:  {}", routing.note));
        if routing.parallel_safe == Some(false) {
            err.push(
                "warning: this brief may touch the same files as a worker already running"
                    .to_string(),
            );
        }
    }
    let data = SpawnData {
        run_id: created.run_id,
        run_dir,
        fleet_dir: created.paths.root().to_string_lossy().into_owned(),
        worktree: created
            .worktree_path
            .map(|p| p.to_string_lossy().into_owned()),
        branch: created.state.branch.clone(),
    };
    // The no-git warning lives in `err`, so `ok` (which zeroes it) is not used.
    Ok(CommandResult {
        code: ExitCode::Ok,
        out,
        err,
        data,
    })
}

/// How long a spawn waits for the human to choose a model Jev was unsure
/// of, unless `$PILOTFISH_ASK_TIMEOUT_MS` says otherwise: the same ten minutes a
/// worker's `fleet_ask` waits.
const MODEL_ASK_TIMEOUT_MS: i64 = 10 * 60_000;

/// How often the waiting spawn looks for the console's answer.
const MODEL_ASK_POLL_MS: u64 = 200;

fn model_ask_timeout_ms() -> i64 {
    std::env::var(crate::paths::env_var("ASK_TIMEOUT_MS"))
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(MODEL_ASK_TIMEOUT_MS)
}

/// Ask Jev which model, thinking level and worktree this brief wants, and
/// fold the answer into the request. Anything the caller pinned explicitly
/// wins: routing fills gaps, it never overrides a decision already made.
///
/// Two cycles. The first chooses the model; when Jev is less sure than
/// `[routing] confidence_threshold`, the human is asked in the console and
/// the spawn waits up to `ask_timeout_ms` for the answer, keeping the
/// configured model if none comes. The second chooses the thinking level
/// among the levels of the model that will run, whoever chose it.
///
/// Returns what was decided, for the run record and the spawn's output.
/// `None` means routing did not run — off, unkeyed, or nothing to choose
/// between — and the spawn proceeds exactly as it always did.
async fn route_request(
    request: &mut SpawnRequest,
    config: &crate::paths::UserConfig,
    fleet_dir: &Path,
    in_flight: &[RunState],
    ask_timeout_ms: i64,
) -> Option<crate::route::Routing> {
    let mut routing_config = config.routing.clone();
    if let Some(explicit) = request.route {
        routing_config.enabled = explicit;
    }
    // Nothing to decide when the caller already decided everything.
    if request.model.is_some() && request.thinking.is_some() {
        return None;
    }
    // The key is only looked up once routing is on: reading the credential
    // store can put up a system dialog, and a spawn that was never going to
    // route has no business causing one.
    if !routing_config.enabled {
        return None;
    }
    // routing is on, so a missing key is said rather than silently skipped
    let key = match tokio::task::spawn_blocking(crate::secrets::typesafe_key).await {
        Ok(Ok(Some((key, _)))) => key,
        Ok(Ok(None)) => {
            return Some(crate::route::Routing {
                note: "not routed: no TypeSafe key — set one with /routing in the console, \
or $PILOTFISH_TYPESAFE_API_KEY"
                    .to_string(),
                ..crate::route::Routing::default()
            });
        }
        Ok(Err(err)) => {
            return Some(crate::route::Routing {
                note: format!("not routed: {err}"),
                ..crate::route::Routing::default()
            });
        }
        Err(_) => return None,
    };
    route_with_key(
        request,
        config,
        &routing_config,
        fleet_dir,
        in_flight,
        key.expose(),
        ask_timeout_ms,
    )
    .await
}

/// [`route_request`] once routing is on and the key is in hand — the part
/// the tests drive, since the key otherwise comes from the environment or
/// the credential store.
async fn route_with_key(
    request: &mut SpawnRequest,
    config: &crate::paths::UserConfig,
    routing_config: &crate::paths::RoutingConfig,
    fleet_dir: &Path,
    in_flight: &[RunState],
    key: &str,
    ask_timeout_ms: i64,
) -> Option<crate::route::Routing> {
    let catalogue = run::read_pi_cache(fleet_dir)
        .map(|cache| cache.available_models)
        .unwrap_or_default();
    let repo_root = fleet_dir.parent().unwrap_or(fleet_dir).to_path_buf();
    let provider = config.worker_provider(request.provider.as_deref());
    let fallback_model = config.worker_model(request.model.as_deref());
    let brief = crate::route::Brief {
        name: &request.name,
        brief: &request.brief,
        repo_root: &repo_root,
        in_flight,
        catalogue: &catalogue,
        provider,
        fallback_model,
        model_pinned: request.model.is_some(),
    };
    let judgment = crate::route::judge(&brief, routing_config, Some(key)).await?;
    let mut routing = judgment.routing;
    if let Some(options) = judgment.ask {
        let question = crate::route::ModelQuestion {
            id: format!(
                "{}-{}",
                sanitize_name(&request.name),
                crate::util::short_uuid(&uuid::Uuid::new_v4())
            ),
            name: request.name.clone(),
            brief: crate::util::first_line(&request.brief).to_string(),
            confidence: routing.confidence,
            threshold: routing_config.confidence_threshold(),
            options,
            fallback: fallback_model.map(str::to_string),
            asked_at: crate::util::now_iso(),
            pid: std::process::id(),
            deadline_ms: now_ms() + ask_timeout_ms,
        };
        let kept = fallback_model.map_or_else(
            || "pi's default model".to_string(),
            |model| format!("the configured {model}"),
        );
        let outcome = match ask_human(&FleetPaths::new(fleet_dir), &question).await {
            Asked::Chose(chosen) => match catalogue.iter().find(|m| m.key() == chosen) {
                Some(model) => {
                    routing.model = Some(model.id.clone());
                    routing.provider = Some(model.provider.clone());
                    format!("you chose {chosen}")
                }
                None => format!("the answer named no candidate; kept {kept}"),
            },
            Asked::KeptFallback => format!("you kept {kept}"),
            Asked::NoAnswer => format!(
                "no answer within {}; kept {kept}",
                wait_words(ask_timeout_ms)
            ),
            Asked::NoConsole => format!("no console open to ask; kept {kept}"),
        };
        routing.note = format!("{}; {outcome}", routing.note);
    }
    // the second cycle: thinking, for whichever model will actually run
    if request.thinking.is_none()
        && let Some(runs) = crate::route::model_that_runs(&brief, &routing)
        && let Some((level, confidence)) =
            crate::route::choose_thinking(&brief, runs, routing_config, key).await
    {
        routing.note = format!("{}; thinking {level} ({confidence:.2})", routing.note);
        routing.thinking = Some(level);
    }
    // the candidates were already narrowed to any pinned provider, so the
    // routed model's own provider can never contradict it
    if request.model.is_none()
        && let Some(model) = &routing.model
    {
        request.model = Some(model.clone());
        request.provider = routing.provider.clone().or(request.provider.clone());
    }
    if request.thinking.is_none()
        && let Some(thinking) = &routing.thinking
    {
        request.thinking = Some(thinking.clone());
    }
    // A read-only brief needs no branch of its own; a `--no-worktree` the
    // caller asked for is never overridden the other way.
    if request.worktree && routing.worktree == Some(false) {
        request.worktree = false;
    }
    Some(routing)
}

/// How a model question put to the human ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    /// A candidate, as `provider:id`.
    Chose(String),
    /// The human chose to keep the configured model.
    KeptFallback,
    NoAnswer,
    /// Nobody to ask: no console is open on this fleet.
    NoConsole,
}

/// Put `question` to the human in the console and wait for the answer, up
/// to its deadline. The question is taken down however this ends —
/// including when the spawn itself is cancelled mid-wait, which is what the
/// guard is for.
async fn ask_human(paths: &FleetPaths, question: &crate::route::ModelQuestion) -> Asked {
    struct Posted<'a>(&'a FleetPaths, &'a str);
    impl Drop for Posted<'_> {
        fn drop(&mut self) {
            crate::route::remove_question(self.0, self.1);
        }
    }
    if crate::paths::console_holder(&paths.console_lock()).is_none() {
        return Asked::NoConsole;
    }
    if crate::route::post_question(paths, question).is_err() {
        return Asked::NoConsole;
    }
    let _posted = Posted(paths, &question.id);
    loop {
        if let Some(answer) = crate::route::read_answer(paths, &question.id) {
            return answer.model.map_or(Asked::KeptFallback, Asked::Chose);
        }
        if now_ms() >= question.deadline_ms {
            return Asked::NoAnswer;
        }
        tokio::time::sleep(std::time::Duration::from_millis(MODEL_ASK_POLL_MS)).await;
    }
}

/// A wait in words: "10 min", or seconds for the short waits tests use.
fn wait_words(ms: i64) -> String {
    if ms >= 60_000 {
        format!("{} min", ms / 60_000)
    } else {
        format!("{} s", (ms / 1000).max(1))
    }
}

/// Create the run: sanitise the name, stamp the run id, cut the worktree on
/// its own branch from `--base` (or HEAD), and write the initial `run.json`.
///
/// # Errors
///
/// Fails on a missing name, a `cwd` that does not exist, a same-name spawn
/// within one second, a name whose previous run is still live, or a
/// worktree git cannot cut. A prior terminal run of the same name is
/// archived here, not refused.
pub async fn create_run(request: &SpawnRequest) -> anyhow::Result<CreatedRun> {
    create_run_with_env(
        request,
        super::ambient_pilotfish_dir().as_deref(),
        crate::paths::user_dir().as_deref(),
    )
    .await
}

/// [`create_run`] with the `$PILOTFISH_DIR` value and the user config dir
/// injected (tests pass `None`s).
async fn create_run_with_env(
    request: &SpawnRequest,
    pilotfish_dir: Option<&str>,
    user_config_dir: Option<&Path>,
) -> anyhow::Result<CreatedRun> {
    let name = sanitize_name(&request.name);
    if name.is_empty() {
        anyhow::bail!("spawn: <name> required");
    }
    // User-level defaults sit under the explicit flags: the run records the
    // resolved model/provider, which is what the monitor hands to pi.
    let config = crate::paths::load_user_config(user_config_dir)?;
    let model = config
        .worker_model(request.model.as_deref())
        .map(str::to_string);
    let provider = config
        .worker_provider(request.provider.as_deref())
        .map(str::to_string);
    let fleet = resolve_fleet_dir_with_env(request.cwd.as_deref(), pilotfish_dir).await?;
    // The fixed layout plus the gitignore entry; idempotent.
    fleet.paths.ensure()?;
    // The run's identity is its uuid; the id and directory name derive from
    // it (`<alias>-<short-uuid>`), and the same uuid is recorded in state so
    // ownership and addressing refer to the run itself.
    let uuid = uuid::Uuid::new_v4();
    let run_id = run_id_for(&name, &uuid);
    let run_dir = fleet.paths.run_dir(&run_id);
    if run_dir.exists() {
        anyhow::bail!(
            "spawn: run {run_id} already exists (same name spawned twice within one second) — retry"
        );
    }
    // A name may have only one live run. A prior terminal run of the same
    // name is archived here as part of the spawn, so `cleanup <name>` and
    // the other ops never resolve to a stale twin; a still-running namesake
    // refuses the spawn — silently duplicating it is how a live worker was
    // archived by accident before.
    for prior in live_namesakes(fleet.paths.root(), &name) {
        let derived = run::derive_status(&prior.state, run::is_alive, now_ms());
        if !derived.is_terminal() {
            anyhow::bail!(
                "spawn: run {} (name \"{}\") is still {derived} — a name may have only \
one live run; stop or clean it first, or use another name.",
                prior.run_id,
                name
            );
        }
        let mut stale = prior.state.clone();
        stale.status = RunStatus::Archived;
        run::save_state(&prior.run_dir, &stale)?;
    }

    let (worktree_path, branch) = match fleet.repo_root.as_deref().filter(|_| request.worktree) {
        Some(root) => {
            let worktrees_dir = fleet.paths.root().join("worktrees");
            let created = git::ensure_worktree(
                root,
                &worktrees_dir,
                &run_id,
                &name,
                request.base.as_deref(),
            )
            .await?;
            (Some(created.worktree_path), Some(created.branch))
        }
        None => (None, None),
    };
    // The base commit is pinned by ensure_worktree and must survive into
    // run.json even when the run has no worktree — diff falls back to the
    // recorded ref, so only set it from the worktree path we actually got.
    let base_commit = if let (Some(root), true) = (fleet.repo_root.as_deref(), request.worktree) {
        Some(git::resolve_commit(root, request.base.as_deref().unwrap_or("HEAD")).await?)
    } else {
        None
    };

    std::fs::create_dir_all(&run_dir)?;
    let mut state = RunState::new(
        fleet.paths.root().to_string_lossy().as_ref(),
        &run_id,
        &name,
        &fleet.target_dir.to_string_lossy(),
        &request.brief,
        worktree_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        branch,
        request.base.clone(),
        model,
        provider,
        request.thinking.clone(),
        request.session.clone(),
        request.skill.clone(),
        request.append_system_prompt.clone(),
        request.tools.clone(),
        request.exclude_tools.clone(),
    );
    state.repo_root = fleet
        .repo_root
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    state.is_git = fleet.is_git;
    state.base_commit = base_commit;
    // The recorded identity is the one the directory and branch were cut
    // from; ownership is the acting session's — the fleet's last-used
    // session, or the default session when the fleet has no session rows
    // yet — so a session's cap and views count exactly the runs it owns.
    state.uuid = uuid;
    state.orchestrator_id = Some(super::acting_session(fleet.paths.root()));
    crate::fleet::run::save_state(&run_dir, &state)?;
    Ok(CreatedRun {
        run_id,
        run_dir,
        paths: fleet.paths,
        state,
        worktree_path,
    })
}

/// The non-archived runs sharing `name` — by exact id, by the legacy
/// `<name>-<14-digit stamp>` id form, or by the `name` field (the alias) —
/// the same resolution set [`run::find_run`] picks from — newest first.
/// Unreadable `run.json` files are skipped, like everywhere else.
fn live_namesakes(fleet_dir: &Path, name: &str) -> Vec<RunRef> {
    let key = sanitize_name(name);
    let Ok(of_name) = regex::Regex::new(&format!("^{}-\\d{{14}}$", regex::escape(&key))) else {
        return Vec::new();
    };
    run::list_runs(fleet_dir)
        .into_iter()
        .filter(|r| {
            r.run_id == key
                || of_name.is_match(&r.run_id)
                || run::load_state(&r.run_dir)
                    .map(|state| state.name == key)
                    .unwrap_or(false)
        })
        .filter_map(|r| {
            run::load_state(&r.run_dir).ok().map(|state| RunRef {
                run_id: r.run_id,
                run_dir: r.run_dir,
                state,
            })
        })
        .filter(|r| r.state.status != RunStatus::Archived)
        .collect()
}

/// Launch `pilotfish monitor` for the run, detached: its own process group
/// (`process_group(0)` — the safe equivalent of Node's `detached: true`),
/// stdio into the run's `pi.log`. The child outlives this process, and it is
/// reaped by a background task: nobody waits on the handle, and an unreaped
/// child would linger as a zombie whose pid keeps answering `kill(pid, 0)` —
/// so a crashed monitor could never read as dead. Returns the monitor's pid.
fn launch_monitor(paths: &FleetPaths, run_id: &str) -> std::io::Result<u32> {
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.pi_log(run_id))?;
    let errors = log.try_clone()?;
    let fleet_dir = paths.root().to_string_lossy().into_owned();
    let mut command = tokio::process::Command::new(exe);
    command
        .args(["monitor", "--fleet-dir", &fleet_dir, "--run", run_id])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors))
        .process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();
    tokio::spawn(async move {
        // Reap whenever the monitor exits; the runtime outlives the caller,
        // so the pid is gone from the process table while we still run.
        let _ = child.wait().await;
    });
    pid.ok_or_else(|| std::io::Error::other("monitor exited before its pid could be read"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::{RunStatus, save_state};
    use crate::git::test_support::{git_sync, tmp_dir};
    use crate::ops::resolve_fleet_dir_with_env;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// `create_run` drives several real git spawns, and under this machine's
    /// parallel-suite load a fresh repo has been observed to read as "not a
    /// git repository" to a mid-test spawn that earlier spawns had just used
    /// fine. Each attempt is self-contained — a new run id every second, so
    /// no collision with a failed attempt's leftovers — so poll `Err` and
    /// return the first `Ok`; a persistent failure surfaces as the last Err.
    async fn create_run_with_retry(
        request: &SpawnRequest,
        user_config_dir: Option<&Path>,
    ) -> anyhow::Result<CreatedRun> {
        // Operation-level, so a longer bound than the per-spawn helper's.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match create_run_with_env(request, None, user_config_dir).await {
                Ok(created) => return Ok(created),
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn init_repo(name: &str) -> PathBuf {
        let root = tmp_dir(name);
        git_sync(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git_sync(&root, &["add", "."]);
        git_sync(&root, &["commit", "-qm", "seed"]);
        root
    }

    fn request(name: &str, brief: &str, cwd: &Path, worktree: bool) -> SpawnRequest {
        SpawnRequest {
            name: name.into(),
            brief: brief.into(),
            cwd: Some(cwd.to_path_buf()),
            model: None,
            provider: None,
            thinking: None,
            worktree,
            base: None,
            skill: None,
            append_system_prompt: None,
            session: None,
            tools: None,
            exclude_tools: None,
            route: None,
        }
    }

    #[tokio::test]
    async fn routing_off_is_the_spawn_that_was_always_there() {
        let root = init_repo("pilotfish-route-off-");
        let created = create_run_with_retry(&request("plain", "do a thing", &root, true), None)
            .await
            .unwrap();
        assert_eq!(created.state.routing, None);
        let raw = std::fs::read_to_string(created.paths.run_json(&created.run_id)).unwrap();
        assert!(
            !raw.contains("\"routing\""),
            "an unrouted run.json is byte-identical to before: {raw}"
        );
    }

    #[tokio::test]
    async fn a_pinned_model_and_thinking_level_are_never_routed() {
        let root = init_repo("pilotfish-route-pinned-");
        let fleet_dir = root.join(crate::paths::STATE_DIR_NAME);
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let mut req = request("pinned", "do a thing", &root, true);
        req.model = Some("claude-opus-5".into());
        req.thinking = Some("high".into());
        // `--route` on, and an endpoint that would refuse: nothing is asked,
        // because there is nothing left to decide
        let mut with_route = req.clone();
        with_route.route = Some(true);
        let config = crate::paths::UserConfig {
            routing: crate::paths::RoutingConfig {
                enabled: true,
                endpoint: Some("http://127.0.0.1:1/v1/systemone".into()),
                ..crate::paths::RoutingConfig::default()
            },
            ..crate::paths::UserConfig::default()
        };
        let decided = route_request(&mut with_route, &config, &fleet_dir, &[], 1_000).await;
        assert_eq!(decided, None, "nothing to route");
        assert_eq!(with_route.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(with_route.thinking.as_deref(), Some("high"));
    }

    /// A repository whose fleet dir holds pi's catalogue, as a worker's boot
    /// would have left it: two models with different reasoning levels.
    fn fleet_with_catalogue(name: &str) -> (PathBuf, PathBuf) {
        use crate::fleet::run::{ModelCost, PiCache, WorkerModel};
        let root = init_repo(name);
        let fleet_dir = root.join(crate::paths::STATE_DIR_NAME);
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let levels = |l: &[&str]| l.iter().map(ToString::to_string).collect::<Vec<_>>();
        let opus = WorkerModel {
            thinking_levels: levels(&["off", "high", "max"]),
            cost: Some(ModelCost {
                input: 5.0,
                output: 25.0,
            }),
            ..WorkerModel::new("anthropic", "claude-opus-5")
        };
        let flash = WorkerModel {
            thinking_levels: levels(&["off", "high", "xhigh"]),
            cost: Some(ModelCost {
                input: 0.5,
                output: 2.0,
            }),
            ..WorkerModel::new("opencode-go", "deepseek-v4-flash")
        };
        run::write_pi_cache(
            &fleet_dir,
            &PiCache {
                available_models: vec![opus, flash],
                ..PiCache::default()
            },
        )
        .unwrap();
        (root, fleet_dir)
    }

    fn routing_on(endpoint: String) -> crate::paths::UserConfig {
        crate::paths::UserConfig {
            routing: crate::paths::RoutingConfig {
                enabled: true,
                endpoint: Some(endpoint),
                ..crate::paths::RoutingConfig::default()
            },
            worker: crate::paths::WorkerConfig {
                model: Some("claude-opus-5".into()),
                provider: None,
            },
            ..crate::paths::UserConfig::default()
        }
    }

    fn unsure() -> serde_json::Value {
        serde_json::json!({"answers": {
            "model": {"choice": "anthropic:claude-opus-5", "confidence": 0.2},
            "needs_worktree": {"noul": 0.9},
        }})
    }

    fn thinking(level: &str) -> serde_json::Value {
        serde_json::json!({"answers": {"thinking": {"choice": level, "confidence": 0.7}}})
    }

    /// A console holding the fleet: the lock a live console keeps fresh.
    fn open_console(fleet_dir: &Path) {
        std::fs::write(
            fleet_dir.join("console.lock"),
            serde_json::json!({"pid": std::process::id(), "ts": crate::util::now_iso()})
                .to_string(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_confident_route_sets_the_model_and_then_its_thinking() {
        let (root, fleet_dir) = fleet_with_catalogue("pilotfish-route-sure-");
        let (url, _requests) = crate::route::test_support::stub(vec![
            serde_json::json!({"answers": {
                "model": {"choice": "opencode-go:deepseek-v4-flash", "confidence": 0.9},
            }}),
            thinking("xhigh"),
        ])
        .await;
        let config = routing_on(url);
        let mut req = request("sure", "do a thing", &root, true);
        let routing = route_with_key(
            &mut req,
            &config,
            &config.routing,
            &fleet_dir,
            &[],
            "k",
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(req.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(req.provider.as_deref(), Some("opencode-go"));
        assert_eq!(req.thinking.as_deref(), Some("xhigh"));
        assert!(routing.note.contains("thinking xhigh"), "{}", routing.note);
    }

    #[tokio::test]
    async fn an_unsure_model_is_put_to_the_console_and_the_answer_runs() {
        let (root, fleet_dir) = fleet_with_catalogue("pilotfish-route-ask-");
        open_console(&fleet_dir);
        let (url, _requests) =
            crate::route::test_support::stub(vec![unsure(), thinking("xhigh")]).await;
        let config = routing_on(url);
        let paths = FleetPaths::new(&fleet_dir);
        let console = {
            let paths = paths.clone();
            tokio::spawn(async move {
                loop {
                    if let Some(question) = crate::route::pending_questions(&paths).first() {
                        crate::route::answer_question(
                            &paths,
                            &question.id,
                            &crate::route::ModelAnswer {
                                model: Some("opencode-go:deepseek-v4-flash".into()),
                            },
                        )
                        .unwrap();
                        return question.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };
        let mut req = request("ask", "do a thing\nin detail", &root, true);
        let routing = route_with_key(
            &mut req,
            &config,
            &config.routing,
            &fleet_dir,
            &[],
            "k",
            10_000,
        )
        .await
        .unwrap();
        let asked = console.await.unwrap();
        assert_eq!(asked.name, "ask");
        assert_eq!(asked.brief, "do a thing");
        assert_eq!(asked.fallback.as_deref(), Some("claude-opus-5"));
        assert_eq!(
            asked.options[0].key, "anthropic:claude-opus-5",
            "jev's leaning first"
        );
        assert_eq!(req.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(req.provider.as_deref(), Some("opencode-go"));
        assert_eq!(
            req.thinking.as_deref(),
            Some("xhigh"),
            "the level is chosen for the model the human picked"
        );
        assert!(
            routing
                .note
                .contains("you chose opencode-go:deepseek-v4-flash"),
            "{}",
            routing.note
        );
        assert_eq!(
            std::fs::read_dir(paths.routing_dir()).unwrap().count(),
            0,
            "the question and its answer are taken down"
        );
    }

    #[tokio::test]
    async fn an_unanswered_model_question_keeps_the_configured_model() {
        let (root, fleet_dir) = fleet_with_catalogue("pilotfish-route-wait-");
        open_console(&fleet_dir);
        let (url, _requests) =
            crate::route::test_support::stub(vec![unsure(), thinking("max")]).await;
        let config = routing_on(url);
        let mut req = request("wait", "do a thing", &root, true);
        let routing = route_with_key(
            &mut req,
            &config,
            &config.routing,
            &fleet_dir,
            &[],
            "k",
            300,
        )
        .await
        .unwrap();
        assert_eq!(req.model, None, "the configured model stands");
        assert_eq!(
            req.thinking.as_deref(),
            Some("max"),
            "and its own levels are chosen from"
        );
        assert!(
            routing
                .note
                .contains("no answer within 1 s; kept the configured claude-opus-5"),
            "{}",
            routing.note
        );
        let paths = FleetPaths::new(&fleet_dir);
        assert!(crate::route::pending_questions(&paths).is_empty());
    }

    #[tokio::test]
    async fn with_no_console_open_nobody_is_waited_for() {
        let (root, fleet_dir) = fleet_with_catalogue("pilotfish-route-alone-");
        let (url, _requests) =
            crate::route::test_support::stub(vec![unsure(), thinking("high")]).await;
        let config = routing_on(url);
        let mut req = request("alone", "do a thing", &root, true);
        let started = Instant::now();
        let routing = route_with_key(
            &mut req,
            &config,
            &config.routing,
            &fleet_dir,
            &[],
            "k",
            60_000,
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no ten-minute wait"
        );
        assert_eq!(req.model, None);
        assert!(routing.note.contains("no console open"), "{}", routing.note);
    }

    #[tokio::test]
    async fn create_run_builds_layout_worktree_and_initial_state() {
        let root = init_repo("pilotfish-spawn-");
        let created =
            create_run_with_retry(&request("auth-worker", "create hello", &root, true), None)
                .await
                .unwrap();
        assert!(
            regex::Regex::new(r"^auth-worker-[0-9a-f]{7}$")
                .unwrap()
                .is_match(&created.run_id)
        );
        // The id is the alias plus the short uuid, and the state records the
        // same uuid plus the owning session (the default until fleet.json
        // names one — a fresh fleet has no session rows yet).
        assert!(!created.state.uuid.is_nil());
        assert_eq!(
            created.run_id,
            format!(
                "auth-worker-{}",
                crate::util::short_uuid(&created.state.uuid)
            )
        );
        assert_eq!(
            created.state.orchestrator_id,
            Some(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION)
        );
        assert!(created.paths.run_json(&created.run_id).is_file());
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gitignore.contains(".pilotfish/"), "{gitignore}");

        let worktree = created.worktree_path.clone().unwrap();
        assert!(worktree.join("seed.txt").exists());
        assert!(worktree.starts_with(created.paths.root().join("worktrees")));
        assert!(
            created
                .state
                .branch
                .clone()
                .unwrap()
                .starts_with("pilotfish/auth-worker-")
        );
        assert_eq!(
            created.state.repo_root.as_deref(),
            Some(root.canonicalize().unwrap().to_string_lossy().as_ref())
        );
        assert!(created.state.is_git);
        let head = git::resolve_commit(&root, "HEAD").await.unwrap();
        assert_eq!(created.state.base_commit.as_deref(), Some(head.as_str()));
        assert_eq!(created.state.task_brief, "create hello");
        assert_eq!(created.state.status, crate::fleet::run::RunStatus::Starting);
        assert_eq!(
            created.state.cwd,
            root.canonicalize().unwrap().to_string_lossy()
        );
    }

    #[tokio::test]
    async fn create_run_in_a_plain_directory_runs_in_place() {
        let dir = tmp_dir("pilotfish-spawn-plain-");
        let created = create_run_with_retry(&request("flat", "b", &dir, true), None)
            .await
            .unwrap();
        assert_eq!(created.worktree_path, None);
        assert_eq!(created.state.branch, None);
        assert!(!created.state.is_git);
        assert_eq!(created.state.repo_root, None);
        assert_eq!(created.state.base_commit, None);
    }

    #[tokio::test]
    async fn create_run_skips_the_worktree_when_asked() {
        let root = init_repo("pilotfish-spawn-");
        let created = create_run_with_retry(&request("nowt", "b", &root, false), None)
            .await
            .unwrap();
        assert_eq!(created.worktree_path, None);
        assert_eq!(created.state.branch, None);
        assert!(
            created.state.is_git,
            "still a git repo, just running in place"
        );
    }

    #[tokio::test]
    async fn an_exited_monitor_is_reaped_so_a_crash_can_read_dead() {
        let dir = tmp_dir("pilotfish-spawn-reap-");
        let paths = FleetPaths::new(dir);
        let run_id = "reap-20260828141530";
        std::fs::create_dir_all(paths.run_dir(run_id)).unwrap();
        // `launch_monitor` always runs `current_exe monitor …`; from the test
        // harness that is this test binary itself, which rejects the unknown
        // `--fleet-dir`/`--run` flags and exits at once — a short-lived
        // stand-in for a monitor that crashes. This process stays alive
        // throughout, so reaping must come from the background task.
        let pid = launch_monitor(&paths, run_id).unwrap();
        let pid = i32::try_from(pid).unwrap();
        // A dropped (unreaped) child stays a zombie, and a zombie answers
        // `kill(pid, 0)` — exactly what `fleet::run::is_alive` checks. Poll
        // until the pid is truly gone from the process table.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while crate::fleet::run::is_alive(Some(pid)) {
            assert!(
                std::time::Instant::now() < deadline,
                "monitor pid {pid} still answers kill(pid, 0) — it was not reaped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// A hand-written prior run on disk, the way an earlier spawn left it:
    /// no worktree, a fixed id so it never collides with a live stamp.
    fn put_run(
        paths: &FleetPaths,
        name: &str,
        run_id: &str,
        status: RunStatus,
        pid: Option<i32>,
    ) -> PathBuf {
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
        state.pid = pid;
        save_state(&run_dir, &state).unwrap();
        run_dir
    }

    #[tokio::test]
    async fn spawn_archives_a_stale_namesake() {
        let root = init_repo("pilotfish-spawn-dupe-");
        let fleet = resolve_fleet_dir_with_env(Some(&root), None).await.unwrap();
        fleet.paths.ensure().unwrap();
        // A settled prior run of the same name: spawning anew archives it as
        // part of the spawn, so the name keeps exactly one live entry and
        // `cleanup <name>` cannot resolve to a stale twin.
        let stale = put_run(
            &fleet.paths,
            "auth",
            "auth-20990101000000",
            RunStatus::Settled,
            None,
        );
        let created = create_run_with_retry(&request("auth", "new work", &root, true), None)
            .await
            .unwrap();
        assert_ne!(created.run_id, "auth-20990101000000", "a fresh run id");
        assert_eq!(
            crate::fleet::run::load_state(&stale).unwrap().status,
            RunStatus::Archived,
            "spawn archives the stale namesake"
        );
        assert_eq!(
            crate::fleet::run::load_state(&created.run_dir)
                .unwrap()
                .status,
            RunStatus::Starting
        );
    }

    #[tokio::test]
    async fn spawn_refuses_when_the_name_still_runs() {
        let root = init_repo("pilotfish-spawn-live-");
        let fleet = resolve_fleet_dir_with_env(Some(&root), None).await.unwrap();
        fleet.paths.ensure().unwrap();
        // A still-running namesake refuses the spawn, naming the live run:
        // silently duplicating the name is how a live worker was archived.
        let live_id = "auth-20990101000001";
        let live_dir = put_run(
            &fleet.paths,
            "auth",
            live_id,
            RunStatus::Running,
            Some(std::process::id().cast_signed()),
        );
        let err = create_run_with_env(&request("auth", "again", &root, true), None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(live_id), "{err}");
        assert!(err.contains("still running"), "{err}");
        assert!(err.contains("one live run"), "{err}");
        assert_eq!(
            crate::fleet::run::load_state(&live_dir).unwrap().status,
            RunStatus::Running,
            "the live run is untouched"
        );
    }

    #[tokio::test]
    async fn empty_names_and_briefs_are_refused() {
        let dir = tmp_dir("pilotfish-spawn-bad-");
        let err = create_run_with_env(&request("!!!", "b", &dir, false), None, None)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "spawn: <name> required");
        let err = spawn_core(request("x", "  ", &dir, false))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "spawn: task brief required after \"--\"");
    }

    #[tokio::test]
    async fn the_user_config_supplies_worker_model_and_provider_defaults() {
        let root = init_repo("pilotfish-spawn-cfg-");
        let user_dir = tmp_dir("pilotfish-user-cfg-");
        std::fs::write(
            user_dir.join("config.toml"),
            "[worker]\nmodel = \"deepseek-v4-flash\"\nprovider = \"opencode-go\"\n",
        )
        .unwrap();
        let created = create_run_with_retry(
            &request("cfg-worker", "do the thing", &root, true),
            Some(&user_dir),
        )
        .await
        .unwrap();
        // The run records the resolved defaults, which the monitor hands to pi.
        assert_eq!(created.state.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(created.state.provider.as_deref(), Some("opencode-go"));
    }

    #[tokio::test]
    async fn an_explicit_worker_model_beats_the_user_config() {
        let root = init_repo("pilotfish-spawn-cfg-explicit-");
        let user_dir = tmp_dir("pilotfish-user-cfg-explicit-");
        std::fs::write(
            user_dir.join("config.toml"),
            "[worker]\nmodel = \"deepseek-v4-flash\"\nprovider = \"opencode-go\"\n",
        )
        .unwrap();
        let mut req = request("cfg-explicit", "do the thing", &root, true);
        req.model = Some("glm-5.3".into());
        let created = create_run_with_retry(&req, Some(&user_dir)).await.unwrap();
        assert_eq!(created.state.model.as_deref(), Some("glm-5.3"));
        // The provider carries no explicit value, so the config still fills it.
        assert_eq!(created.state.provider.as_deref(), Some("opencode-go"));
    }

    /// A live run on disk owned by `owner` (`None` = the unowned legacy
    /// shape), the way an earlier spawn of that session left it.
    fn put_live_run(paths: &FleetPaths, run_id: &str, name: &str, owner: Option<uuid::Uuid>) {
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
        state.status = RunStatus::Running;
        state.pid = Some(std::process::id().cast_signed());
        state.orchestrator_id = owner;
        save_state(&run_dir, &state).unwrap();
    }

    /// The fleet dir of a repo, ensured like spawn ensures it.
    fn fleet_of(root: &Path) -> PathBuf {
        let dir = root.join(crate::paths::STATE_DIR_NAME);
        std::fs::create_dir_all(dir.join("runs")).unwrap();
        dir
    }

    #[tokio::test]
    async fn spawn_refuses_at_the_per_session_cap_naming_the_slots() {
        let root = init_repo("pilotfish-spawn-cap-");
        let fleet = fleet_of(&root);
        let default = Some(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION);
        // Three live runs of the (default) session fill the default cap of 3.
        for i in 0..3 {
            put_live_run(
                &FleetPaths::new(&fleet),
                &format!("cap{i}-6082814153{i}"),
                &format!("cap{i}"),
                default,
            );
        }
        let before = std::fs::read_dir(fleet.join("runs"))
            .unwrap()
            .flatten()
            .count();
        let worktrees = fleet.join("worktrees");
        let worktrees_before = std::fs::read_dir(&worktrees)
            .map(Iterator::count)
            .unwrap_or(0);

        let result = spawn_core_with_dirs(request("fourth", "b", &root, true), None, None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Error, "refused with exit 1");
        let err = result.err.join("\n");
        assert!(err.contains("refused"), "{err}");
        assert!(err.contains("cap is 3"), "{err}");
        assert!(err.contains("max_workers_per_session"), "{err}");
        // Every holder is named, with its id and derived view.
        for i in 0..3 {
            assert!(err.contains(&format!("cap{i}-6082814153{i}")), "{err}");
        }
        // Nothing was created: no new run, no worktree.
        let after = std::fs::read_dir(fleet.join("runs"))
            .unwrap()
            .flatten()
            .count();
        assert_eq!(after, before, "no run directory appeared");
        let worktrees_after = std::fs::read_dir(&worktrees)
            .map(Iterator::count)
            .unwrap_or(0);
        assert_eq!(worktrees_before, worktrees_after, "no worktree was cut");
    }

    #[tokio::test]
    async fn the_cap_counts_live_runs_only_and_other_sessions_do_not_hold_slots() {
        let root = init_repo("pilotfish-spawn-cap2-");
        let fleet = fleet_of(&root);
        let paths = FleetPaths::new(&fleet);
        let other = Some(uuid::Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap());
        // Three live runs owned by *another* session: none count toward this
        // (default) session's cap — the whole point of a per-session limit.
        for i in 0..3 {
            put_live_run(
                &paths,
                &format!("theirs{i}-6082814153{i}"),
                &format!("theirs{i}"),
                other,
            );
        }
        let result = spawn_core_with_dirs(request("mine", "b", &root, true), None, None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Ok, "{:?}", result.err);

        // Settled runs free their slot: two live, one settled — a third
        // live would refuse, the settled one never holds a slot.
        let mut settled = RunState::new(
            paths.root().to_string_lossy().as_ref(),
            "old-60828141530",
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
        settled.status = RunStatus::Settled;
        settled.orchestrator_id = Some(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION);
        let old_dir = paths.run_dir("old-60828141530");
        std::fs::create_dir_all(&old_dir).unwrap();
        save_state(&old_dir, &settled).unwrap();
        put_live_run(&paths, "a-60828141531", "a", None);
        put_live_run(&paths, "b-60828141532", "b", None);
        put_live_run(
            &paths,
            "c-60828141533",
            "c",
            Some(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION),
        );
        let result = spawn_core_with_dirs(request("again", "b", &root, true), None, None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Error);
        let err = result.err.join("\n");
        assert!(err.contains("cap is 3"), "{err}");
        assert!(err.contains("a-60828141531"), "{err}");
        assert!(err.contains("b-60828141532"), "{err}");
        assert!(err.contains("c-60828141533"), "{err}");
        assert!(!err.contains("old-"), "a settled run holds no slot: {err}");
    }

    #[tokio::test]
    async fn the_user_config_sets_the_cap_and_zero_refuses_everything() {
        let root = init_repo("pilotfish-spawn-capcfg-");
        let fleet = fleet_of(&root);
        let paths = FleetPaths::new(&fleet);
        let user_dir = tmp_dir("pilotfish-user-cap-");
        // A lowered cap: two live runs fill it.
        std::fs::write(
            user_dir.join("config.toml"),
            "[limits]\nmax_workers_per_session = 2\n",
        )
        .unwrap();
        put_live_run(&paths, "x-60828141530", "x", None);
        put_live_run(&paths, "y-60828141531", "y", None);
        let result = spawn_core_with_dirs(request("z", "b", &root, true), None, Some(&user_dir))
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::Error);
        assert!(
            result.err.join("\n").contains("cap is 2"),
            "{:?}",
            result.err
        );

        // A cap of zero refuses even with nothing running: no spawning at
        // all, with a hint naming the config key.
        let empty = init_repo("pilotfish-spawn-capzero-");
        std::fs::write(
            user_dir.join("config.toml"),
            "[limits]\nmax_workers_per_session = 0\n",
        )
        .unwrap();
        let result =
            spawn_core_with_dirs(request("solo", "b", &empty, true), None, Some(&user_dir))
                .await
                .unwrap();
        assert_eq!(result.code, ExitCode::Error);
        let err = result.err.join("\n");
        assert!(err.contains("cap is 0"), "{err}");
        assert!(
            err.contains("raise [limits] max_workers_per_session"),
            "actionable hint: {err}"
        );
    }

    #[tokio::test]
    async fn a_spawn_records_the_acting_session_as_owner() {
        let root = init_repo("pilotfish-spawn-owner-");
        let fleet = fleet_of(&root);
        // The fleet's last-used session is the acting one; the new run is
        // recorded as *its*, so its cap and views count exactly its runs.
        let mut store = crate::orch::session::FleetSessions::new();
        store.upsert(crate::orch::session::OrchestratorSession::new("/repo"));
        let session = store.last_used().unwrap().uuid;
        crate::orch::session::save(&fleet, &mut store).unwrap();
        let created = create_run_with_retry(&request("owned", "b", &root, true), None)
            .await
            .unwrap();
        assert_eq!(created.state.orchestrator_id, Some(session));
        // And the session's own live runs now fill its cap: one more live
        // run blocks this session's next spawn.
        let state = run::load_state(&created.run_dir).unwrap();
        assert_eq!(state.orchestrator_id, Some(session));
    }
}
