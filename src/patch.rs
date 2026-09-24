//! A worker's changes as structured data: `git diff <base>` in its worktree
//! (commits and uncommitted edits alike), parsed into files, hunks and
//! numbered lines, plus the untracked files the diff cannot show. The
//! desktop's changes pane draws this; nothing here renders.

use std::path::Path;

/// Past this much patch text the pane stops: the rest is an editor's job.
pub const PATCH_CAP: usize = 512 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patch {
    pub files: Vec<FileDiff>,
    /// Paths git does not track yet (`??` in `git status`).
    pub untracked: Vec<String>,
    /// The patch was cut at [`PATCH_CAP`].
    pub truncated: bool,
}

impl Patch {
    #[must_use]
    pub fn added(&self) -> usize {
        self.files.iter().map(|file| file.added).sum()
    }

    #[must_use]
    pub fn removed(&self) -> usize {
        self.files.iter().map(|file| file.removed).sum()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileStatus {
    #[default]
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    /// The path before a rename.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Only the file mode changed (or changed too).
    pub mode_change: bool,
    pub added: usize,
    pub removed: usize,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@ context` line as git wrote it.
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

/// The worktree's changes against `base`, capped at [`PATCH_CAP`].
///
/// # Errors
///
/// Fails when git cannot diff against `base`, with git's stderr.
pub async fn load(worktree: &Path, base: &str) -> anyhow::Result<Patch> {
    let text = crate::git::diff_patch(worktree, base).await?;
    let (text, truncated) = cap(&text);
    let mut patch = parse(text);
    patch.truncated = truncated;
    patch.untracked = crate::git::dirty_files(worktree)
        .await
        .into_iter()
        .filter_map(|line| line.strip_prefix("?? ").map(unquote))
        .collect();
    Ok(patch)
}

/// Cut at the last whole line under the cap.
pub(crate) fn cap(text: &str) -> (&str, bool) {
    if text.len() <= PATCH_CAP {
        return (text, false);
    }
    let mut end = PATCH_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = text[..end].rfind('\n').map_or(end, |at| at + 1);
    (&text[..end], true)
}

/// Parse `git diff` output. Tolerant: a line it does not understand is
/// skipped, never an error — the pane shows what it can.
#[must_use]
pub fn parse(text: &str) -> Patch {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            files.push(FileDiff {
                path: git_header_path(rest),
                ..FileDiff::default()
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        let in_hunk = !file.hunks.is_empty();
        if let Some(header) = raw.strip_prefix("@@") {
            let (old, new) = hunk_starts(header);
            old_line = old;
            new_line = new;
            file.hunks.push(Hunk {
                header: raw.to_string(),
                lines: Vec::new(),
            });
        } else if in_hunk && let Some(text) = raw.strip_prefix('+') {
            file.added += 1;
            push_line(file, LineKind::Added, None, Some(new_line), text);
            new_line += 1;
        } else if in_hunk && let Some(text) = raw.strip_prefix('-') {
            file.removed += 1;
            push_line(file, LineKind::Removed, Some(old_line), None, text);
            old_line += 1;
        } else if in_hunk && let Some(text) = raw.strip_prefix(' ') {
            push_line(
                file,
                LineKind::Context,
                Some(old_line),
                Some(new_line),
                text,
            );
            old_line += 1;
            new_line += 1;
        } else if raw.starts_with('\\') {
            // "\ No newline at end of file"
        } else if raw.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if raw.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if raw.starts_with("old mode") || raw.starts_with("new mode") {
            file.mode_change = true;
        } else if let Some(from) = raw.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(unquote(from));
        } else if let Some(to) = raw.strip_prefix("rename to ") {
            file.path = unquote(to);
        } else if raw.starts_with("Binary files ") || raw == "GIT binary patch" {
            file.binary = true;
        } else if let Some(path) = raw.strip_prefix("+++ ")
            && let Some(path) = side_path(path, "b/")
        {
            // a deletion's `+++` is /dev/null: the header's path stands
            file.path = path;
        }
    }
    Patch {
        files,
        ..Patch::default()
    }
}

fn push_line(file: &mut FileDiff, kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) {
    if let Some(hunk) = file.hunks.last_mut() {
        hunk.lines.push(Line {
            kind,
            old,
            new,
            text: text.to_string(),
        });
    }
}

/// `@@ -12,5 +12,7 @@` → (12, 12).
fn hunk_starts(header: &str) -> (u32, u32) {
    let mut old = 0;
    let mut new = 0;
    for part in header.split_whitespace() {
        let start = |spec: &str| {
            spec.split(',')
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0)
        };
        if let Some(spec) = part.strip_prefix('-') {
            old = start(spec);
        } else if let Some(spec) = part.strip_prefix('+') {
            new = start(spec);
        } else if part == "@@" && (old, new) != (0, 0) {
            break;
        }
    }
    (old, new)
}

/// The `b/` side of `diff --git a/x b/y`; the `---`/`+++` lines refine it.
fn git_header_path(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(at) = rest.find(" b/") {
        return unquote(&rest[at + 3..]);
    }
    unquote(rest.trim_start_matches("a/"))
}

/// `b/src/x.rs` → `src/x.rs`; `/dev/null` → none.
fn side_path(path: &str, prefix: &str) -> Option<String> {
    let path = path.trim_end_matches('\t');
    if path == "/dev/null" {
        return None;
    }
    let path = unquote(path);
    Some(path.strip_prefix(prefix).unwrap_or(&path).to_string())
}

/// Git quotes paths with unusual characters (`"a\tb.rs"`); undo the quotes
/// and the common escapes.
fn unquote(path: &str) -> String {
    let path = path.trim();
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.to_string();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}
