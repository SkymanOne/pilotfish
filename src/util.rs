//! Shared helpers: ids, timestamps, atomic writes, JSONL framing.
//!
//! Ported from the TypeScript `src/util.ts`; the behaviours its tests pin down
//! (framing across chunk boundaries, offsets that never split a line, atomic
//! rename) carry over unchanged.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use uuid::Uuid;

/// Worker branches are cut as `<prefix>/<name>-<last 7 of the run id>`.
pub const BRANCH_PREFIX: &str = "parl";

/// Result of [`split_json_lines`]: complete lines plus the unfinished tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitResult {
    pub lines: Vec<String>,
    pub rest: String,
}

/// Strict JSONL framing: split on `\n` only, strip one trailing `\r`.
/// A chunk boundary may fall anywhere; the partial tail comes back as `rest`
/// and is prepended to the next chunk.
pub fn split_json_lines(chunk: &str, prev_rest: &str) -> SplitResult {
    let buffer = format!("{prev_rest}{chunk}");
    let mut lines = Vec::new();
    let mut rest = buffer.as_str();
    while let Some(idx) = rest.find('\n') {
        let mut line = &rest[..idx];
        rest = &rest[idx + 1..];
        if let Some(stripped) = line.strip_suffix('\r') {
            line = stripped;
        }
        lines.push(line.to_string());
    }
    SplitResult {
        lines,
        rest: rest.to_string(),
    }
}

/// Monotonic-per-process sequence so overlapping writes from one process
/// never share a temp path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Serialize `value` as pretty JSON, write it to `<path>.tmp-<pid>-<n>` in the
/// same directory, fsync, and rename over `path`. Readers see either the old
/// file or the new one, never a half-written one.
///
/// # Errors
///
/// Returns `std::io::Error` when serialization fails, or when the write,
/// fsync or rename fails.
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        "{}.tmp-{}-{seq}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    let mut file = File::create(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Append one JSON line. A single small `write` under O_APPEND keeps
/// concurrent appenders from interleaving mid-line.
///
/// # Errors
///
/// Returns `std::io::Error` when serialization or the append fails.
pub fn append_json_line<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let line = serde_json::to_string(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    append_text(path, &format!("{line}\n"))
}

/// Append raw text, creating the file when missing.
///
/// # Errors
///
/// Returns `std::io::Error` when the file cannot be opened or written.
pub fn append_text(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())
}

/// Newest-last tail of a JSONL file; unparsable lines are skipped silently.
/// A missing file reads as empty — logs are optional by nature.
pub fn read_jsonl_tail<T: DeserializeOwned>(path: &Path, n: usize) -> Vec<T> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = raw.split('\n').filter(|l| !l.is_empty()).collect();
    lines
        .iter()
        .rev()
        .take(n)
        .rev()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Last `n` lines of a text file, without the trailing newline.
pub fn tail_text(path: &Path, n_lines: usize) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let body = raw.strip_suffix('\n').unwrap_or(&raw);
    if body.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = body.split('\n').collect();
    let start = lines.len().saturating_sub(n_lines);
    lines[start..].join("\n")
}

/// Current time as RFC3339 UTC with exactly millisecond precision
/// (`2026-08-30T12:00:00.000Z`), matching the old JS `toISOString()`.
pub fn now_iso() -> String {
    iso_at(OffsetDateTime::now_utc())
}

/// Same format, for a fixed instant (tests, reproducible envelopes).
pub fn iso_at(at: OffsetDateTime) -> String {
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    // Rendering a UTC datetime with this description cannot fail.
    at.format(&fmt).unwrap_or_default()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse an RFC3339 timestamp into milliseconds since the epoch.
pub fn parse_ts_ms(ts: &str) -> Option<i64> {
    OffsetDateTime::parse(ts, &Rfc3339)
        .ok()
        .map(|dt| (dt.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// How long ago an RFC3339 timestamp was, or `None` when it does not parse
/// or lies in the future (a clock that jumped should read as "unknown age",
/// not as a negative one).
#[must_use]
pub fn age_of(ts: &str) -> Option<std::time::Duration> {
    let then = parse_ts_ms(ts)?;
    let millis = now_ms().checked_sub(then)?;
    u64::try_from(millis)
        .ok()
        .map(std::time::Duration::from_millis)
}

/// The nil uuid, as the serde default for identity fields: a state file
/// written before the field existed must read as the same degenerate
/// identity every time. (`Uuid::default()` is a *random* v4 under the v4
/// feature — the wrong default for persistence.)
#[must_use]
pub fn nil_uuid() -> Uuid {
    Uuid::nil()
}

/// Run id for a worker started now: `<name>-<last 7 hex chars of the uuid>`.
/// The uuid is the run's identity; the dir and branch names stay human
/// readable (`ls` shows the alias, not hex soup).
pub fn run_id_for(name: &str, uuid: &Uuid) -> String {
    format!("{name}-{}", short_uuid(uuid))
}

/// The last 7 hex characters of a uuid — the suffix of run ids, session
/// dirs and branch names.
pub fn short_uuid(uuid: &Uuid) -> String {
    let simple = uuid.simple().to_string();
    simple[simple.len().saturating_sub(7)..].to_string()
}

/// Last 7 characters of a run id, as used in branch names. Under the uuid
/// scheme a run id is `<name>-<short_uuid>`, so this is the short uuid
/// itself; legacy `<name>-<14-digit>` ids keep working here too.
pub fn short7(run_id: &str) -> &str {
    let start = run_id.len().saturating_sub(7);
    &run_id[start..]
}

/// The worker's branch: `parl/<name>-<short7>`.
pub fn branch_for(name: &str, run_id: &str) -> String {
    format!("{BRANCH_PREFIX}/{name}-{}", short7(run_id))
}

/// Text up to the first newline.
pub fn first_line(s: &str) -> &str {
    s.find('\n').map_or(s, |idx| &s[..idx])
}

/// One line of agent-controlled text as a terminal can safely show it.
///
/// Tool output is written by whatever the agent ran, and command-line tools
/// draw progress with control characters: `git rebase` emits
/// `Rebasing (1/6)\rRebasing (2/6)\r…\rSuccessfully rebased and updated …`.
/// Carriage returns are applied the way a terminal would — the last segment
/// is what the sequence finally said. Escape sequences are removed whole,
/// rather than having their `ESC` spaced out and their payload left behind
/// as literal `[31m` litter. Every control character that survives becomes a
/// space, so agent output can never move the cursor, repaint a row, or start
/// a sequence of its own.
///
/// Newlines are the caller's business: split first, then call this per line.
#[must_use]
pub fn visible_line(text: &str) -> String {
    let settled = text.rsplit('\r').find(|part| !part.is_empty());
    strip_escapes(settled.unwrap_or(""))
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Drop ANSI escape sequences whole. CSI (`ESC [ … final`) and OSC
/// (`ESC ] … BEL`, or `ESC ] … ESC \`) are the ones tools actually emit;
/// anything else introduced by `ESC` drops the `ESC` and its single
/// following byte. An unterminated sequence swallows the rest of the line,
/// which is what a terminal would do with it too.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&ch) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or to the two-byte string terminator
            Some(']') => {
                let mut prev_esc = false;
                for ch in chars.by_ref() {
                    if ch == '\u{7}' || (prev_esc && ch == '\\') {
                        break;
                    }
                    prev_esc = ch == '\u{1b}';
                }
            }
            // anything else is a two-character escape; both go
            Some(_) | None => {}
        }
    }
    out
}

/// Compact human age: `30s`, `5m`, `2h`, `3d`.
pub fn format_age(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", ms.div_euclid(1000))
    } else if ms < 3_600_000 {
        format!("{}m", ms.div_euclid(60_000))
    } else if ms < 86_400_000 {
        format!("{}h", ms.div_euclid(3_600_000))
    } else {
        format!("{}d", ms.div_euclid(86_400_000))
    }
}

/// Run/branch-safe name: lowercase kebab-case, no leading/trailing hyphens.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    out
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// [`base36`] left-padded with zeros to exactly six characters.
///
/// `{:0>6}`, not `{:06}`: the numeric zero-pad flag only works on integer
/// types, and `Display for str` renders a width as left-aligned *space*
/// padding — trailing spaces on ~1 id in 36.
fn base36_6(value: u64) -> String {
    format!("{:0>6}", base36(value))
}

/// Short unique id: `<prefix>_<time36>_<random6>`; sortable enough for logs.
pub fn new_id(prefix: &str) -> String {
    let millis = now_ms().max(0) as u64;
    let random: u64 = rand::random();
    format!(
        "{prefix}_{}_{}",
        base36(millis),
        base36_6(random % 36u64.pow(6))
    )
}

/// Read the complete lines appended to `path` after byte `offset`.
///
/// The returned offset sits just past the last `\n` seen, so a partial
/// trailing line is re-read on the next call instead of being split. An
/// offset produced here is always a UTF-8 char boundary (`\n` cannot appear
/// inside a multi-byte sequence), so the lossy decode below only ever kicks
/// in for files that were not valid UTF-8 to begin with.
pub fn read_new_lines(path: &Path, offset: u64) -> (Vec<String>, u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (Vec::new(), offset);
    };
    let size = meta.len();
    if size <= offset {
        return (Vec::new(), offset);
    }
    let mut buf = vec![0u8; (size - offset) as usize];
    let Ok(mut file) = File::open(path) else {
        return (Vec::new(), offset);
    };
    if file.seek(SeekFrom::Start(offset)).is_err() || file.read_exact(&mut buf).is_err() {
        // Shrank between stat and read; treat as nothing new this round.
        return (Vec::new(), offset);
    }
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return (Vec::new(), offset);
    };
    let text = String::from_utf8_lossy(&buf[..=last_nl]);
    let result = split_json_lines(&text, "");
    (
        result.lines.into_iter().filter(|l| !l.is_empty()).collect(),
        offset + last_nl as u64 + 1,
    )
}

#[cfg(test)]
mod visible_line_tests {
    use super::visible_line;

    #[test]
    fn carriage_returns_resolve_the_way_a_terminal_shows_them() {
        // the real `git rebase` output that tore the console's frame
        assert_eq!(
            visible_line(
                "Rebasing (1/6)\rRebasing (2/6)\rRebasing (6/6)\rSuccessfully rebased and updated refs/heads/feat/escrow."
            ),
            "Successfully rebased and updated refs/heads/feat/escrow.",
            "progress collapses to what it finally said"
        );
        assert_eq!(
            visible_line("done\r"),
            "done",
            "a trailing CR is not a line"
        );
        assert_eq!(visible_line("plain"), "plain");
        assert_eq!(visible_line(""), "");
        assert_eq!(visible_line("\r"), "");
    }

    #[test]
    fn every_other_control_character_becomes_a_space() {
        assert_eq!(visible_line("a\tb"), "a b", "a tab would jump a tab stop");
        assert_eq!(
            visible_line("\x1b[31mred\x1b[0m"),
            "red",
            "a colour sequence goes whole, payload included"
        );
        assert_eq!(visible_line("a\x08b"), "a b");
        assert_eq!(visible_line("bell\x07"), "bell ");
        let out = visible_line("ok\u{9b}[2J");
        assert!(
            !out.chars().any(char::is_control),
            "nothing controlling survives: {out:?}"
        );
    }

    #[test]
    fn escape_sequences_go_whole_rather_than_leaving_their_payload() {
        // the litter this replaces: `ESC` spaced out, `[31m` left as text
        assert_eq!(
            visible_line("\x1b[1;32mPASS\x1b[0m 12 tests"),
            "PASS 12 tests"
        );
        assert_eq!(visible_line("\x1b[2K\x1b[1Gbuilding"), "building");
        // OSC 0 (set window title), both terminators
        assert_eq!(visible_line("\x1b]0;a title\x07after"), "after");
        assert_eq!(visible_line("\x1b]0;a title\x1b\\after"), "after");
        // a two-character escape takes its second byte with it
        assert_eq!(visible_line("a\x1b=b"), "ab");
        // unterminated: the rest of the line goes, as a terminal would
        assert_eq!(visible_line("keep\x1b[31"), "keep");
        assert_eq!(visible_line("\x1b"), "");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for text in ["héllo · wörld", "日本語のテキスト", "→ ⚙ ↳ ▸ ▍"] {
            assert_eq!(visible_line(text), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn split_json_lines_strict_framing_across_chunk_boundaries() {
        let payload = "{\"a\":\"xy\"}\n{\"b\":\"c\"}\r\n";
        let mut rest = String::new();
        let mut acc = Vec::new();
        for chunk in [&payload[..7], &payload[7..]] {
            let mut r = split_json_lines(chunk, &rest);
            acc.append(&mut r.lines);
            rest = r.rest;
        }
        assert_eq!(acc, vec!["{\"a\":\"xy\"}", "{\"b\":\"c\"}"]);
        assert_eq!(rest, "");
    }

    #[test]
    fn split_json_lines_u2028_is_not_a_delimiter() {
        let r = split_json_lines("{\"a\":\"x\u{2028}y\"}\n", "");
        assert_eq!(r.lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&r.lines[0]).unwrap();
        assert_eq!(parsed["a"], "x\u{2028}y");
    }

    #[test]
    fn split_json_lines_keeps_incomplete_tail() {
        let r = split_json_lines("{\"a\":1}\n{\"b\":", "");
        assert_eq!(r.lines, vec!["{\"a\":1}"]);
        assert_eq!(r.rest, "{\"b\":");
    }

    #[test]
    fn atomic_write_json_round_trips_without_tmp_files() {
        let dir = tmp_dir("parl-util-");
        let path = dir.join("state.json");
        atomic_write_json(&path, &serde_json::json!({ "a": 1 })).unwrap();
        atomic_write_json(&path, &serde_json::json!({ "a": 2 })).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
            serde_json::json!({ "a": 2 })
        );
        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["state.json"]);
    }

    #[test]
    fn append_json_line_then_read_jsonl_tail_returns_newest_last_slice() {
        let dir = tmp_dir("parl-util-");
        let path = dir.join("events.jsonl");
        for i in 0..5 {
            append_json_line(&path, &serde_json::json!({ "i": i })).unwrap();
        }
        let tail: Vec<serde_json::Value> = read_jsonl_tail(&path, 3);
        let got: Vec<i64> = tail.iter().map(|v| v["i"].as_i64().unwrap()).collect();
        assert_eq!(got, vec![2, 3, 4]);
    }

    #[test]
    fn run_id_and_branch_formats_are_utc() {
        let uuid = Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap();
        let id = run_id_for("auth-worker", &uuid);
        assert_eq!(id, "auth-worker-f7a8b9c");
        assert_eq!(short7(&id), "f7a8b9c");
        assert_eq!(short_uuid(&uuid), "f7a8b9c");
        assert_eq!(branch_for("auth-worker", &id), "parl/auth-worker-f7a8b9c");
        // A legacy 14-digit run id still shortens to seven characters, so
        // the branch rule needs no special case for what is on disk.
        let legacy = "auth-20260828141530";
        assert_eq!(short7(legacy), "8141530");
        assert_eq!(branch_for("auth", legacy), "parl/auth-8141530");
        assert_eq!(first_line("a\nb"), "a");
        assert_eq!(first_line("solo"), "solo");
    }

    #[test]
    fn now_iso_has_millisecond_precision() {
        let ts = now_iso();
        assert_eq!(ts.len(), 24, "{ts}");
        assert!(ts.ends_with('Z'), "{ts}");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
        assert!(parse_ts_ms(&ts).is_some());
    }

    #[test]
    fn format_age_renders_compact_ages() {
        assert_eq!(format_age(30_000), "30s");
        assert_eq!(format_age(5 * 60_000), "5m");
        assert_eq!(format_age(125 * 60_000), "2h");
        assert_eq!(format_age(3 * 86_400_000), "3d");
    }

    #[test]
    fn sanitize_name_kebab_cases() {
        assert_eq!(sanitize_name("Auth Worker 2!"), "auth-worker-2");
        assert_eq!(sanitize_name("--x--"), "x");
        assert_eq!(sanitize_name("über"), "ber");
    }

    #[test]
    fn new_id_has_prefix_time_and_random_parts() {
        let id = new_id("m");
        let mut parts = id.split('_');
        assert_eq!(parts.next(), Some("m"));
        let time = parts.next().unwrap();
        let rand = parts.next().unwrap();
        assert!(!time.is_empty());
        assert_eq!(rand.len(), 6);
        assert!(id.starts_with("m_"));
    }

    #[test]
    fn base36_six_zero_fills_short_values_on_the_left() {
        // The values whose base36 form is shorter than six characters —
        // the ~2.78% of draws that `{:06}` padded with trailing spaces.
        assert_eq!(base36_6(0), "000000");
        assert_eq!(base36_6(9), "000009");
        assert_eq!(base36_6(35), "00000z");
        assert_eq!(base36_6(36), "000010");
        assert_eq!(base36_6(36u64.pow(5) - 1), "0zzzzz");
        assert_eq!(base36_6(36u64.pow(5)), "100000");
        assert_eq!(base36_6(36u64.pow(6) - 1), "zzzzzz");
    }

    #[test]
    fn new_id_stays_whitespace_free_across_thousands_of_draws() {
        // `{:06}` left-aligned the short draws with spaces, and the shape
        // test above passed right through it (trailing spaces kept
        // `rand.len() == 6` true). Thousands of draws are certain to land
        // in that ~2.78% bucket, so the real generator is pinned too.
        for prefix in ["m", "ev", "t"] {
            for _ in 0..2_500 {
                let id = new_id(prefix);
                assert!(id.chars().all(|c| !c.is_whitespace()), "{id:?}");
                let mut parts = id.split('_');
                assert_eq!(parts.next(), Some(prefix), "{id:?}");
                let time = parts.next().unwrap();
                let rand = parts.next().unwrap();
                assert!(parts.next().is_none(), "{id:?}");
                assert!(
                    !time.is_empty()
                        && time
                            .bytes()
                            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
                    "{id:?}"
                );
                assert_eq!(rand.len(), 6, "{id:?}");
                assert!(
                    rand.bytes()
                        .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
                    "{id:?}"
                );
            }
        }
    }

    #[test]
    fn read_new_lines_advances_only_past_complete_lines() {
        let dir = tmp_dir("parl-util-");
        let path = dir.join("inbox.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"b\":").unwrap();
        let (lines, offset) = read_new_lines(&path, 0);
        assert_eq!(lines, vec!["{\"a\":1}"]);
        assert_eq!(offset, 8);
        // Multi-byte character split across calls must survive the boundary.
        append_text(&path, "\"é\"}\r\n").unwrap();
        let (lines, offset2) = read_new_lines(&path, offset);
        assert_eq!(lines, vec!["{\"b\":\"é\"}"]);
        assert_eq!(offset2, std::fs::metadata(&path).unwrap().len());
        let (lines, offset3) = read_new_lines(&path, offset2);
        assert!(lines.is_empty());
        assert_eq!(offset3, offset2);
        let (lines, offset4) = read_new_lines(&dir.join("missing.jsonl"), 0);
        assert!(lines.is_empty());
        assert_eq!(offset4, 0);
    }

    #[test]
    fn tail_text_ignores_the_trailing_newline() {
        let dir = tmp_dir("parl-util-");
        let path = dir.join("rpc.log");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        assert_eq!(tail_text(&path, 2), "b\nc");
        assert_eq!(tail_text(&path, 10), "a\nb\nc");
        std::fs::write(&path, "a\nb").unwrap();
        assert_eq!(tail_text(&path, 1), "b");
        assert_eq!(tail_text(&dir.join("missing"), 3), "");
    }
}
