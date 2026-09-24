//! The shared operation layer: every fleet action lives here once, and the
//! CLI subcommands, the MCP tools and the console all call the same code.
//! Each file owns one verb family; the signatures taking CLI-parsed values
//! are the frozen contract with `main.rs` (which later workers never edit).
//!
//! Every core returns a [`CommandResult`] and never prints; [`print_result`]
//! is the single place that turns one into stdout, stderr and an exit code.
//! The next wave's MCP server renders the same struct as tool output, so the
//! `data` stays typed and serialisable rather than pre-formatted.

pub mod integrate;
pub mod query;
pub mod session;
pub mod spawn;
pub mod steer;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::cli::ExitCode;
use crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION;
use crate::fleet::run::{self, RunSummary};
use crate::git;
use crate::paths::{FleetPaths, env_var};

/// What a command core produces: the exit code, the lines the CLI prints on
/// stdout (`out`) and stderr (`err`), and structured data for programmatic
/// callers such as the MCP server.
#[derive(Debug, Clone)]
pub struct CommandResult<T = serde_json::Value> {
    pub code: ExitCode,
    pub out: Vec<String>,
    pub err: Vec<String>,
    pub data: T,
}

/// A successful core result. Failure paths use [`fail`] or a literal when the
/// code itself carries meaning (wait timeouts, merge conflicts).
pub const fn ok<T>(data: T, out: Vec<String>) -> CommandResult<T> {
    CommandResult {
        code: ExitCode::Ok,
        out,
        err: Vec::new(),
        data,
    }
}

/// A refused core result; `data` falls back to `Default` because failures
/// carry their meaning in `err` and the exit code.
pub fn fail<T: Default>(code: ExitCode, err: Vec<String>) -> CommandResult<T> {
    CommandResult {
        code,
        out: Vec::new(),
        err,
        data: T::default(),
    }
}

/// Print a core's lines the way the CLI always has, and hand back its exit code.
pub fn print_result<T>(result: CommandResult<T>) -> ExitCode {
    for line in result.out {
        println!("{line}");
    }
    for line in result.err {
        eprintln!("{line}");
    }
    result.code
}

/// Where a fleet's state lives once `cwd` is anchored: the repo root when the
/// target is inside one, the target itself otherwise (and `PILOTFISH_DIR` wins over
/// both — see [`FleetPaths::discover`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFleet {
    /// The caller's target, symlinks resolved so non-git targets compare
    /// equal to git's real paths (e.g. macOS `/var` -> `/private/var`).
    pub target_dir: PathBuf,
    pub repo_root: Option<PathBuf>,
    pub is_git: bool,
    pub paths: FleetPaths,
}

/// Locate the fleet dir for `cwd` (default: the process's working directory).
///
/// # Errors
///
/// Fails when the target does not exist; git probe errors propagate.
pub async fn resolve_fleet_dir(cwd: Option<&Path>) -> anyhow::Result<ResolvedFleet> {
    resolve_fleet_dir_with_env(cwd, ambient_pilotfish_dir().as_deref()).await
}

/// [`resolve_fleet_dir`] with the `$PILOTFISH_DIR` value injected, mirroring
/// [`FleetPaths::discover_with_env`]: production passes the real environment
/// value; tests pass `None` so resolution can never leave the caller's own
/// directories by inheriting an ambient variable.
///
/// # Errors
///
/// Fails when the target does not exist; git probe errors propagate.
pub async fn resolve_fleet_dir_with_env(
    cwd: Option<&Path>,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<ResolvedFleet> {
    let requested = match cwd {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    if !requested.exists() {
        anyhow::bail!("--cwd does not exist: {}", requested.display());
    }
    let target_dir = std::fs::canonicalize(&requested)?;
    let is_git = git::is_git_repo(&target_dir).await;
    let root = if is_git {
        git::repo_root(&target_dir).await
    } else {
        None
    };
    let resolved_root = root.as_deref().unwrap_or(&target_dir);
    Ok(ResolvedFleet {
        repo_root: is_git.then_some(resolved_root.to_path_buf()),
        is_git,
        paths: FleetPaths::discover_with_env(resolved_root, pilotfish_dir),
        target_dir,
    })
}

/// The ambient `$PILOTFISH_DIR` value, passed into the injectable variants by the
/// production wrappers. Tests pass `None` instead, so nothing in a test run
/// resolves the environment and lands in an unrelated fleet.
pub(crate) fn ambient_pilotfish_dir() -> Option<String> {
    std::env::var(env_var("DIR")).ok()
}

/// The session an op acts as: the fleet's last-used session (the one a
/// console or orchestrator monitor most recently touched `fleet.json`), or
/// the default session when the fleet has no session rows yet — the
/// pre-session identity, so a fresh fleet and its legacy runs behave as one
/// session's. Resolved from the fleet dir the op already resolved, so the
/// ownership bookkeeping never splits from where the op writes.
pub(crate) fn acting_session(fleet_dir: &Path) -> Uuid {
    crate::orch::session::resolve_session(fleet_dir)
        .map(|session| session.uuid)
        .unwrap_or(DEFAULT_ORCHESTRATOR_SESSION)
}

/// The runs the acting session sees as its own: the runs whose `run.json`
/// records it as owner — plus, when the acting session is the default
/// (pre-session) one, the unowned legacy runs whose state predates session
/// ownership, which nothing else can claim. Newest id first.
pub(crate) fn runs_for_acting_session(fleet_dir: &Path, session: Uuid) -> Vec<RunSummary> {
    let mut runs = run::list_runs_for_owner(fleet_dir, session);
    if session == DEFAULT_ORCHESTRATOR_SESSION {
        let owned: HashSet<String> = runs.iter().map(|r| r.run_id.clone()).collect();
        for summary in run::list_runs(fleet_dir) {
            if owned.contains(&summary.run_id) {
                continue;
            }
            let unowned = run::load_state(&summary.run_dir)
                .ok()
                .is_some_and(|state| state.orchestrator_id.is_none());
            if unowned {
                runs.push(summary);
            }
        }
        runs.sort_by(|a, b| b.run_id.cmp(&a.run_id));
    }
    runs
}

/// The session's still-live runs — the derived view, not the stored status,
/// so a dead monitor frees its slot and a settled run never holds one. This
/// is the set that counts against the per-session worker cap.
pub(crate) fn live_runs_for_session(fleet_dir: &Path, session: Uuid) -> Vec<run::RunState> {
    runs_for_acting_session(fleet_dir, session)
        .into_iter()
        .filter_map(|r| run::load_state(&r.run_dir).ok())
        .filter(|state| {
            !run::derive_status(state, run::is_alive, crate::util::now_ms()).is_terminal()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{RETRY_BOUND, RETRY_INTERVAL, git_sync, tmp_dir};
    use std::time::Instant;

    #[tokio::test]
    async fn resolve_fleet_dir_cases() {
        {
            // Under full-suite parallel load the git spawn behind the resolution
            // fails transiently, and canonicalize() of the root git reports can
            // come back NotFound a moment later — the environmental loss
            // documented in src/git.rs, whose probe this retries the same way,
            // repo setup included in the retried condition. The bound is tripled
            // from the shared budget: when another suite churns the same OS temp
            // directory, the NotFound windows outlast it, and a share-sized
            // bound burned out without a surviving attempt (observed once in
            // sixteen full-suite runs). A genuinely broken resolution still
            // fails loudly via the bound, with what the last attempt saw.
            let deadline = Instant::now() + 3 * RETRY_BOUND;
            let mut last_seen = String::from("no attempt completed");
            let (sub_real, in_repo) = loop {
                assert!(
                    Instant::now() < deadline,
                    "resolve_fleet_dir never anchored at the repo root: {last_seen}"
                );
                let root = tmp_dir("pilotfish-ops-resolve-");
                git_sync(&root, &["init", "-q", "-b", "main"]);
                let sub = root.join("sub");
                std::fs::create_dir_all(&sub).unwrap();
                let (root_real, sub_real) = match (root.canonicalize(), sub.canonicalize()) {
                    (Ok(root_real), Ok(sub_real)) => (root_real, sub_real),
                    (root_real, sub_real) => {
                        last_seen =
                            format!("setup canonicalize: root={root_real:?} sub={sub_real:?}");
                        tokio::time::sleep(RETRY_INTERVAL).await;
                        continue;
                    }
                };
                match resolve_fleet_dir_with_env(Some(&sub), None).await {
                    Ok(resolved)
                        if resolved.is_git
                            && resolved
                                .repo_root
                                .as_ref()
                                .and_then(|repo| repo.canonicalize().ok())
                                .is_some_and(|real| *real == root_real)
                            && resolved.paths.root() == root_real.join(".pilotfish")
                            && resolved.target_dir == sub_real =>
                    {
                        break (sub_real, resolved);
                    }
                    Ok(resolved) => {
                        // keep what the failed attempt actually saw
                        last_seen = format!(
                            "last attempt: is_git={} repo_root={:?} repo_root.canonicalize={:?} paths.root={:?}",
                            resolved.is_git,
                            resolved.repo_root,
                            resolved
                                .repo_root
                                .as_ref()
                                .and_then(|repo| repo.canonicalize().ok()),
                            resolved.paths.root()
                        );
                    }
                    Err(err) => last_seen = format!("last attempt errored: {err:#}"),
                }
                tokio::time::sleep(RETRY_INTERVAL).await;
            };
            assert_eq!(
                in_repo.target_dir, sub_real,
                "the target stays where the caller pointed"
            );

            let plain = tmp_dir("pilotfish-ops-plain-");
            let standalone = resolve_fleet_dir_with_env(Some(&plain), None)
                .await
                .unwrap();
            assert!(!standalone.is_git);
            assert_eq!(standalone.repo_root, None);
            assert_eq!(
                standalone.paths.root(),
                plain.canonicalize().unwrap().join(".pilotfish")
            );

            let missing = resolve_fleet_dir_with_env(Some(&plain.join("nope")), None).await;
            let err = missing.unwrap_err().to_string();
            assert!(err.contains("does not exist"), "{err}");
        }
        {
            let plain = tmp_dir("pilotfish-ops-override-");
            // An injected value wins over the cwd fallback, like `$PILOTFISH_DIR` does.
            let pinned = resolve_fleet_dir_with_env(Some(&plain), Some("/elsewhere/fleet"))
                .await
                .unwrap();
            assert_eq!(pinned.paths.root(), Path::new("/elsewhere/fleet"));
            // A blank value is the variable set-but-empty: the fallback applies.
            let blank = resolve_fleet_dir_with_env(Some(&plain), Some("  "))
                .await
                .unwrap();
            assert_eq!(blank.paths.root(), blank.target_dir.join(".pilotfish"));
        }
    }

    /// A fleet dir with one run on disk, owned by `owner` (`None` = the
    /// unowned legacy shape).
    fn put_run(
        fleet: &Path,
        run_id: &str,
        name: &str,
        status: crate::fleet::run::RunStatus,
        pid: Option<i32>,
        owner: Option<Uuid>,
    ) {
        let run_dir = fleet.join("runs").join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut state = crate::fleet::run::RunState::new(
            fleet.to_string_lossy().as_ref(),
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
        state.orchestrator_id = owner;
        crate::fleet::run::save_state(&run_dir, &state).unwrap();
    }

    #[test]
    fn session_scoping() {
        {
            let fleet = tmp_dir("pilotfish-ops-owner-");
            std::fs::create_dir_all(fleet.join("runs")).unwrap();
            let default = DEFAULT_ORCHESTRATOR_SESSION;
            let other = Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap();
            put_run(
                &fleet,
                "mine-60828141530",
                "mine",
                crate::fleet::run::RunStatus::Running,
                Some(std::process::id().cast_signed()),
                Some(default),
            );
            put_run(
                &fleet,
                "legacy-60828141531",
                "legacy",
                crate::fleet::run::RunStatus::Settled,
                None,
                None,
            );
            put_run(
                &fleet,
                "theirs-60828141532",
                "theirs",
                crate::fleet::run::RunStatus::Running,
                Some(std::process::id().cast_signed()),
                Some(other),
            );
            // The default session sees its own runs plus the unowned legacy one,
            // newest id first.
            let default_runs: Vec<String> = runs_for_acting_session(&fleet, default)
                .into_iter()
                .map(|r| r.run_id)
                .collect();
            assert_eq!(default_runs, vec!["mine-60828141530", "legacy-60828141531"]);
            // Another session sees exactly its own.
            let other_runs: Vec<String> = runs_for_acting_session(&fleet, other)
                .into_iter()
                .map(|r| r.run_id)
                .collect();
            assert_eq!(other_runs, vec!["theirs-60828141532"]);
            // The unfiltered listing still sees everything.
            assert_eq!(crate::fleet::run::list_runs(&fleet).len(), 3);
            // A run in a foreign directory without run.json is not claimed.
            std::fs::create_dir_all(fleet.join("runs/garbage-60828141533")).unwrap();
            assert_eq!(runs_for_acting_session(&fleet, default).len(), 2);
        }
        {
            let fleet = tmp_dir("pilotfish-ops-session-");
            // No fleet.json: the pre-session identity.
            assert_eq!(acting_session(&fleet), DEFAULT_ORCHESTRATOR_SESSION);
            let mut store = crate::orch::session::FleetSessions::new();
            let mut older = crate::orch::session::OrchestratorSession::new("/repo");
            older.last_used_at = "2026-08-01T00:00:00.000Z".into();
            let mut newer = crate::orch::session::OrchestratorSession::new("/repo");
            newer.last_used_at = "2026-09-01T00:00:00.000Z".into();
            let newer_uuid = newer.uuid;
            store.upsert(older);
            store.upsert(newer);
            crate::orch::session::save(&fleet, &mut store).unwrap();
            assert_eq!(acting_session(&fleet), newer_uuid);
        }
        {
            let fleet = tmp_dir("pilotfish-ops-live-");
            std::fs::create_dir_all(fleet.join("runs")).unwrap();
            let default = DEFAULT_ORCHESTRATOR_SESSION;
            // Running with a live pid, settled, archived, and a stale Starting
            // (past grace, no pid — reads dead): one live slot.
            put_run(
                &fleet,
                "r-60828141530",
                "r",
                crate::fleet::run::RunStatus::Running,
                Some(std::process::id().cast_signed()),
                Some(default),
            );
            put_run(
                &fleet,
                "s-60828141531",
                "s",
                crate::fleet::run::RunStatus::Settled,
                None,
                Some(default),
            );
            put_run(
                &fleet,
                "a-60828141532",
                "a",
                crate::fleet::run::RunStatus::Archived,
                None,
                Some(default),
            );
            put_run(
                &fleet,
                "d-60828141533",
                "d",
                crate::fleet::run::RunStatus::Starting,
                None,
                Some(default),
            );
            // The stale Starting run actually reads dead: its creation stamp is
            // older than the starting grace, so no pid means the monitor died.
            let stale_dir = fleet.join("runs/d-60828141533");
            let mut stale = crate::fleet::run::load_state(&stale_dir).unwrap();
            stale.created_at = "2020-01-01T00:00:00.000Z".into();
            crate::fleet::run::save_state(&stale_dir, &stale).unwrap();
            let live: Vec<String> = live_runs_for_session(&fleet, default)
                .into_iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(live, vec!["r-60828141530"], "one live slot among the five");
        }
    }
}
