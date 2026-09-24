//! Integrating a worker's work: diff against its base, merge its branch
//! (exit 5 on conflicts), and cleanup (worktree + branch removal, archive).
//! These are the sharp edges: diff and merge only see committed work, the
//! merge lands in the run's recorded repo root wherever the CLI is invoked
//! from, and conflicts are aborted so the worker can rebase — the
//! orchestrator never edits files itself. (Ported from `diffCore`,
//! `mergeCore` and `cleanupCore`/`cleanupRuns` in the TypeScript
//! `src/commands.ts`.)

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::cli::ExitCode;
use crate::fleet::envelope::{Envelope, Party, append_envelope};
use crate::fleet::run::{self, RunRef, RunStatus};
use crate::git::{self, MergeOutcome};
use crate::util::now_ms;

use super::steer::resolve_run_with_env;
use super::{CommandResult, fail, ok, print_result, resolve_fleet_dir};

/// How long `cleanup --force` waits for an aborted run to be terminal *and*
/// have its monitor gone, so the monitor's final flush cannot race the
/// archive write.
pub const CLEANUP_ABORT_WAIT_MS: u64 = 10_000;

/// What `diff` found, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffData {
    /// False when the run has no isolated worktree (or it is gone).
    pub applicable: bool,
    pub text: String,
    /// Uncommitted paths in the worktree (invisible to diff/merge).
    pub dirty: Vec<String>,
}

/// What `merge` did, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeData {
    pub branch: String,
    /// The checkout the branch was merged into (the run's recorded repo root).
    pub into: String,
    /// `false` for `--no-commit` (staged only) and conflicts.
    pub committed: bool,
    pub conflicts: Vec<String>,
}

/// What `cleanup` archived, skipped and refused, for programmatic callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupData {
    pub archived: Vec<String>,
    /// Runs `all` left alone because they are still running: `--force` only
    /// aborts a worker it was pointed at by name, never a sweep.
    pub skipped: Vec<String>,
    /// Runs that could not be cleaned, as `<runId>: <reason>` lines so the
    /// caller sees which failed and why without the batch aborting.
    pub failed: Vec<String>,
    /// Runs that need `--force` (still running when named, or a dirty
    /// worktree) and were left untouched.
    pub refused: Vec<String>,
}

/// The worker's changes vs its base (git diff --stat, or --name-only).
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn diff(
    name: &str,
    cwd: Option<&Path>,
    name_only: bool,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(diff_core(name, cwd, name_only).await?))
}

/// Merge the settled worker's branch into the run's recorded checkout.
/// Exit 5 on conflicts.
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn merge(
    name: &str,
    cwd: Option<&Path>,
    no_commit: bool,
) -> anyhow::Result<crate::cli::ExitCode> {
    Ok(print_result(merge_core(name, cwd, no_commit).await?))
}

/// Remove a run's worktree + branch and archive it (`<name>` or `all`).
///
/// # Errors
///
/// Fails when the fleet dir cannot be resolved.
pub async fn cleanup(
    target: &str,
    cwd: Option<&Path>,
    force: bool,
) -> anyhow::Result<crate::cli::ExitCode> {
    let fleet = resolve_fleet_dir(cwd).await?;
    Ok(print_result(
        cleanup_runs(fleet.paths.root(), target, force).await?,
    ))
}

/// The diff core: committed work only. A dirty worktree warns on stderr —
/// uncommitted changes are invisible to diff and merge.
///
/// # Errors
///
/// Fails when the run cannot be resolved (empty or unknown name); git diff
/// failures come back as a `fail` result, not an error.
pub async fn diff_core(
    name: &str,
    cwd: Option<&Path>,
    name_only: bool,
) -> anyhow::Result<CommandResult<DiffData>> {
    diff_core_with_env(
        name,
        cwd,
        name_only,
        super::ambient_pilotfish_dir().as_deref(),
    )
    .await
}

/// [`diff_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`);
/// the dashboard's poller pins its own anchored fleet dir instead.
pub(crate) async fn diff_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    name_only: bool,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<DiffData>> {
    let (_paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let state = &target.state;
    let worktree = state
        .worktree
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| p.exists());
    let Some(worktree) = worktree else {
        let text = "not applicable (run has no isolated worktree)";
        return Ok(ok(
            DiffData {
                applicable: false,
                text: text.to_string(),
                dirty: Vec::new(),
            },
            vec![text.to_string()],
        ));
    };
    let base = state
        .base_commit
        .clone()
        .or_else(|| state.base.clone())
        .unwrap_or_else(|| "HEAD".to_string());
    let text = match git::diff_against_base(&worktree, &base, name_only).await {
        Ok(out) if out.is_empty() => "(no changes)".to_string(),
        Ok(text) => text,
        Err(err) => return Ok(fail(ExitCode::Error, vec![format!("{err:#}")])),
    };
    let dirty = git::dirty_files(&worktree).await;
    // Built by hand: `ok` zeroes err, and the dirty warning must survive.
    Ok(CommandResult {
        code: ExitCode::Ok,
        out: vec![text.clone()],
        err: if dirty.is_empty() {
            Vec::new()
        } else {
            vec![dirty_warning(&dirty, "diff", "merge will not include them")]
        },
        data: DiffData {
            applicable: true,
            text,
            dirty,
        },
    })
}

/// Uncommitted worker output is invisible to diff/merge and lost by
/// `cleanup --force`; the warning names the files so the loss is visible.
fn dirty_warning(files: &[String], command: &str, consequence: &str) -> String {
    format!(
        "{command}: warning — worktree has {} uncommitted change(s) (worker did not commit); {consequence}:\n{}",
        files.len(),
        files
            .iter()
            .map(|f| format!("  {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Merge the settled worker's branch into the checkout the run was spawned
/// from, wherever this was invoked from. Exit 5 on conflicts; the merge is
/// always rolled back so the checkout stays clean and the worker can rebase
/// in its own worktree — the orchestrator never edits files itself.
///
/// # Errors
///
/// Fails when the run cannot be resolved; refusals (not settled, no branch,
/// no checkout) and git failures come back as `fail` results.
pub async fn merge_core(
    name: &str,
    cwd: Option<&Path>,
    no_commit: bool,
) -> anyhow::Result<CommandResult<MergeData>> {
    merge_core_with_env(
        name,
        cwd,
        no_commit,
        super::ambient_pilotfish_dir().as_deref(),
    )
    .await
}

/// [`merge_core`] with the `$PILOTFISH_DIR` value injected (tests pass `None`).
pub(crate) async fn merge_core_with_env(
    name: &str,
    cwd: Option<&Path>,
    no_commit: bool,
    pilotfish_dir: Option<&str>,
) -> anyhow::Result<CommandResult<MergeData>> {
    let (_paths, target) = resolve_run_with_env(name, cwd, pilotfish_dir).await?;
    let state = &target.state;
    let derived = run::derive_status(state, run::is_alive, now_ms());
    if derived != RunStatus::Settled {
        return Ok(fail(
            ExitCode::Error,
            vec![format!(
                "merge: run {} is {derived} — only settled runs can be merged.",
                state.name
            )],
        ));
    }
    let Some(branch) = state.branch.clone() else {
        return Ok(fail(
            ExitCode::Error,
            vec![format!(
                "merge: run {} has no branch (spawned without a worktree) — nothing to merge.",
                state.name
            )],
        ));
    };
    if let Err(err) = git::check_branch(&branch) {
        return Ok(fail(ExitCode::Error, vec![format!("merge: {err:#}")]));
    }
    // The orchestrating checkout is the repo the run was spawned from,
    // wherever we're invoked from.
    let Some(repo_root) = state.repo_root.as_deref().map(PathBuf::from) else {
        return Ok(fail(
            ExitCode::Error,
            vec![format!(
                "merge: run {} has no git checkout to merge into (repoRoot: none).",
                state.name
            )],
        ));
    };
    if !git::is_git_repo(&repo_root).await {
        return Ok(fail(
            ExitCode::Error,
            vec![format!(
                "merge: run {} has no git checkout to merge into (repoRoot: {}).",
                state.name,
                repo_root.display()
            )],
        ));
    }
    let mut err: Vec<String> = Vec::new();
    if let Some(worktree) = state.worktree.as_deref() {
        let dirty = git::dirty_files(Path::new(worktree)).await;
        if !dirty.is_empty() {
            err.push(dirty_warning(
                &dirty,
                "merge",
                "they are not part of the branch",
            ));
        }
    }
    // A merge already in progress is the human's: `git merge` would fail and
    // the conflict listing + `--abort` would discard their resolutions.
    if git::merge_in_progress(&repo_root).await {
        return Ok(fail(
            ExitCode::Error,
            vec![format!(
                "merge: a merge is already in progress in {} (MERGE_HEAD present); \
resolve or abort it yourself first — pilotfish will not abort another merge.",
                repo_root.display()
            )],
        ));
    }
    let outcome = git::merge_branch(&repo_root, &branch, no_commit, true).await;
    Ok(merge_outcome_result(
        outcome, state, branch, &repo_root, err, no_commit,
    ))
}

/// Render a merge outcome as the core result: conflicts abort with the
/// rebase brief (exit 5), git failures report stderr, success reports the
/// merge — carrying the dirty warning built by the caller, which `ok`
/// would zero.
fn merge_outcome_result(
    outcome: MergeOutcome,
    state: &run::RunState,
    branch: String,
    repo_root: &Path,
    mut err: Vec<String>,
    no_commit: bool,
) -> CommandResult<MergeData> {
    match outcome {
        MergeOutcome::Conflicted(files) => {
            let base = state
                .base_commit
                .as_deref()
                .and_then(|c| c.get(..7).map(str::to_string))
                .unwrap_or_else(|| "the base commit".to_string());
            err.push(format!(
                "merge: conflicts in:\n{}\nThe merge was aborted; the checkout is clean. \
Have the worker rebase its branch {branch} onto the current HEAD of {} (it was cut from {base}) \
in its own worktree, resolve the conflicts there, commit, and then merge again.",
                files.join("\n"),
                repo_root.display(),
            ));
            CommandResult {
                code: crate::cli::ExitCode::MergeConflict,
                out: Vec::new(),
                err,
                data: MergeData {
                    branch,
                    into: repo_root.to_string_lossy().into_owned(),
                    committed: false,
                    conflicts: files,
                },
            }
        }
        MergeOutcome::Failed(stderr) => {
            // Hand-built like everywhere else: `fail` would zero `err`, and
            // the dirty warning built by the caller must survive.
            err.push(format!("merge: git merge failed:\n{}", stderr.trim()));
            CommandResult {
                code: ExitCode::Error,
                out: Vec::new(),
                err,
                data: MergeData::default(),
            }
        }
        outcome => {
            let staged = matches!(outcome, MergeOutcome::Staged);
            let mut out = vec![format!(
                "merged {branch} into {}{}",
                repo_root.display(),
                if staged {
                    " (staged, not committed)"
                } else {
                    ""
                }
            )];
            out.push("Run your integration checks before cleanup.".to_string());
            CommandResult {
                code: ExitCode::Ok,
                out,
                err,
                data: MergeData {
                    branch,
                    into: repo_root.to_string_lossy().into_owned(),
                    committed: !no_commit,
                    conflicts: Vec::new(),
                },
            }
        }
    }
}

/// The same cleanup, for callers that already know the fleet dir (the
/// console, the reaper). Archived by setting the status: reports and events
/// are always kept.
///
/// # Errors
///
/// Fails on an empty target or an unknown single-name target. Per-run
/// failures never abort a batch: with `all` they are collected in
/// [`CleanupData::failed`] and the remaining runs are still cleaned.
pub async fn cleanup_runs(
    fleet_dir: &Path,
    target: &str,
    force: bool,
) -> anyhow::Result<CommandResult<CleanupData>> {
    if target.trim().is_empty() {
        anyhow::bail!("cleanup: <name|all> required");
    }
    let all = target.trim() == "all";
    let targets: Vec<RunRef> = if all {
        run::list_runs(fleet_dir)
            .into_iter()
            .filter_map(|r| {
                run::load_state(&r.run_dir).ok().map(|state| RunRef {
                    run_id: r.run_id,
                    run_dir: r.run_dir,
                    state,
                })
            })
            .collect()
    } else {
        vec![run::find_run(fleet_dir, target)?]
    };

    let mut out: Vec<String> = Vec::new();
    let mut err: Vec<String> = Vec::new();
    let mut data = CleanupData::default();
    let mut failed_any = false;
    // A whole-fleet sweep is legitimate, but crossing sessions is said out
    // loud: runs owned by other orchestrator sessions are not this session's
    // to tidy silently. The acting session's own set is the fold-in view
    // (the default session inherits the unowned legacy runs).
    if all {
        let session = super::acting_session(fleet_dir);
        let mine: std::collections::HashSet<String> =
            super::runs_for_acting_session(fleet_dir, session)
                .into_iter()
                .map(|r| r.run_id)
                .collect();
        let foreign = targets
            .iter()
            .filter(|t| t.state.status != RunStatus::Archived)
            .filter(|t| !mine.contains(&t.run_id))
            .count();
        if foreign > 0 {
            out.push(format!(
                "cleanup all: note — the sweep also covers {foreign} run(s) owned by other \
orchestrator session(s)"
            ));
        }
    }
    for target in targets {
        match cleanup_one(&target, force, all).await {
            CleanupOutcome::AlreadyArchived => {
                if !all {
                    out.push(format!("{} is already archived", target.run_id));
                }
            }
            CleanupOutcome::Archived { notes } => {
                out.push(format!("archived {}", target.run_id));
                data.archived.push(target.run_id.clone());
                err.extend(notes);
            }
            CleanupOutcome::Skipped { note } => {
                err.push(format!("cleanup: skipping {} — {note}", target.run_id));
                data.skipped.push(target.run_id.clone());
            }
            CleanupOutcome::Refused { note } => {
                err.push(format!("cleanup: refusing {} — {note}", target.run_id));
                data.refused.push(target.run_id.clone());
                if !all {
                    failed_any = true;
                }
            }
            CleanupOutcome::Failed { note } => {
                err.push(format!("cleanup: failed {} — {note}", target.run_id));
                data.failed.push(format!("{}: {note}", target.run_id));
                failed_any = true;
            }
        }
    }
    Ok(CommandResult {
        code: if failed_any {
            ExitCode::Error
        } else {
            ExitCode::Ok
        },
        out,
        err,
        data,
    })
}

/// One run's verdict in a cleanup sweep; [`cleanup_runs`] turns it into
/// lines and the [`CleanupData`] lists. Per-run decisions stay here so a
/// single failure can never abort the batch — a run that cannot be cleaned
/// is reported, the rest are still cleaned.
pub(super) enum CleanupOutcome {
    /// Already archived; only a named target is told.
    AlreadyArchived,
    /// Worktree and branch are gone, the run is archived. `notes` carries
    /// stderr lines that must survive the success (a kept unmerged branch,
    /// a slow abort) — hand-built for the same reason `ok` is not used.
    Archived { notes: Vec<String> },
    /// Left alone because it is still running: a sweep never aborts a
    /// worker it was not pointed at by name, even with `--force`.
    Skipped { note: String },
    /// Cleanable only with `--force` (running when named, or a dirty
    /// worktree); the run is left untouched.
    Refused { note: String },
    /// Could not be cleaned at all (git refused, the archive write failed).
    Failed { note: String },
}

/// Clean one run, best-effort. `all` skips live workers and other failures
/// instead of aborting the sweep; a single named target fails loudly
/// instead of being silently dropped.
pub(super) async fn cleanup_one(target: &RunRef, force: bool, all: bool) -> CleanupOutcome {
    if target.state.status == RunStatus::Archived {
        return CleanupOutcome::AlreadyArchived;
    }
    let derived = run::derive_status(&target.state, run::is_alive, crate::util::now_ms());
    if !derived.is_terminal() {
        if all {
            // `--force` means "discard unmerged work", never "kill live
            // workers": only an explicit name may abort a running run.
            return CleanupOutcome::Skipped {
                note: format!(
                    "run {} is still {derived} — target it by name with --force to abort and clean.",
                    target.state.name
                ),
            };
        }
        if !force {
            return CleanupOutcome::Refused {
                note: format!(
                    "run {} is {derived} — use --force to abort and clean.",
                    target.state.name
                ),
            };
        }
        let stopped = abort_and_wait(target).await;
        if !stopped {
            return CleanupOutcome::Failed {
                note: format!(
                    "run {} did not stop within {}s — not archiving a run whose monitor is still alive",
                    target.state.name,
                    CLEANUP_ABORT_WAIT_MS / 1000
                ),
            };
        }
    }
    let mut notes: Vec<String> = Vec::new();
    if let (Some(worktree), Some(repo_root)) = (
        target.state.worktree.clone(),
        target.state.repo_root.clone(),
    ) {
        // A corrupt or hand-edited run.json must never aim a worktree remove,
        // `remove_dir_all` or a branch delete at the human's checkout.
        let worktrees_dir = Path::new(&target.state.fleet_dir).join("worktrees");
        if let Err(err) =
            git::check_worktree_path(&worktrees_dir, Path::new(&repo_root), Path::new(&worktree))
        {
            return CleanupOutcome::Failed {
                note: format!("{err:#}"),
            };
        }
        if let Some(branch) = target.state.branch.as_deref()
            && let Err(err) = git::check_branch(branch)
        {
            return CleanupOutcome::Failed {
                note: format!("{err:#}"),
            };
        }
        let removed = match git::remove_worktree(
            Path::new(&repo_root),
            Path::new(&worktree),
            target.state.branch.as_deref(),
            force,
        )
        .await
        {
            Ok(removed) => removed,
            Err(err) => {
                return CleanupOutcome::Failed {
                    note: format!("worktree {worktree} could not be removed: {err:#}"),
                };
            }
        };
        if !removed.worktree_removed && Path::new(&worktree).exists() {
            return CleanupOutcome::Refused {
                note: format!(
                    "worktree {worktree} could not be removed (uncommitted changes?) — \
inspect or commit them, or use --force to discard."
                ),
            };
        }
        if let Some(branch) = &target.state.branch
            && !removed.branch_deleted
        {
            notes.push(format!(
                "cleanup: kept unmerged branch {branch} (use --force to delete it)"
            ));
        }
    }
    // Archive by setting the status; reports and events are always kept.
    let mut state = target.state.clone();
    state.status = RunStatus::Archived;
    if let Err(save_err) = run::save_state(&target.run_dir, &state) {
        return CleanupOutcome::Failed {
            note: format!("could not archive {}: {save_err}", target.run_dir.display()),
        };
    }
    CleanupOutcome::Archived { notes }
}

/// Send the abort envelope and wait (up to [`CLEANUP_ABORT_WAIT_MS`]) for
/// the run to be terminal *and* its monitor gone — the monitor's final
/// flush must not race the archive write. `true` when it stopped in time.
/// The abort's provenance is the fleet's acting session, like every other
/// orchestrator-originated steering.
async fn abort_and_wait(target: &RunRef) -> bool {
    let paths = run::fleet_paths_of(&target.state);
    append_envelope(
        &paths.run_inbox(&target.run_id),
        &Envelope::abort(
            Party::Orchestrator(super::acting_session(paths.root())),
            target.worker_party(),
        ),
    )
    .ok();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(CLEANUP_ABORT_WAIT_MS) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(state) = run::load_state(&target.run_dir) {
            let derived = run::derive_status(&state, run::is_alive, crate::util::now_ms());
            // Terminal AND monitor gone: its final flush can no longer race
            // our archive write.
            if derived.is_terminal() && !run::is_alive(state.pid) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::RunState;
    use crate::ops::resolve_fleet_dir_with_env;
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

    fn git_sync(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one committed seed file; the fleet dir is gitignored like
    /// production, so `status --porcelain` reads clean.
    fn init_repo(name: &str) -> PathBuf {
        let root = tmp_dir(name);
        git_sync(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), ".pilotfish/\n").unwrap();
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git_sync(&root, &["add", "."]);
        git_sync(&root, &["commit", "-qm", "seed"]);
        root
    }

    /// A run created the way spawn does (worktree cut by the real git
    /// helper), without booting a monitor: state on disk, base pinned.
    async fn make_run(root: &Path, name: &str, with_worktree: bool) -> (PathBuf, RunRef) {
        let fleet = resolve_fleet_dir_with_env(Some(root), None).await.unwrap();
        fleet.paths.ensure().unwrap();
        let run_id = format!("{name}-20260828141530");
        let run_dir = fleet.paths.run_dir(&run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut state = RunState::new(
            fleet.paths.root().to_string_lossy().as_ref(),
            &run_id,
            name,
            &root.to_string_lossy(),
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
        if with_worktree {
            let info = git::ensure_worktree(
                root,
                &fleet.paths.root().join("worktrees"),
                &run_id,
                name,
                None,
            )
            .await
            .unwrap();
            state.worktree = Some(info.worktree_path.to_string_lossy().into_owned());
            state.branch = Some(info.branch);
            state.base_commit = Some(info.base_commit);
        }
        state.repo_root = Some(root.to_string_lossy().into_owned());
        state.is_git = true;
        run::save_state(&run_dir, &state).unwrap();
        (
            fleet.paths.root().to_path_buf(),
            RunRef {
                run_id,
                run_dir,
                state,
            },
        )
    }

    fn commit_worktree_file(worktree: &Path, file: &str, body: &str) {
        std::fs::write(worktree.join(file), body).unwrap();
        git_sync(worktree, &["add", "."]);
        git_sync(worktree, &["commit", "-qm", &format!("write {file}")]);
    }

    fn settle(run_dir: &Path) {
        let mut state = run::load_state(run_dir).unwrap();
        state.status = RunStatus::Settled;
        state.settled_at = Some(crate::util::now_iso());
        run::save_state(run_dir, &state).unwrap();
    }

    #[tokio::test]
    async fn diff_cases() {
        {
            let dir = tmp_dir("pilotfish-int-flat-");
            make_run(&dir, "flat", false).await;
            let result = diff_core_with_env("flat", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Ok);
            assert_eq!(
                result.out,
                vec!["not applicable (run has no isolated worktree)"]
            );
            assert!(!result.data.applicable);
            assert!(result.data.dirty.is_empty());
        }
        {
            let dir = init_repo("pilotfish-int-diff-");
            let (_fleet, target) = make_run(&dir, "worker", true).await;
            let worktree = PathBuf::from(target.state.worktree.clone().unwrap());
            commit_worktree_file(&worktree, "hello.txt", "hi\n");

            let stat = diff_core_with_env("worker", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(stat.code, ExitCode::Ok);
            assert!(stat.out[0].contains("hello.txt"), "{}", stat.out[0]);
            assert!(stat.err.is_empty(), "{:?}", stat.err);

            // An uncommitted change is invisible to diff but warned about.
            std::fs::write(worktree.join("forgot.txt"), "u\n").unwrap();
            let dirty = diff_core_with_env("worker", Some(&dir), true, None)
                .await
                .unwrap();
            assert_eq!(dirty.out[0], "hello.txt");
            assert_eq!(dirty.data.dirty, vec!["?? forgot.txt".to_string()]);
            assert!(
                dirty.err[0].contains("1 uncommitted change(s)")
                    && dirty.err[0].contains("forgot.txt"),
                "{}",
                dirty.err[0]
            );
        }
    }

    #[tokio::test]
    async fn merge_cases() {
        {
            let dir = init_repo("pilotfish-int-merge-");
            let (_fleet, target) = make_run(&dir, "worker", true).await;
            let worktree = PathBuf::from(target.state.worktree.clone().unwrap());
            commit_worktree_file(&worktree, "hello.txt", "hi\n");
            settle(&target.run_dir);

            let result = merge_core_with_env("worker", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Ok, "{:?}", result.err);
            assert_eq!(result.data.branch, "pilotfish/worker-8141530");
            assert!(result.data.committed);
            assert!(
                result.out[0].starts_with("merged pilotfish/worker-8141530 into "),
                "{:?}",
                result.out
            );
            assert_eq!(result.data.into, target.state.repo_root.clone().unwrap());
            assert!(result.out[1].contains("integration checks"));
            let hello = std::fs::read_to_string(dir.join("hello.txt")).unwrap();
            assert_eq!(hello, "hi\n", "the branch actually landed");
        }
        {
            let dir = init_repo("pilotfish-int-mergegates-");
            let (_fleet, target) = make_run(&dir, "flat", false).await;
            let result = merge_core_with_env("flat", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Error);
            assert!(
                result.err[0].contains("is starting — only settled runs can be merged"),
                "{}",
                result.err[0]
            );
            // Settled but without a branch.
            let mut state = target.state.clone();
            state.status = RunStatus::Settled;
            state.settled_at = Some(crate::util::now_iso());
            run::save_state(&target.run_dir, &state).unwrap();
            let result = merge_core_with_env("flat", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Error);
            assert!(result.err[0].contains("has no branch"), "{}", result.err[0]);
        }
        {
            let dir = init_repo("pilotfish-int-nocommit-");
            let (_fleet, target) = make_run(&dir, "worker", true).await;
            let worktree = PathBuf::from(target.state.worktree.clone().unwrap());
            commit_worktree_file(&worktree, "feat.txt", "f\n");
            settle(&target.run_dir);

            let result = merge_core_with_env("worker", Some(&dir), true, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Ok);
            assert!(
                result.out[0].contains("(staged, not committed)"),
                "{:?}",
                result.out
            );
            assert!(!result.data.committed);
            let status = git::git_raw(&["status", "--porcelain"], &dir).await;
            assert!(status.stdout.contains('A'), "{}", status.stdout);
            git_sync(&dir, &["merge", "--abort"]);
        }
        {
            // A hand-edited run.json naming `main` as the branch must never
            // reach `git merge`.
            let dir = init_repo("pilotfish-int-mergemain-");
            let (_fleet, target) = make_run(&dir, "worker", true).await;
            settle(&target.run_dir);
            let mut state = run::load_state(&target.run_dir).unwrap();
            state.branch = Some("main".to_string());
            run::save_state(&target.run_dir, &state).unwrap();

            let result = merge_core_with_env("worker", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Error);
            assert!(
                result.err[0].contains("not a pilotfish/ branch"),
                "{}",
                result.err[0]
            );
        }
        {
            // A merge already in progress in the checkout is the human's —
            // the op must refuse (exit 1) and never abort it.
            let dir = init_repo("pilotfish-int-inprog-");
            let (_fleet, target) = make_run(&dir, "worker", true).await;
            settle(&target.run_dir);
            // A second branch edits seed.txt; main moves on, then the merge
            // conflicts and is left in progress (MERGE_HEAD present).
            git_sync(&dir, &["checkout", "-q", "-b", "feature"]);
            std::fs::write(dir.join("seed.txt"), "feature version\n").unwrap();
            git_sync(&dir, &["commit", "-qam", "feature version"]);
            git_sync(&dir, &["checkout", "-q", "main"]);
            std::fs::write(dir.join("seed.txt"), "main version\n").unwrap();
            git_sync(&dir, &["commit", "-qam", "main version"]);
            let merged = git::git_raw(&["merge", "feature"], &dir).await;
            assert!(!merged.ok(), "the setup merge must conflict");
            assert!(
                git::git_raw(&["rev-parse", "-q", "--verify", "MERGE_HEAD"], &dir)
                    .await
                    .ok(),
                "setup left MERGE_HEAD behind"
            );
            let seed = std::fs::read_to_string(dir.join("seed.txt")).unwrap();
            assert!(seed.contains("<<<<<<< HEAD"), "{seed}");

            let result = merge_core_with_env("worker", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(result.code, ExitCode::Error, "{:?}", result.err);
            assert!(
                result.err[0].contains("already in progress"),
                "{}",
                result.err[0]
            );
            // The human's merge is intact: MERGE_HEAD present, conflicts kept.
            assert!(
                git::git_raw(&["rev-parse", "-q", "--verify", "MERGE_HEAD"], &dir)
                    .await
                    .ok(),
                "the human's merge was not aborted"
            );
            let seed = std::fs::read_to_string(dir.join("seed.txt")).unwrap();
            assert!(seed.contains("<<<<<<< HEAD"), "{seed}");
        }
    }

    #[tokio::test]
    async fn merge_conflict_exit_5() {
        let dir = init_repo("pilotfish-int-conflict-");
        let (_fleet, target) = make_run(&dir, "worker", true).await;
        let worktree = PathBuf::from(target.state.worktree.clone().unwrap());
        // Worker edits seed.txt on its branch; the parent moves on too.
        std::fs::write(worktree.join("seed.txt"), "worker version\n").unwrap();
        git_sync(&worktree, &["add", "."]);
        git_sync(&worktree, &["commit", "-qm", "worker version"]);
        std::fs::write(dir.join("seed.txt"), "parent version\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "parent version"]);
        settle(&target.run_dir);

        let result = merge_core_with_env("worker", Some(&dir), false, None)
            .await
            .unwrap();
        assert_eq!(result.code, ExitCode::MergeConflict);
        assert_eq!(result.out, Vec::<String>::new());
        let err = result.err.join("\n");
        assert!(err.contains("conflicts in:\nseed.txt"), "{err}");
        assert!(err.contains("The merge was aborted; the checkout is clean"));
        assert!(err.contains("rebase its branch pilotfish/worker-8141530"));
        assert!(
            err.contains(
                target
                    .state
                    .base_commit
                    .as_deref()
                    .and_then(|c| c.get(..7))
                    .unwrap()
            )
        );
        // The abort left the checkout clean, and the data names the conflicts.
        assert!(!git::worktree_is_dirty(&dir).await);
        assert_eq!(result.data.conflicts, vec!["seed.txt".to_string()]);
    }

    #[tokio::test]
    async fn cleanup_keeps_report() {
        {
            let dir = init_repo("pilotfish-int-cleanup-");
            let (fleet, target) = make_run(&dir, "worker", true).await;
            let report = crate::fleet::report::report_path(&fleet, &target.run_id);
            std::fs::write(&report, "# Fleet Report\n").unwrap();
            let worktree = PathBuf::from(target.state.worktree.clone().unwrap());
            commit_worktree_file(&worktree, "hello.txt", "hi\n");
            settle(&target.run_dir);

            let result = cleanup_runs(&fleet, "worker", false).await.unwrap();
            assert_eq!(result.code, ExitCode::Ok, "{:?}", result.err);
            assert_eq!(result.out, vec![format!("archived {}", target.run_id)]);
            assert_eq!(result.data.archived, vec![target.run_id.clone()]);
            assert!(!worktree.exists());
            let listed = git::git_raw(
                &["branch", "--list", target.state.branch.as_deref().unwrap()],
                &dir,
            )
            .await;
            // The branch was never merged, so non-force cleanup keeps it.
            assert!(!listed.stdout.trim().is_empty(), "unmerged branch kept");
            assert!(
                result
                    .err
                    .iter()
                    .any(|e| e.contains("kept unmerged branch")),
                "{:?}",
                result.err
            );
            // The report and events survive the archive.
            assert!(crate::fleet::report::report_path(&fleet, &target.run_id).is_file());

            let again = cleanup_runs(&fleet, "worker", false).await.unwrap();
            assert_eq!(
                again.out,
                vec![format!("{} is already archived", target.run_id)]
            );
            // diff on an archived (worktree gone) run is not applicable.
            let diffed = diff_core_with_env("worker", Some(&dir), false, None)
                .await
                .unwrap();
            assert_eq!(
                diffed.out,
                vec!["not applicable (run has no isolated worktree)"]
            );
        }
        {
            let dir = init_repo("pilotfish-int-targets-");
            let (fleet, _target) = make_run(&dir, "flat", false).await;
            // An unknown target is a hard error.
            let err = cleanup_runs(&fleet, "ghost", false).await.unwrap_err();
            assert!(err.to_string().contains("No run found"), "{err}");
            let err = cleanup_runs(&fleet, "  ", false).await.unwrap_err();
            assert_eq!(err.to_string(), "cleanup: <name|all> required");
        }
    }

    #[tokio::test]
    async fn cleanup_dirty_needs_force() {
        let dir = init_repo("pilotfish-int-dirty-");
        let (fleet, target) = make_run(&dir, "worker", true).await;
        let worktree = PathBuf::from(target.state.worktree.clone().unwrap());
        commit_worktree_file(&worktree, "hello.txt", "hi\n");
        std::fs::write(worktree.join("forgot.txt"), "uncommitted\n").unwrap();
        settle(&target.run_dir);

        let refused = cleanup_runs(&fleet, "worker", false).await.unwrap();
        assert_eq!(refused.code, ExitCode::Error);
        assert!(
            refused.err[0].contains("could not be removed"),
            "{}",
            refused.err[0]
        );
        assert_eq!(refused.data.refused, vec![target.run_id.clone()]);
        assert!(worktree.exists());
        let state = run::load_state(&target.run_dir).unwrap();
        assert_ne!(state.status, RunStatus::Archived);

        let forced = cleanup_runs(&fleet, "worker", true).await.unwrap();
        assert_eq!(forced.code, ExitCode::Ok, "{:?}", forced.err);
        assert!(!worktree.exists());
        let state = run::load_state(&target.run_dir).unwrap();
        assert_eq!(state.status, RunStatus::Archived);
    }

    #[tokio::test]
    async fn cleanup_refuses_unsafe_worktree() {
        let dir = init_repo("pilotfish-int-hostile-");
        let (fleet, target) = make_run(&dir, "worker", true).await;
        settle(&target.run_dir);
        // A hand-edited run.json naming the checkout itself as the worktree.
        let mut state = run::load_state(&target.run_dir).unwrap();
        state.worktree = Some(dir.to_string_lossy().into_owned());
        run::save_state(&target.run_dir, &state).unwrap();

        let result = cleanup_runs(&fleet, "worker", true).await.unwrap();
        assert_eq!(result.code, ExitCode::Error);
        assert_eq!(result.data.failed.len(), 1);
        assert!(
            result.err.iter().any(|e| e.contains("repo root")),
            "{:?}",
            result.err
        );
        // The checkout survives and the run is not archived.
        assert!(dir.join("seed.txt").is_file());
        assert_eq!(
            run::load_state(&target.run_dir).unwrap().status,
            RunStatus::Settled
        );
    }

    #[tokio::test]
    async fn cleanup_force_aborts() {
        {
            let dir = init_repo("pilotfish-int-forceabort-");
            let (fleet, target) = make_run(&dir, "slow", false).await;
            // A "monitor" that processes the abort 500 ms in: status goes
            // stopped and the pid (our own test process) goes away. While it is
            // still Running with a live pid, a plain cleanup would refuse.
            let mut state = run::load_state(&target.run_dir).unwrap();
            state.status = RunStatus::Running;
            state.pid = Some(std::process::id().cast_signed());
            run::save_state(&target.run_dir, &state).unwrap();
            let tardy = target.run_dir.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Ok(mut s) = run::load_state(&tardy) {
                    s.status = RunStatus::Stopped;
                    s.pid = None;
                    run::save_state(&tardy, &s).unwrap();
                }
            });

            let forced = cleanup_runs(&fleet, "slow", true).await.unwrap();
            assert_eq!(forced.code, ExitCode::Ok, "{:?}", forced.err);
            assert!(!forced.err.iter().any(|e| e.contains("archiving anyway")));
            let state = run::load_state(&target.run_dir).unwrap();
            assert_eq!(state.status, RunStatus::Archived);
            // The abort envelope reached the inbox for the monitor to see.
            let raw = std::fs::read_to_string(
                crate::paths::FleetPaths::new(&fleet).run_inbox(&target.run_id),
            )
            .unwrap();
            assert!(raw.contains("\"abort\""), "{raw}");
        }
        {
            let dir = init_repo("pilotfish-int-abortparty-");
            let (fleet, target) = make_run(&dir, "slowp", false).await;
            let mut state = run::load_state(&target.run_dir).unwrap();
            state.status = RunStatus::Running;
            state.pid = Some(std::process::id().cast_signed());
            run::save_state(&target.run_dir, &state).unwrap();
            let tardy = target.run_dir.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Ok(mut s) = run::load_state(&tardy) {
                    s.status = RunStatus::Stopped;
                    s.pid = None;
                    run::save_state(&tardy, &s).unwrap();
                }
            });

            let forced = cleanup_runs(&fleet, "slowp", true).await.unwrap();
            assert_eq!(forced.code, ExitCode::Ok, "{:?}", forced.err);
            // The abort envelope is attributed to the fleet's acting session —
            // the default session when fleet.json names none, like every other
            // orchestrator-originated steering.
            let raw = std::fs::read_to_string(
                crate::paths::FleetPaths::new(&fleet).run_inbox(&target.run_id),
            )
            .unwrap();
            let envelopes: Vec<Envelope> = raw
                .lines()
                .filter(|l| !l.is_empty())
                .filter_map(Envelope::parse_line)
                .collect();
            assert_eq!(envelopes.len(), 1);
            assert_eq!(
                envelopes[0].from,
                Party::Orchestrator(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION)
            );
        }
    }

    #[tokio::test]
    async fn cleanup_all_sweep() {
        {
            let dir = init_repo("pilotfish-int-all-");
            let (fleet, _settled) = make_run(&dir, "done", false).await;
            settle(&fleet.join("runs").join("done-20260828141530"));
            // A second run that looks alive.
            let busy_id = "busy-20260828141531";
            let busy_dir = fleet.join("runs").join(busy_id);
            std::fs::create_dir_all(&busy_dir).unwrap();
            let mut busy = RunState::new(
                fleet.to_string_lossy().as_ref(),
                busy_id,
                "busy",
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
            busy.status = RunStatus::Running;
            busy.pid = Some(std::process::id().cast_signed());
            run::save_state(&busy_dir, &busy).unwrap();

            let all = cleanup_runs(&fleet, "all", false).await.unwrap();
            assert_eq!(all.code, ExitCode::Ok);
            assert_eq!(all.data.archived, vec!["done-20260828141530".to_string()]);
            assert!(
                all.err.iter().any(|e| e.contains("skipping busy")),
                "{:?}",
                all.err
            );
            assert_eq!(all.data.skipped, vec![busy_id.to_string()]);
            assert_eq!(
                run::load_state(&busy_dir).unwrap().status,
                RunStatus::Running
            );
            // --force still never aborts a run the sweep was not pointed at:
            // `all` leaves the running one alone (only an explicit name may
            // abort a live worker), and reports it as skipped.
            let forced = cleanup_runs(&fleet, "all", true).await.unwrap();
            assert_eq!(forced.code, ExitCode::Ok, "{:?}", forced.err);
            assert_eq!(forced.data.archived, Vec::<String>::new());
            assert_eq!(forced.data.skipped, vec![busy_id.to_string()]);
            assert!(
                forced.err.iter().any(|e| e.contains("skipping busy")),
                "{:?}",
                forced.err
            );
            assert_eq!(
                run::load_state(&busy_dir).unwrap().status,
                RunStatus::Running,
                "`all --force` must leave a live worker alone"
            );
        }
        {
            let dir = init_repo("pilotfish-int-xsession-");
            let (fleet, mine) = make_run(&dir, "mine", false).await;
            settle(&mine.run_dir);
            let (_, theirs) = make_run(&dir, "theirs", false).await;
            // The second run belongs to another orchestrator session.
            let mut state = run::load_state(&theirs.run_dir).unwrap();
            state.orchestrator_id =
                Some(uuid::Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap());
            run::save_state(&theirs.run_dir, &state).unwrap();
            settle(&theirs.run_dir);

            // The whole-fleet sweep is still legitimate, but it says out loud
            // that it crosses sessions.
            let all = cleanup_runs(&fleet, "all", false).await.unwrap();
            assert_eq!(all.code, ExitCode::Ok, "{:?}", all.err);
            assert!(
                all.out
                    .iter()
                    .any(|l| l.contains("also covers 1 run(s) owned by other")),
                "{:?}",
                all.out
            );
            assert_eq!(all.data.archived.len(), 2, "both runs are still cleaned");
            // A fleet where everything is this session's gets no note (the
            // default-session fold includes the unowned legacy runs).
            let (_f2, own) = make_run(&dir, "own2", false).await;
            settle(&own.run_dir);
            let again = cleanup_runs(&fleet, "all", false).await.unwrap();
            assert!(
                !again.out.iter().any(|l| l.contains("also covers")),
                "no foreign runs left: {:?}",
                again.out
            );
        }
    }

    #[tokio::test]
    async fn cleanup_all_survives_failure() {
        let dir = init_repo("pilotfish-int-batchfail-");
        let (fleet, broken) = make_run(&dir, "broken", true).await;
        settle(&broken.run_dir);
        let (_fleet2, healthy) = make_run(&dir, "healthy", false).await;
        settle(&healthy.run_dir);
        // The broken run's git admin record is gone, as a crash left it in
        // production: `git worktree remove` refuses even with --force — the
        // exact failure that used to abort the whole batch, cleaning nothing.
        let admin = dir.join(".git").join("worktrees").join(&broken.run_id);
        std::fs::remove_dir_all(&admin).unwrap();

        let all = cleanup_runs(&fleet, "all", true).await.unwrap();
        assert_eq!(all.code, ExitCode::Error, "the failure is reported");
        // The healthy run was still cleaned, and the broken one was not.
        assert_eq!(all.data.archived, vec![healthy.run_id.clone()]);
        assert!(
            !all.out.iter().any(|l| l.contains(&broken.run_id)),
            "broken run must not read as archived: {:?}",
            all.out
        );
        // The failure is collected with its reason, naming the worktree.
        assert_eq!(all.data.failed.len(), 1);
        assert!(
            all.data.failed[0].contains(&broken.run_id),
            "{}",
            all.data.failed[0]
        );
        assert!(
            all.err.iter().any(|l| l.contains("could not be removed")),
            "{:?}",
            all.err
        );
        assert_eq!(
            run::load_state(&broken.run_dir).unwrap().status,
            RunStatus::Settled,
            "the failed run is left untouched"
        );
        assert_eq!(
            run::load_state(&healthy.run_dir).unwrap().status,
            RunStatus::Archived
        );
        // A single named target still fails loudly on the same breakage.
        let named = cleanup_runs(&fleet, "broken", true).await.unwrap();
        assert_eq!(named.code, ExitCode::Error);
        assert_eq!(named.data.failed.len(), 1);
        assert_eq!(named.data.archived, Vec::<String>::new());
    }
}
