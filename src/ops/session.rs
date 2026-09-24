//! Removing an orchestrator session outright, once its work is done: the
//! orchestrator stopped, every worker it spawned removed along with its
//! worktree and branch, its run records and transcript deleted, and its row
//! gone from `fleet.json`.
//!
//! The removal is forced all the way down, because a finished session with
//! a stuck worker is still finished: what does not stop when asked is
//! killed, and a worktree git will not remove is deleted. It discards
//! unmerged work on purpose, so the console always asks first and names
//! what will go.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Serialize;

use crate::cli::ExitCode;
use crate::fleet::envelope::{Party, append_envelope};
use crate::fleet::run::{self, RunRef};
use crate::git;
use crate::orch::health;
use crate::orch::records::OrchestratorCommand;
use crate::orch::session::{self, OrchestratorSession};
use crate::paths::{FleetPaths, SessionKey};

use super::CommandResult;
use super::integrate::{CleanupOutcome, cleanup_one};

/// How long the orchestrator's monitor gets to act on the stop request
/// before it is signalled.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// How long a signalled process gets to go before the next step.
const KILL_WAIT: Duration = Duration::from_secs(3);

/// What removing a session did, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemovedSession {
    /// The runs removed: worktree, branch and record.
    pub removed: Vec<String>,
    /// The runs that had to be forced — killed, or their worktree deleted
    /// by hand — and were removed all the same.
    pub forced: Vec<String>,
    /// Runs whose record could not be deleted even so; everything else of
    /// theirs is gone.
    pub left: Vec<String>,
}

/// Remove the session `key`: stop its orchestrator, remove every run it
/// owns — a live one aborted, its worktree discarded and its branch deleted
/// whether merged or not — then delete its directory and its row.
///
/// Forced throughout. An orchestrator or worker that will not stop when
/// asked has its process group killed (only after checking the pid still
/// runs *that* monitor, never a recycled one), and a worktree `git` will
/// not remove is deleted and pruned. What still could not be deleted is
/// named in the result, whose code is then an error.
///
/// # Errors
///
/// When the session's row cannot be removed from `fleet.json`.
pub async fn remove_session(
    fleet_dir: &Path,
    key: &SessionKey,
) -> anyhow::Result<CommandResult<RemovedSession>> {
    let paths = FleetPaths::new(fleet_dir);
    let label = key.alias.clone().unwrap_or_else(|| key.dir_name());
    let mut data = RemovedSession::default();
    let mut err: Vec<String> = Vec::new();

    if !stop_orchestrator(&paths, key).await {
        // deleting its directory below stops a monitor on its next poll
        err.push(format!(
            "session remove: {label}'s orchestrator did not stop even when killed; \
removing its directory stops it"
        ));
    }

    for summary in run::list_runs_for_owner(fleet_dir, key.uuid) {
        let Ok(state) = run::load_state(&summary.run_dir) else {
            continue;
        };
        let target = RunRef {
            run_id: summary.run_id.clone(),
            run_dir: summary.run_dir.clone(),
            state,
        };
        match cleanup_one(&target, true, false).await {
            CleanupOutcome::AlreadyArchived | CleanupOutcome::Archived { .. } => {}
            CleanupOutcome::Skipped { note }
            | CleanupOutcome::Refused { note }
            | CleanupOutcome::Failed { note } => {
                err.push(format!(
                    "session remove: forcing {} — {note}",
                    target.run_id
                ));
                force_remove_run(&target).await;
                data.forced.push(target.run_id.clone());
            }
        }
        match remove_dir_retrying(&summary.run_dir).await {
            Ok(()) => data.removed.push(summary.run_id),
            Err(e) => {
                err.push(format!(
                    "session remove: {}'s record could not be deleted: {e}",
                    summary.run_id
                ));
                data.left.push(summary.run_id);
            }
        }
    }

    if let Err(e) = remove_dir_retrying(&paths.orchestrator_dir(key)).await {
        err.push(format!(
            "session remove: {label}'s transcript could not be deleted: {e}"
        ));
        data.left.push(key.dir_name());
    }
    session::with_store_mutation(fleet_dir, |store| {
        store.sessions.remove(&key.uuid);
    })
    .with_context(|| format!("session remove: removing {label} from fleet.json"))?;
    let workers = data.removed.len();
    Ok(CommandResult {
        code: if data.left.is_empty() {
            ExitCode::Ok
        } else {
            ExitCode::Error
        },
        out: vec![format!(
            "removed session {label} and its {workers} worker{}, with their worktrees and branches",
            if workers == 1 { "" } else { "s" }
        )],
        err,
        data,
    })
}

/// Stop the session's monitor, escalating: ask, then SIGTERM through the
/// orphan reaper, then SIGKILL the monitor's process group (claude and the
/// MCP server with it). Every signal is sent only while the pid still runs
/// this session's monitor. `true` once no monitor is left.
async fn stop_orchestrator(paths: &FleetPaths, key: &SessionKey) -> bool {
    let row = session::load(paths.root()).and_then(|store| store.sessions.get(&key.uuid).cloned());
    let Some(row) = row else {
        return true;
    };
    if !monitor_alive(&row) {
        return true;
    }
    let _ = append_envelope(
        &paths.orchestrator_inbox(key),
        &OrchestratorCommand::Stop.to_envelope(Party::Console),
    );
    if wait_for_exit(&row, STOP_WAIT).await {
        return true;
    }
    let matcher = health::orphan_matcher_for(key);
    let _ = health::reap_orphan_orchestrator(row.pid, &matcher, row.pid_started_at);
    if wait_for_exit(&row, KILL_WAIT).await {
        return true;
    }
    if let Some(pid) = row.pid
        && runs_marked(pid, &matcher)
    {
        kill_group(pid);
    }
    wait_for_exit(&row, KILL_WAIT).await
}

/// Force out a run that would not go cleanly: kill its monitor's process
/// group (pi with it) if it still runs, then take the worktree and the
/// branch by hand — `--force` twice removes even a locked worktree, and
/// what `git` still refuses is deleted and pruned.
async fn force_remove_run(target: &RunRef) {
    if let Some(pid) = target.state.pid
        && runs_marked(pid, &format!("--run {}", target.run_id))
    {
        kill_group(pid);
        let start = std::time::Instant::now();
        while run::is_alive(Some(pid)) && start.elapsed() < KILL_WAIT {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let (Some(worktree), Some(repo_root)) = (&target.state.worktree, &target.state.repo_root)
    else {
        return;
    };
    let repo = Path::new(repo_root);
    let tree = Path::new(worktree);
    if tree.exists() {
        let _ = git::git_raw(
            &["worktree", "remove", "--force", "--force", worktree],
            repo,
        )
        .await;
    }
    if tree.exists() {
        let _ = std::fs::remove_dir_all(tree);
    }
    let _ = git::git_raw(&["worktree", "prune"], repo).await;
    if let Some(branch) = &target.state.branch {
        git::delete_branch(repo, branch, true).await;
    }
}

/// Whether `pid` is alive and its command line carries `marker` — the
/// guard that keeps a kill off a pid recycled onto some other process.
fn runs_marked(pid: i32, marker: &str) -> bool {
    run::is_alive(Some(pid)) && health::command_line_of(pid).is_some_and(|cmd| cmd.contains(marker))
}

/// SIGKILL a monitor's whole process group: monitors are spawned as group
/// leaders, so their children (pi, or claude and its MCP server) go too.
fn kill_group(pid: i32) {
    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
}

/// Whether the row's monitor is running: its pid alive, and not a pid
/// recycled onto a process that started after the monitor did.
fn monitor_alive(row: &OrchestratorSession) -> bool {
    let Some(pid) = row.pid else {
        return false;
    };
    if !run::is_alive(Some(pid)) {
        return false;
    }
    match (row.pid_started_at, health::process_started_at(pid)) {
        (Some(recorded), Some(current)) => current <= recorded,
        _ => true,
    }
}

async fn wait_for_exit(row: &OrchestratorSession, within: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < within {
        if !monitor_alive(row) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !monitor_alive(row)
}

/// `remove_dir_all`, retried: a monitor that has just stopped may still be
/// closing a file inside, and a removal that meets it half-way fails with
/// `ENOTEMPTY`.
async fn remove_dir_retrying(dir: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    for _ in 0..5 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = Err(e),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::{RunState, RunStatus};
    use crate::git::test_support::{git_sync, tmp_dir};
    use std::path::PathBuf;

    fn init_repo(name: &str) -> PathBuf {
        let root = tmp_dir(name);
        git_sync(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git_sync(&root, &["add", "."]);
        git_sync(&root, &["commit", "-qm", "seed"]);
        root
    }

    /// A finished worker owned by `owner`: its own worktree on its own
    /// branch, carrying a commit the checkout never merged.
    fn finished_worker(
        root: &Path,
        fleet_dir: &Path,
        name: &str,
        owner: uuid::Uuid,
    ) -> (String, PathBuf, String) {
        let run_id = format!("{name}-1234567");
        let branch = format!("pilotfish/{run_id}");
        let worktree = fleet_dir.join("worktrees").join(&run_id);
        git_sync(
            root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                &worktree.to_string_lossy(),
            ],
        );
        std::fs::write(worktree.join(format!("{name}.txt")), "work\n").unwrap();
        git_sync(&worktree, &["add", "."]);
        git_sync(&worktree, &["commit", "-qm", name]);
        let run_dir = fleet_dir.join("runs").join(&run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let state = RunState {
            id: run_id.clone(),
            name: name.into(),
            orchestrator_id: Some(owner),
            status: RunStatus::Settled,
            worktree: Some(worktree.to_string_lossy().into_owned()),
            branch: Some(branch.clone()),
            repo_root: Some(root.to_string_lossy().into_owned()),
            is_git: true,
            fleet_dir: fleet_dir.to_string_lossy().into_owned(),
            ..RunState::default()
        };
        run::save_state(&run_dir, &state).unwrap();
        (run_id, worktree, branch)
    }

    fn branch_exists(root: &Path, branch: &str) -> bool {
        std::process::Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    }

    #[tokio::test]
    async fn remove_session_scoped() {
        let root = init_repo("pilotfish-session-rm-");
        let fleet_dir = root.join(crate::paths::STATE_DIR_NAME);
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let done = session::create_session(&fleet_dir, Some("done")).unwrap();
        let other = session::create_session(&fleet_dir, Some("other")).unwrap();
        let paths = FleetPaths::new(&fleet_dir);
        std::fs::create_dir_all(paths.orchestrator_dir(&done.key())).unwrap();
        std::fs::write(paths.orchestrator_events(&done.key()), "{}\n").unwrap();
        let (gone, gone_tree, gone_branch) =
            finished_worker(&root, &fleet_dir, "add-auth", done.uuid);
        let (kept, kept_tree, kept_branch) =
            finished_worker(&root, &fleet_dir, "keep-me", other.uuid);

        let result = remove_session(&fleet_dir, &done.key()).await.unwrap();
        assert_eq!(result.code, ExitCode::Ok, "{:?}", result.err);
        assert_eq!(result.data.removed, vec![gone.clone()]);
        assert!(!gone_tree.exists(), "the worktree is gone");
        assert!(
            !branch_exists(&root, &gone_branch),
            "unmerged, and deleted anyway: the session is done"
        );
        assert!(!fleet_dir.join("runs").join(&gone).exists());
        assert!(!paths.orchestrator_dir(&done.key()).exists());
        let store = session::load(&fleet_dir).unwrap();
        assert!(!store.sessions.contains_key(&done.uuid), "its row too");

        // another session's worker is not this session's to remove
        assert!(store.sessions.contains_key(&other.uuid));
        assert!(kept_tree.exists());
        assert!(branch_exists(&root, &kept_branch));
        assert!(fleet_dir.join("runs").join(&kept).exists());
    }

    #[tokio::test]
    async fn force_remove_locked_worktree() {
        let root = init_repo("pilotfish-session-force-");
        let fleet_dir = root.join(crate::paths::STATE_DIR_NAME);
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let done = session::create_session(&fleet_dir, Some("stuck")).unwrap();
        let (run_id, tree, branch) = finished_worker(&root, &fleet_dir, "locked", done.uuid);
        // a locked worktree: `git worktree remove --force` refuses it
        git_sync(&root, &["worktree", "lock", &tree.to_string_lossy()]);

        let result = remove_session(&fleet_dir, &done.key()).await.unwrap();
        assert_eq!(result.code, ExitCode::Ok, "{:?}", result.err);
        assert_eq!(result.data.forced, vec![run_id.clone()]);
        assert_eq!(result.data.removed, vec![run_id.clone()]);
        assert!(!tree.exists());
        assert!(!branch_exists(&root, &branch));
        assert!(!fleet_dir.join("runs").join(&run_id).exists());
        assert!(
            !session::load(&fleet_dir)
                .unwrap()
                .sessions
                .contains_key(&done.uuid),
            "the session goes even though a worker had to be forced"
        );
    }

    #[tokio::test]
    async fn force_stop_worker_process() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        let root = init_repo("pilotfish-session-kill-");
        let fleet_dir = root.join(crate::paths::STATE_DIR_NAME);
        std::fs::create_dir_all(&fleet_dir).unwrap();
        let (run_id, tree, branch) =
            finished_worker(&root, &fleet_dir, "hung", uuid::Uuid::new_v4());
        // a stand-in monitor: a group leader whose command line names the run
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30", "sh", "--run", &run_id])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let waiter = std::thread::spawn(move || child.wait().unwrap());
        let mut state = run::load_state(&fleet_dir.join("runs").join(&run_id)).unwrap();
        state.status = crate::fleet::run::RunStatus::Running;
        state.pid = Some(pid);
        let target = RunRef {
            run_id: run_id.clone(),
            run_dir: fleet_dir.join("runs").join(&run_id),
            state,
        };

        force_remove_run(&target).await;
        let status = waiter.join().unwrap();
        assert_eq!(status.signal(), Some(9), "killed, not asked");
        assert!(!tree.exists());
        assert!(!branch_exists(&root, &branch));
    }

    #[test]
    fn never_kill_unmarked_pid() {
        let me = i32::try_from(std::process::id()).unwrap();
        assert!(
            !runs_marked(me, "--run definitely-not-this-process"),
            "a live pid without the run's marker is someone else's"
        );
    }
}
