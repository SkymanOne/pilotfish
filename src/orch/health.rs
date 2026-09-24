//! Orchestrator health: is the `claude` we are about to drive a version this
//! app was tested against, and is there an orphaned one left over from a
//! console that crashed?
//!
//! Ported from the TypeScript `src/orchestrator/health.ts`.

use std::process::Command;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::fleet::run::is_alive;
use crate::orch::args::claude_command_from_spec;
use crate::paths::{SessionKey, env_var};

/// Claude Code versions whose stream-json protocol this app was verified
/// against: 2.1.x up to but not including 2.2.
pub const TESTED_CLAUDE_RANGE: ((u64, u64), (u64, u64)) = ((2, 1), (2, 2));

/// The command-line marker [`reap_orphan_orchestrator`] looks for in a
/// monitored pid, built per session: `--session <uuid>`, which only that
/// session's own monitor carries. A bare `"claude"` had matched every
/// session's process at once — with N monitors a stale pid recycled onto
/// another session's healthy monitor would have been killed.
#[must_use]
pub fn orphan_matcher_for(key: &SessionKey) -> String {
    format!("--session {}", key.uuid)
}

/// What the version check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionCheck {
    pub version: Option<String>,
    pub supported: bool,
    pub warning: Option<String>,
}

/// Extract the first `major.minor.patch` from a `claude --version` output.
#[must_use]
pub fn parse_claude_version(output: &str) -> Option<String> {
    let regex = regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").ok()?;
    regex
        .captures(output)
        .and_then(|caps| caps.get(0))
        .map(|matched| matched.as_str().to_string())
}

/// Whether the version falls inside the tested range. None is not. Never
/// fatal either way: an unsupported version still runs, it just says so.
#[must_use]
pub fn version_supported(version: Option<&str>) -> bool {
    let Some(version) = version else {
        return false;
    };
    let mut parts = version.split('.');
    let (Some(Ok(major)), Some(Ok(minor))) = (
        parts.next().map(str::parse::<u64>),
        parts.next().map(str::parse::<u64>),
    ) else {
        return false;
    };
    let ((min_major, min_minor), (max_major, max_minor)) = TESTED_CLAUDE_RANGE;
    if major < min_major || (major == min_major && minor < min_minor) {
        return false;
    }
    major < max_major || (major == max_major && minor < max_minor)
}

/// `claude --version` against the real environment.
#[must_use]
pub fn check_claude_version() -> VersionCheck {
    check_claude_version_with_spec(std::env::var(env_var("CLAUDE_BIN")).ok().as_deref())
}

/// [`check_claude_version`] against an explicit binary spec (tests).
#[must_use]
pub fn check_claude_version_with_spec(spec: Option<&str>) -> VersionCheck {
    let (bin, prefix) = claude_command_from_spec(spec);
    let output = Command::new(&bin).args(&prefix).arg("--version").output();
    let Ok(output) = output else {
        return VersionCheck {
            version: None,
            supported: false,
            warning: Some(format!(
                "could not run `{bin} --version` — is Claude Code installed and on your PATH?"
            )),
        };
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = parse_claude_version(&text);
    match version {
        None => VersionCheck {
            version: None,
            supported: false,
            warning: Some(format!(
                "could not read a version from `{bin} --version` — continuing anyway"
            )),
        },
        Some(version) if version_supported(Some(&version)) => VersionCheck {
            version: Some(version),
            supported: true,
            warning: None,
        },
        Some(version) => {
            let ((min_major, min_minor), (max_major, max_minor)) = TESTED_CLAUDE_RANGE;
            VersionCheck {
                version: Some(version.clone()),
                supported: false,
                warning: Some(format!(
                    "Claude Code {version} is outside the tested range \
                     ({min_major}.{min_minor}.x up to but not including {max_major}.{max_minor}) \
                     — the stream-json protocol may have changed"
                )),
            }
        }
    }
}

/// A process's command line (`ps -p <pid> -o command=`), or none when it
/// cannot be read (missing, or not POSIX).
#[must_use]
pub fn command_line_of(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// A process's start time as epoch seconds, read from `ps -o lstart=`
/// (the same format on macOS and procps Linux: `Sun Aug 30 12:00:00 2026`).
/// None when it cannot be read (missing pid) or parsed (a non-C locale
/// spells months in another language). Second resolution is enough: a
/// pid recycled within the same second — and bearing the same session's
/// matcher — is below the noise this guard exists for.
#[must_use]
pub fn process_started_at(pid: i32) -> Option<i64> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if line.is_empty() {
        None
    } else {
        lstart_epoch(&line)
    }
}

/// Parse a `ps -o lstart=` line into epoch seconds. The two platforms the
/// reaper cares about spell it differently — macOS prints `Sun 30 Aug
/// 16:29:24 2026` (day before month), procps Linux `Sun Aug 30 16:29:24
/// 2026` — so the tokens are identified by content, not position. The
/// weekday name is not needed for a timestamp and is skipped.
#[must_use]
fn lstart_epoch(lstart: &str) -> Option<i64> {
    let mut parts = lstart.split_whitespace();
    // (ignore the weekday)
    parts.next()?;
    // The second token is the day on macOS, the month on Linux.
    let second = parts.next()?;
    let (month, day) = if second.bytes().all(|b| b.is_ascii_digit()) {
        (parts.next()?, second)
    } else {
        (second, parts.next()?)
    };
    let clock = parts.next()?;
    let year = parts.next()?;
    let month_num = match month {
        "Jan" => 1_u8,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: u8 = day.trim().parse().ok()?;
    let mut clock_parts = clock.split(':');
    let (hour, minute, second_of_minute) = (
        clock_parts.next()?.parse::<u8>().ok()?,
        clock_parts.next()?.parse::<u8>().ok()?,
        clock_parts.next()?.parse::<u8>().ok()?,
    );
    let month: time::Month = time::Month::try_from(month_num).ok()?;
    let year: i32 = year.parse().ok()?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let time = time::Time::from_hms(hour, minute, second_of_minute).ok()?;
    Some(
        time::PrimitiveDateTime::new(date, time)
            .assume_utc()
            .unix_timestamp(),
    )
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapResult {
    pub reaped: bool,
    pub reason: Option<String>,
}

/// Kill a monitor left behind by a console that died. Only a live pid whose
/// command line still carries *this session's* marker is touched, and even
/// then only when its process started no later than the row recorded: pids
/// get reused, and killing a healthy session's process would be far worse
/// than leaving one behind. A row without a recorded start time skips the
/// second guard — the session-unique matcher still stands alone.
#[must_use]
pub fn reap_orphan_orchestrator(
    pid: Option<i32>,
    matcher: &str,
    started_at: Option<i64>,
) -> ReapResult {
    if !is_alive(pid) {
        return ReapResult {
            reaped: false,
            reason: None,
        };
    }
    let Some(pid) = pid else {
        return ReapResult {
            reaped: false,
            reason: None,
        };
    };
    let Some(command) = command_line_of(pid) else {
        return ReapResult {
            reaped: false,
            reason: Some(format!(
                "pid {pid} is alive but its command line could not be read — leaving it alone"
            )),
        };
    };
    if !command.contains(matcher) {
        return ReapResult {
            reaped: false,
            reason: Some(format!(
                "pid {pid} is alive but is not a \"{matcher}\" process — leaving it alone"
            )),
        };
    }
    // The row's own pid-and-start snapshot: a pid that now hosts a process
    // started *after* the recording is a recycled pid, not the orphan.
    if let Some(recorded) = started_at
        && let Some(current) = process_started_at(pid)
        && current > recorded
    {
        return ReapResult {
            reaped: false,
            reason: Some(format!(
                "pid {pid} matches the session but started after the recorded start time — \
                 it is a recycled pid, not the orphan; leaving it alone"
            )),
        };
    }
    match kill(Pid::from_raw(pid), Signal::SIGTERM) {
        Ok(()) => ReapResult {
            reaped: true,
            reason: Some(format!(
                "stopped an orphaned orchestrator (pid {pid}) left by an earlier console"
            )),
        },
        Err(_) => ReapResult {
            reaped: false,
            reason: Some(format!("pid {pid} could not be signalled")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_version_check() {
        {
            assert_eq!(
                parse_claude_version("2.1.251 (Claude Code)"),
                Some("2.1.251".to_string())
            );
            assert_eq!(parse_claude_version("no version here"), None);
            assert_eq!(TESTED_CLAUDE_RANGE.0, (2, 1));
            assert!(version_supported(Some("2.1.0")));
            assert!(version_supported(Some("2.1.251")));
            assert!(!version_supported(Some("2.0.9")));
            assert!(!version_supported(Some("2.2.0")));
            assert!(!version_supported(Some("3.0.0")));
            assert!(!version_supported(None));
        }
        {
            let (_dir, spec) = version_spec("2.1.251");
            let check = check_claude_version_with_spec(Some(&spec));
            assert_eq!(check.version.as_deref(), Some("2.1.251"));
            assert!(check.supported);
            assert_eq!(check.warning, None);
        }
        {
            let (_dir, spec) = version_spec("2.0.9");
            let check = check_claude_version_with_spec(Some(&spec));
            assert_eq!(check.version.as_deref(), Some("2.0.9"));
            assert!(!check.supported);
            let warning = check.warning.expect("a warning names the range");
            assert!(warning.contains("outside the tested range"), "{warning}");
        }
        {
            let check = check_claude_version_with_spec(Some("/nonexistent/claude-binary"));
            assert_eq!(check.version, None);
            assert!(!check.supported);
            let warning = check
                .warning
                .expect("the warning explains what to look for");
            assert!(warning.contains("could not run"), "{warning}");
        }
    }

    /// A `claude --version` stand-in that prints a fixed version.
    fn version_spec(version: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude-version.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"{version} (Claude Code)\"\n"),
        )
        .unwrap();
        let spec = format!("sh {}", script.display());
        (dir, spec)
    }

    #[test]
    fn orphan_reaper() {
        {
            // none and a dead pid: nothing to do, nothing to say
            assert_eq!(
                reap_orphan_orchestrator(None, "--session anything", None),
                ReapResult {
                    reaped: false,
                    reason: None
                }
            );
            assert_eq!(
                reap_orphan_orchestrator(Some(999_999_999), "--session anything", None),
                ReapResult {
                    reaped: false,
                    reason: None
                }
            );

            // our own process is alive but does not look like the child: this
            // repo path contains "claude", so the match would be fatal — the
            // matcher is the caller's contract, and it must be precise
            let own_pid = i32::try_from(std::process::id()).unwrap_or(1);
            let not_the_child =
                reap_orphan_orchestrator(Some(own_pid), "--session never-mine", None);
            assert!(!not_the_child.reaped);
            let reason = not_the_child
                .reason
                .expect("it explains why it left it alone");
            assert!(reason.contains("is not a"), "{reason}");
            assert!(command_line_of(own_pid).is_some());
            assert_eq!(command_line_of(999_999_999), None);
        }
        {
            // A's stale row recorded the pid with A's matcher; the OS recycled
            // it onto session B's process. B's command line carries B's own
            // `--session` marker, so A's matcher cannot match it — the reaper
            // must leave B alone even though it is alive.
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep is available");
            let pid = i32::try_from(child.id()).unwrap_or(1);
            let session_a = SessionKey::new(Some("a".into()), uuid::Uuid::new_v4());
            let matcher = orphan_matcher_for(&session_a);
            assert!(matcher.starts_with("--session "), "{matcher}");
            let started = process_started_at(pid).expect("the process start is readable");
            let result = reap_orphan_orchestrator(Some(pid), &matcher, Some(started));
            assert!(!result.reaped, "{result:?}");
            let reason = result.reason.expect("it explains why it left it alone");
            assert!(
                reason.contains("is not a") && reason.contains("--session"),
                "{reason}"
            );
            assert!(is_alive(Some(pid)), "session B's process was not touched");
            let _ = child.kill();
            let _ = child.wait();
        }
        {
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep is available");
            let pid = i32::try_from(child.id()).unwrap_or(1);
            // The stale row remembers a start time from before this process
            // existed — the classic recycled-pid shape.
            let now = crate::util::now_ms() / 1_000;
            let recorded = now - 60;
            let result = reap_orphan_orchestrator(Some(pid), "sleep", Some(recorded));
            assert!(!result.reaped, "{result:?}");
            let reason = result.reason.expect("it says why it left it alone");
            assert!(reason.contains("recycled pid"), "{reason}");
            assert!(is_alive(Some(pid)), "the recycled occupant survives");
            let _ = child.kill();
            let _ = child.wait();
        }
        {
            // The row's own snapshot: pid, session matcher and the start time
            // recorded when the monitor booted. The process is untouched since,
            // so the reaper recognises it as the very orphan it was left with.
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sleep is available");
            let pid = i32::try_from(child.id()).unwrap_or(1);
            let started = process_started_at(pid).expect("the process start is readable");
            let result = reap_orphan_orchestrator(Some(pid), "sleep", Some(started));
            assert!(result.reaped, "{result:?}");
            let reason = result.reason.expect("it says what it stopped");
            assert!(
                reason.contains("stopped an orphaned orchestrator"),
                "{reason}"
            );
            let _ = child.wait();
            assert!(!is_alive(Some(pid)), "the orphan was terminated");
        }
        {
            use time::macros::{date, time as clock_time};
            let expected =
                time::PrimitiveDateTime::new(date!(2026 - 08 - 30), clock_time!(12:00:00))
                    .assume_utc()
                    .unix_timestamp();
            // procps Linux order: month before the day
            assert_eq!(lstart_epoch("Sun Aug 30 12:00:00 2026"), Some(expected));
            // macOS/BSD order: day before the month
            assert_eq!(lstart_epoch("Sun 30 Aug 12:00:00 2026"), Some(expected));
            // single-digit days are space-padded on both
            let expected =
                time::PrimitiveDateTime::new(date!(2026 - 08 - 05), clock_time!(09:07:01))
                    .assume_utc()
                    .unix_timestamp();
            assert_eq!(lstart_epoch("Wed Aug  5 09:07:01 2026"), Some(expected));
            assert_eq!(lstart_epoch("Wed  5 Aug 09:07:01 2026"), Some(expected));
            assert_eq!(lstart_epoch("not a time"), None);
            assert_eq!(lstart_epoch(""), None);
            // The real ps on this machine speaks one of the two dialects.
            let own = i32::try_from(std::process::id()).unwrap_or(1);
            let started = process_started_at(own).expect("our own lstart parses");
            assert!(started > 1_700_000_000, "{started}");
        }
    }
}
