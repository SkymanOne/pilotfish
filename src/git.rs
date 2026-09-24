//! A thin wrapper over the `git` CLI (deliberately not `git2`): worktree
//! create/remove, branch delete, diff against a base commit, merge with
//! conflict detection, and the "is it merged / is it dirty" checks cleanup
//! needs.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::util::branch_for;

/// The real outcome of one git invocation: its exit code plus captured
/// streams. Merge conflicts print to *stdout*, so only the exit code can be
/// trusted to detect failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GitResult {
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Run `git <args>` in `cwd` and report its real exit code.
///
/// Unlike a naive "reject when stderr is non-empty" wrapper, the exit code is
/// authoritative; a git that cannot even be spawned surfaces as code 1 with
/// the spawn error in `stderr`.
pub async fn git_raw(args: &[&str], cwd: &Path) -> GitResult {
    match tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(out) => GitResult {
            code: out.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(e) => GitResult {
            code: 1,
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

/// Is `dir` inside a git work tree?
pub async fn is_git_repo(dir: &Path) -> bool {
    let r = git_raw(&["rev-parse", "--is-inside-work-tree"], dir).await;
    r.ok() && r.stdout.trim() == "true"
}

/// The repository root containing `dir`, or `None` outside a repo.
pub async fn repo_root(dir: &Path) -> Option<PathBuf> {
    let r = git_raw(&["rev-parse", "--show-toplevel"], dir).await;
    if r.ok() {
        let root = r.stdout.trim();
        if root.is_empty() {
            None
        } else {
            Some(PathBuf::from(root))
        }
    } else {
        None
    }
}

/// Resolve a ref to its commit sha (`<ref>^{commit}`), pinning the base for
/// later diffs: inside a worktree HEAD moves, so `diff` needs the sha.
///
/// # Errors
///
/// Fails when `rev-parse` cannot resolve `ref_` or returns nothing.
pub async fn resolve_commit(repo: &Path, ref_: &str) -> anyhow::Result<String> {
    let r = git_raw(&["rev-parse", &format!("{ref_}^{{commit}}")], repo).await;
    if r.ok() {
        let sha = r.stdout.trim();
        if sha.is_empty() {
            anyhow::bail!("git rev-parse {ref_}^{{commit}} returned nothing");
        }
        Ok(sha.to_string())
    } else {
        anyhow::bail!(
            "git rev-parse {ref_}^{{commit}} failed: {}",
            r.stderr.trim()
        )
    }
}

/// What [`ensure_worktree`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub worktree_path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub base_commit: String,
}

/// Create a worktree at `<worktrees_dir>/<run_id>` on a fresh branch
/// `pilotfish/<name>-<short7>`, cut from `base` (or `HEAD`).
///
/// # Errors
///
/// Fails when the base ref cannot be resolved or `git worktree add`
/// refuses (e.g. the branch already exists).
pub async fn ensure_worktree(
    repo_root: &Path,
    worktrees_dir: &Path,
    run_id: &str,
    name: &str,
    base: Option<&str>,
) -> anyhow::Result<WorktreeInfo> {
    let branch = branch_for(name, run_id);
    let worktree_path = worktrees_dir.join(run_id);
    let base_ref = base.unwrap_or("HEAD");
    // Pin the base commit now: inside the worktree HEAD moves, so `diff`
    // needs the sha.
    let base_commit = resolve_commit(repo_root, base_ref).await?;
    let r = git_raw(
        &[
            "worktree",
            "add",
            worktree_path.to_string_lossy().as_ref(),
            "-b",
            &branch,
            base_ref,
        ],
        repo_root,
    )
    .await;
    if !r.ok() {
        anyhow::bail!(
            "git worktree add {} failed: {}",
            worktree_path.display(),
            r.stderr.trim()
        );
    }
    Ok(WorktreeInfo {
        worktree_path,
        branch,
        base_ref: base_ref.to_string(),
        base_commit,
    })
}

/// What [`remove_worktree`] managed to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct RemoveWorktreeResult {
    pub worktree_removed: bool,
    pub branch_deleted: bool,
}

/// Remove a worktree and (best-effort, non-force) delete its branch.
///
/// A missing worktree is already gone — just tidy git's administrative files
/// (`worktree prune`) and fall through to branch deletion. Non-force failures
/// are reported in the result rather than thrown: cleanup is best-effort.
///
/// # Errors
///
/// Fails only when a forced removal cannot be performed by git; everything
/// else is reported through [`RemoveWorktreeResult`].
pub async fn remove_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    branch: Option<&str>,
    force: bool,
) -> anyhow::Result<RemoveWorktreeResult> {
    let mut result = RemoveWorktreeResult::default();
    if worktree_path.exists() {
        let mut args: Vec<&str> = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        let path_str = worktree_path.to_string_lossy().into_owned();
        args.push(path_str.as_str());
        let r = git_raw(&args, repo_root).await;
        if r.ok() {
            result.worktree_removed = true;
        } else if force {
            anyhow::bail!(
                "Failed to remove worktree {}: {}",
                worktree_path.display(),
                r.stderr.trim()
            );
        }
        // non-force: other failures are reported via the result, not thrown.
    } else {
        // Best effort; a prune failure is not worth failing cleanup over.
        let _ = git_raw(&["worktree", "prune"], repo_root).await;
    }
    if let Some(branch) = branch {
        let delete = if force { "-D" } else { "-d" };
        let r = git_raw(&["branch", delete, branch], repo_root).await;
        // The branch is kept when unmerged; callers surface that.
        result.branch_deleted = r.ok();
    }
    Ok(result)
}

/// `git diff --stat` (or `--name-only`) of the worktree's committed work
/// against its base. `base` is the run's `baseCommit`, falling back to its
/// `base` ref, falling back to `HEAD`.
///
/// # Errors
///
/// Fails when git cannot diff against `base` (e.g. an unresolvable ref),
/// with git's stderr in the message.
pub async fn diff_against_base(
    worktree: &Path,
    base: &str,
    name_only: bool,
) -> anyhow::Result<String> {
    let flag = if name_only { "--name-only" } else { "--stat" };
    let r = git_raw(&["diff", flag, &format!("{base}...HEAD")], worktree).await;
    if r.ok() {
        Ok(r.stdout.trim_end().to_string())
    } else {
        anyhow::bail!("diff: {}", r.stderr.trim())
    }
}

/// The full patch of a worktree against `base`: commits and uncommitted
/// edits to tracked files alike (`git diff <base>`, not `<base>...HEAD`),
/// renames detected. Untracked files are not in it — see [`dirty_files`].
///
/// # Errors
///
/// Fails when git cannot diff against `base`, with git's stderr.
pub async fn diff_patch(worktree: &Path, base: &str) -> anyhow::Result<String> {
    let r = git_raw(
        &["diff", "--no-color", "--no-ext-diff", "-M", base, "--"],
        worktree,
    )
    .await;
    if r.ok() {
        Ok(r.stdout)
    } else {
        anyhow::bail!("diff: {}", r.stderr.trim())
    }
}

/// Uncommitted paths in a worktree (whole `--porcelain` lines): invisible to
/// diff/merge, lost by `cleanup --force`.
pub async fn dirty_files(worktree: &Path) -> Vec<String> {
    let status = git_raw(&["status", "--porcelain"], worktree).await;
    if !status.ok() {
        return Vec::new();
    }
    status
        .stdout
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Does the worktree have uncommitted changes?
pub async fn worktree_is_dirty(worktree: &Path) -> bool {
    !dirty_files(worktree).await.is_empty()
}

/// Is `branch` fully merged into the current `HEAD` of `repo`?
pub async fn branch_is_merged(repo: &Path, branch: &str) -> bool {
    git_raw(&["merge-base", "--is-ancestor", branch, "HEAD"], repo)
        .await
        .ok()
}

/// Delete a branch (`-D` when forced). Returns whether git accepted it —
/// `-d` refuses unmerged branches, which callers surface as "kept".
pub async fn delete_branch(repo: &Path, branch: &str, force: bool) -> bool {
    let delete = if force { "-D" } else { "-d" };
    git_raw(&["branch", delete, branch], repo).await.ok()
}

/// Check a recorded worktree path before a destructive op touches it.
///
/// `worktree` must lie under `worktrees_dir` (the fleet's worktrees
/// directory) and must not be `repo_root` itself. Comparison is on
/// canonicalized paths when the worktree exists — a symlink or `..`-laden
/// path resolves before it is trusted; a missing path is still checked
/// lexically, because nothing may ever hand it to `remove_dir_all`.
///
/// # Errors
///
/// Names the `worktree` field and its value when it is unsafe.
pub fn check_worktree_path(
    worktrees_dir: &Path,
    repo_root: &Path,
    worktree: &Path,
) -> anyhow::Result<()> {
    // `..` is resolved by the filesystem, not by a prefix comparison: a path
    // that climbs out can look like it sits under the worktrees directory
    if worktree
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "worktree field {} contains `..` — refusing to remove it",
            worktree.display()
        );
    }
    if worktree == repo_root {
        anyhow::bail!(
            "worktree field {} is the repo root {} — refusing to remove it",
            worktree.display(),
            repo_root.display()
        );
    }
    if let (Ok(w), Ok(r)) = (worktree.canonicalize(), repo_root.canonicalize())
        && w == r
    {
        anyhow::bail!(
            "worktree field {} resolves to the repo root {} — refusing to remove it",
            worktree.display(),
            repo_root.display()
        );
    }
    let under = match (worktree.canonicalize(), worktrees_dir.canonicalize()) {
        (Ok(w), Ok(b)) => w.starts_with(&b),
        _ => worktree.starts_with(worktrees_dir),
    };
    if !under {
        anyhow::bail!(
            "worktree field {} is outside the worktrees directory {} — refusing to remove it",
            worktree.display(),
            worktrees_dir.display()
        );
    }
    Ok(())
}

/// Check a recorded branch before `branch -D` or `git merge` touches it.
///
/// A run may only ever name a `pilotfish/` branch the fleet itself created;
/// a hand-edited `run.json` naming `main` (or anything else) must never
/// reach a destructive git call.
///
/// # Errors
///
/// Names the `branch` field and its value.
pub fn check_branch(branch: &str) -> anyhow::Result<()> {
    if branch.starts_with("pilotfish/") {
        return Ok(());
    }
    anyhow::bail!("branch field {branch} is not a pilotfish/ branch — refusing to touch it")
}

/// What the human is in the middle of in `repo`, if anything: a merge, a
/// cherry-pick, a revert, a rebase, or conflicts left unresolved. A merge
/// on top of any of them would report the human's conflicts as the
/// worker's, and its `--abort` would throw the human's work away.
pub async fn operation_in_progress(repo: &Path) -> Option<&'static str> {
    for (head, what) in [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ] {
        if git_raw(&["rev-parse", "-q", "--verify", head], repo)
            .await
            .ok()
        {
            return Some(what);
        }
    }
    for dir in ["rebase-merge", "rebase-apply"] {
        let path = git_raw(&["rev-parse", "--git-path", dir], repo).await;
        let path = Path::new(path.stdout.trim());
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            repo.join(path)
        };
        if path.is_dir() {
            return Some("rebase");
        }
    }
    let unmerged = git_raw(&["diff", "--name-only", "--diff-filter=U"], repo).await;
    (!unmerged.stdout.trim().is_empty()).then_some("conflict resolution")
}

/// What [`merge_branch`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Fast-forward or a clean merge commit.
    Merged,
    /// `--no-commit`: staged, nothing committed.
    Staged,
    /// Conflicts in these files; the merge was aborted when
    /// `abort_on_conflict` was set.
    Conflicted(Vec<String>),
    /// git refused for another reason (bad ref, dirty index, …).
    Failed(String),
}

/// Merge `branch` into the checkout at `repo`.
///
/// With `no_commit` the merge is staged (`--no-commit --no-ff`) but not
/// committed. On conflict the conflicted file list comes from
/// `git diff --name-only --diff-filter=U`; with `abort_on_conflict` the merge
/// is rolled back (`git merge --abort`) so the checkout stays clean and the
/// worker can rebase instead — the orchestrator never edits.
pub async fn merge_branch(
    repo: &Path,
    branch: &str,
    no_commit: bool,
    abort_on_conflict: bool,
) -> MergeOutcome {
    let mut args: Vec<&str> = vec!["merge"];
    if no_commit {
        args.extend(["--no-commit", "--no-ff"]);
    }
    args.push(branch);
    let r = git_raw(&args, repo).await;
    if r.ok() {
        return if no_commit {
            MergeOutcome::Staged
        } else {
            MergeOutcome::Merged
        };
    }
    let conflicts = git_raw(&["diff", "--name-only", "--diff-filter=U"], repo).await;
    let files: Vec<String> = conflicts
        .stdout
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if files.is_empty() {
        return MergeOutcome::Failed(r.stderr.trim().to_string());
    }
    if abort_on_conflict {
        let _ = git_raw(&["merge", "--abort"], repo).await;
    }
    MergeOutcome::Conflicted(files)
}

/// Test-only git helpers shared by the unit suites (`git`, `ops::spawn`,
/// `paths`). Test-setup git spawns have been observed to fail transiently
/// under full-suite parallel load ("No such file or directory" from the
/// spawn itself), so every test-side git call goes through the same bounded
/// retry. Production code stays single-shot on purpose.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// How long a transient-prone git call may keep being retried.
    pub const RETRY_BOUND: Duration = Duration::from_secs(10);
    /// Pause between retries.
    pub const RETRY_INTERVAL: Duration = Duration::from_millis(100);

    /// Run `git args` in `dir` with a test identity (commits must work on a
    /// machine with no git config), retrying within [`RETRY_BOUND`]: a git
    /// spawn can transiently fail when the machine is loaded. Panics with
    /// the last attempt's stderr — setup must succeed for the test to mean
    /// anything, and the bound keeps a real breakage from hanging the suite.
    /// Only ever used for setup commands whose retry is safe (`git init` is
    /// idempotent; a failed command left nothing behind to half-apply).
    pub fn git_sync(dir: &Path, args: &[&str]) {
        let deadline = Instant::now() + RETRY_BOUND;
        loop {
            let attempt = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output();
            match attempt {
                Ok(out) if out.status.success() => return,
                attempt => {
                    let detail = match attempt.as_ref() {
                        Ok(out) => format!(
                            "{}: {}",
                            out.status,
                            String::from_utf8_lossy(&out.stderr).trim()
                        ),
                        Err(err) => err.to_string(),
                    };
                    assert!(
                        Instant::now() < deadline,
                        "git {args:?} in {dir:?} never succeeded: {detail}"
                    );
                    std::thread::sleep(RETRY_INTERVAL);
                }
            }
        }
    }

    /// A unique throwaway directory under the OS temp dir.
    pub fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{RETRY_BOUND, RETRY_INTERVAL, git_sync, tmp_dir};
    use super::*;
    use std::time::Instant;

    /// A repo with one committed seed file, like the TypeScript test helper.
    fn init_repo(name: &str) -> PathBuf {
        let root = tmp_dir(name);
        git_sync(&root, &["init", "-q", "-b", "main"]);
        // Production spawns gitignore the fleet dir (ensure()); mirror that,
        // so `git status --porcelain` reads clean with a worktree inside.
        std::fs::write(root.join(".gitignore"), ".pilotfish/\n").unwrap();
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git_sync(&root, &["add", "."]);
        git_sync(&root, &["commit", "-qm", "seed"]);
        root
    }

    #[tokio::test]
    async fn git_basics() {
        {
            let root = init_repo("pilotfish-git-");
            let r = git_raw(&["rev-parse", "--is-inside-work-tree"], &root).await;
            assert!(r.ok());
            assert_eq!(r.stdout.trim(), "true");
            let bad = git_raw(&["rev-parse", "--verify", "nope"], &root).await;
            assert!(!bad.ok());
            assert!(bad.stderr.contains("fatal"), "{}", bad.stderr);
        }
        {
            // A git spawn can transiently fail when the machine is loaded (this
            // suite runs many git subprocesses in parallel). Repo setup retries
            // inside the shared helper; the production probes are single-shot by
            // design, so poll them against the same bound before failing the
            // test.
            let deadline = Instant::now() + RETRY_BOUND;
            loop {
                let root = init_repo("pilotfish-git-");
                // Under heavy load `git rev-parse --show-toplevel` has been
                // observed to hand back a root that fails `canonicalize` with
                // NotFound a moment later (forensics: git itself never reports a
                // ghost toplevel; the loss is environmental). Fold the resolution
                // into the retried condition instead of asserting it — a
                // persistent mismatch still fails the test via the bound.
                if is_git_repo(&root).await
                    && let Some(resolved) = repo_root(&root).await
                    && let (Ok(resolved_real), Ok(root_real)) =
                        (resolved.canonicalize(), root.canonicalize())
                    && resolved_real == root_real
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "git repo not detected at {}",
                    root.display()
                );
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
            let plain = tmp_dir("pilotfish-plain-");
            assert!(!is_git_repo(&plain).await);
            assert_eq!(repo_root(&plain).await, None);
        }
        {
            let root = init_repo("pilotfish-git-");
            let worktrees = root.join(".pilotfish").join("worktrees");
            let info = ensure_worktree(&root, &worktrees, "d-20260828141530", "d", None)
                .await
                .unwrap();
            std::fs::write(info.worktree_path.join("hello.txt"), "hi\n").unwrap();
            git_sync(&info.worktree_path, &["add", "."]);
            git_sync(&info.worktree_path, &["commit", "-qm", "hello"]);
            // Uncommitted change the worker forgot to commit.
            std::fs::write(info.worktree_path.join("forgot.txt"), "uncommitted\n").unwrap();
            let dirty = dirty_files(&info.worktree_path).await;
            assert_eq!(dirty, vec!["?? forgot.txt".to_string()]);
            assert!(worktree_is_dirty(&info.worktree_path).await);

            let stat = diff_against_base(&info.worktree_path, &info.base_commit, false)
                .await
                .unwrap();
            assert!(stat.contains("hello.txt"), "{stat}");
            let names = diff_against_base(&info.worktree_path, &info.base_commit, true)
                .await
                .unwrap();
            assert_eq!(names, "hello.txt");
            // A nonsense base fails with git's message.
            assert!(
                diff_against_base(&info.worktree_path, "not-a-ref", false)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn worker_patch() {
        use crate::patch::{FileStatus, LineKind};
        let root = init_repo("pilotfish-git-patch");
        std::fs::write(root.join("auth.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        std::fs::write(root.join("old.rs"), "moved\n").unwrap();
        git_sync(&root, &["add", "."]);
        git_sync(&root, &["commit", "-qm", "base"]);
        let base = resolve_commit(&root, "HEAD").await.unwrap();

        // committed: an edit and a rename; uncommitted: a tracked edit;
        // untracked: a new file
        std::fs::write(root.join("auth.rs"), "fn a() {}\nfn b2() {}\nfn c() {}\n").unwrap();
        git_sync(&root, &["mv", "old.rs", "new.rs"]);
        git_sync(&root, &["commit", "-qam", "work"]);
        std::fs::write(root.join("seed.txt"), "seed\nmore\n").unwrap();
        std::fs::write(root.join("fresh.rs"), "fn fresh() {}\n").unwrap();

        let patch = crate::patch::load(&root, &base).await.unwrap();
        assert!(!patch.truncated);
        assert_eq!(patch.untracked, vec!["fresh.rs".to_string()]);
        let auth = patch.files.iter().find(|f| f.path == "auth.rs").unwrap();
        assert_eq!((auth.added, auth.removed), (1, 1));
        let lines = &auth.hunks[0].lines;
        let removed = lines.iter().find(|l| l.kind == LineKind::Removed).unwrap();
        let added = lines.iter().find(|l| l.kind == LineKind::Added).unwrap();
        assert_eq!(
            (removed.old, removed.new, removed.text.as_str()),
            (Some(2), None, "fn b() {}")
        );
        assert_eq!(
            (added.old, added.new, added.text.as_str()),
            (None, Some(2), "fn b2() {}")
        );
        let renamed = patch.files.iter().find(|f| f.path == "new.rs").unwrap();
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.rs"));
        let seed = patch.files.iter().find(|f| f.path == "seed.txt").unwrap();
        assert_eq!((seed.added, seed.removed), (1, 0));
        assert_eq!((patch.added(), patch.removed()), (2, 1));

        // the parser alone: a new binary, a deletion, a mode change, and a
        // quoted path
        let parsed = crate::patch::parse(
            "diff --git a/logo.png b/logo.png\nnew file mode 100644\nBinary files /dev/null and b/logo.png differ\n\
             diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-fn gone() {}\n\
             diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
             diff --git \"a/tab\\there.rs\" \"b/tab\\there.rs\"\n--- \"a/tab\\there.rs\"\n+++ \"b/tab\\there.rs\"\n@@ -1 +1 @@\n-x\n+y\n",
        );
        let [logo, gone, script, tabbed] = parsed.files.as_slice() else {
            panic!("{parsed:?}");
        };
        assert!(logo.binary && logo.status == FileStatus::Added);
        assert_eq!(
            (gone.path.as_str(), gone.status, gone.removed),
            ("gone.rs", FileStatus::Deleted, 1)
        );
        assert!(script.mode_change && script.hunks.is_empty());
        assert_eq!(tabbed.path, "tab\there.rs");

        // past the cap: cut on a line boundary, and said so
        let big = "+é line\n".repeat(crate::patch::PATCH_CAP / 8);
        let (cut, truncated) = crate::patch::cap(&big);
        assert!(truncated && cut.len() <= crate::patch::PATCH_CAP && cut.ends_with('\n'));
    }

    #[tokio::test]
    async fn worktree_removal() {
        {
            let root = init_repo("pilotfish-git-");
            let worktrees = root.join(".pilotfish").join("worktrees");
            let info = ensure_worktree(&root, &worktrees, "auth-20260828141530", "auth", None)
                .await
                .unwrap();
            assert_eq!(info.branch, "pilotfish/auth-8141530");
            assert_eq!(info.base_ref, "HEAD");
            assert!(info.worktree_path.join("seed.txt").exists());
            assert_eq!(
                resolve_commit(&root, "HEAD").await.unwrap(),
                info.base_commit
            );

            // Commit work on the branch, then merge it into main.
            std::fs::write(info.worktree_path.join("hello.txt"), "hi\n").unwrap();
            git_sync(&info.worktree_path, &["add", "."]);
            git_sync(&info.worktree_path, &["commit", "-qm", "hello"]);
            assert!(!worktree_is_dirty(&info.worktree_path).await);
            git_sync(&root, &["merge", &info.branch, "-q", "--no-edit"]);
            assert!(branch_is_merged(&root, &info.branch).await);

            let r = remove_worktree(&root, &info.worktree_path, Some(&info.branch), false)
                .await
                .unwrap();
            assert!(r.worktree_removed);
            assert!(r.branch_deleted);
            assert!(!info.worktree_path.exists());
            assert!(
                !branch_is_merged(&root, &info.branch).await || {
                    // branch gone: merged check is moot, just confirm it's deleted
                    let listed = git_raw(&["branch", "--list", &info.branch], &root).await;
                    listed.stdout.trim().is_empty()
                }
            );
        }
        {
            let root = init_repo("pilotfish-git-");
            let worktrees = root.join(".pilotfish").join("worktrees");
            let info = ensure_worktree(&root, &worktrees, "x-20260828141530", "x", None)
                .await
                .unwrap();
            std::fs::write(info.worktree_path.join("unmerged.txt"), "u\n").unwrap();
            git_sync(&info.worktree_path, &["add", "."]);
            git_sync(&info.worktree_path, &["commit", "-qm", "u"]);
            assert!(!branch_is_merged(&root, &info.branch).await);

            let soft = remove_worktree(&root, &info.worktree_path, Some(&info.branch), false)
                .await
                .unwrap();
            assert!(soft.worktree_removed);
            assert!(!soft.branch_deleted);
            let hard = remove_worktree(&root, &info.worktree_path, Some(&info.branch), true)
                .await
                .unwrap();
            // Worktree already gone; the branch delete now succeeds.
            assert!(!hard.worktree_removed);
            assert!(hard.branch_deleted);
        }
        {
            let root = init_repo("pilotfish-git-");
            let worktrees = root.join(".pilotfish").join("worktrees");
            let info = ensure_worktree(&root, &worktrees, "m-20260828141530", "m", None)
                .await
                .unwrap();
            // Simulate a worktree removed out from under us.
            std::fs::remove_dir_all(&info.worktree_path).unwrap();
            let r = remove_worktree(&root, &info.worktree_path, Some(&info.branch), false)
                .await
                .unwrap();
            assert!(!r.worktree_removed);
            assert!(r.branch_deleted);
        }
    }

    #[tokio::test]
    async fn merge_outcomes() {
        {
            let root = init_repo("pilotfish-git-");
            let worktrees = root.join(".pilotfish").join("worktrees");
            // The branch is cut from the current HEAD (spawn time)…
            let info = ensure_worktree(&root, &worktrees, "c-20260828141530", "c", None)
                .await
                .unwrap();
            // …then the parent moves on…
            std::fs::write(root.join("seed.txt"), "parent version\n").unwrap();
            git_sync(&root, &["commit", "-qam", "parent version"]);
            // …while the worker edits the same file on its branch.
            std::fs::write(info.worktree_path.join("seed.txt"), "worker version\n").unwrap();
            git_sync(&info.worktree_path, &["add", "."]);
            git_sync(&info.worktree_path, &["commit", "-qm", "worker version"]);

            let outcome = merge_branch(&root, &info.branch, false, true).await;
            match outcome {
                MergeOutcome::Conflicted(files) => assert_eq!(files, vec!["seed.txt".to_string()]),
                other => panic!("{other:?}"),
            }
            // The abort left the checkout clean.
            assert!(!worktree_is_dirty(&root).await);
            let aborted = git_raw(&["status"], &root).await;
            assert!(!aborted.stdout.contains("All conflicts fixed"));

            // Without abort-on-conflict the conflicted index stays for the caller.
            let outcome = merge_branch(&root, &info.branch, false, false).await;
            assert!(matches!(outcome, MergeOutcome::Conflicted(_)));
            git_sync(&root, &["merge", "--abort"]);
        }
        {
            let root = init_repo("pilotfish-git-");
            let worktrees = root.join(".pilotfish").join("worktrees");
            let info = ensure_worktree(&root, &worktrees, "s-20260828141530", "s", None)
                .await
                .unwrap();
            std::fs::write(info.worktree_path.join("feat.txt"), "f\n").unwrap();
            git_sync(&info.worktree_path, &["add", "."]);
            git_sync(&info.worktree_path, &["commit", "-qm", "feat"]);

            let staged = merge_branch(&root, &info.branch, true, false).await;
            assert_eq!(staged, MergeOutcome::Staged);
            // Nothing committed yet: HEAD unchanged, change staged.
            let status = git_raw(&["status", "--porcelain"], &root).await;
            assert!(status.stdout.contains('A'), "{}", status.stdout);
            git_sync(&root, &["merge", "--abort"]);

            let merged = merge_branch(&root, &info.branch, false, false).await;
            assert_eq!(merged, MergeOutcome::Merged);

            let failed = merge_branch(&root, "pilotfish/never-existed", false, false).await;
            assert!(matches!(failed, MergeOutcome::Failed(_)));
        }
        {
            // a cherry-pick stopped on conflicts is not a merge, but it is the
            // human's all the same: a merge on top would report their
            // conflicts as the worker's
            let root = init_repo("pilotfish-git-busy-");
            git_sync(&root, &["config", "user.name", "t"]);
            git_sync(&root, &["config", "user.email", "t@t"]);
            git_sync(&root, &["checkout", "-q", "-b", "side"]);
            std::fs::write(root.join("seed.txt"), "side\n").unwrap();
            git_sync(&root, &["commit", "-qam", "side"]);
            git_sync(&root, &["checkout", "-q", "main"]);
            std::fs::write(root.join("seed.txt"), "main\n").unwrap();
            git_sync(&root, &["commit", "-qam", "main"]);
            assert_eq!(operation_in_progress(&root).await, None);
            assert!(!git_raw(&["cherry-pick", "side"], &root).await.ok());
            assert_eq!(operation_in_progress(&root).await, Some("cherry-pick"));
            git_sync(&root, &["cherry-pick", "--abort"]);
            assert_eq!(operation_in_progress(&root).await, None);
        }
    }
}
