//! The durable session store: `.parl/fleet.json` holds every orchestrator
//! session, keyed by session uuid. The console and the watcher keep their
//! cursors here — they are per-conversation, so each session owns its own
//! [`WatcherState`] — and the orchestrator keeps its claude session id, so
//! reopening the console picks the same conversation back up.
//!
//! Ported from the TypeScript `src/orchestrator/session.ts` (which stored
//! `orchestrator.json`) onto the one `fleet.json` the new layout defines.
//! One Rust-specific addition: the CLI hands the monitor nothing but
//! `--fleet-dir`, so the console records the launch flags here under
//! [`LaunchOptions`] for the monitor it spawns — and the monitor writes mode
//! changes back, so a restarted monitor keeps running the way the last one
//! did.
//!
//! Tolerant serde, like every state file in this crate: unknown fields are
//! ignored, missing ones fall back to defaults, and a version this code does
//! not know is treated as no sessions at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::paths::{FleetPaths, SessionKey};
use crate::util::{atomic_write_json, now_iso};

/// The only version this code reads; anything else starts fresh. Version 2
/// is the sessions map (v1 held a single session object).
pub const SESSION_VERSION: u8 = 2;

/// One run's watcher cursor: how much of its `events.jsonl` is consumed, and
/// the last view reported (so terminal transitions are not re-reported).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RunCursor {
    /// Byte offset already consumed in the run's events file.
    pub events_offset: u64,
    /// Last derived view reported for this run, as its display name
    /// (`"running"`, `"blocked"`, `"settled"`, …); none before the first.
    pub last_view: Option<String>,
}

/// The watcher's cursors, saved so a resumed console does not replay history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct WatcherState {
    pub cursors: HashMap<String, RunCursor>,
}

/// Launch flags the console records for the monitor it spawns. `fresh` is a
/// one-shot instruction (start a new session) and clears once used; the rest
/// describe how to bring claude up, updated live by the monitor so a
/// restarted monitor keeps the mode and Remote Control state in force.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LaunchOptions {
    /// The model asked for at launch (claude's default when none). Most
    /// specific wins: the explicit flag, then this persisted record, then
    /// `~/.parl/config.toml`'s `[orchestrator] model`.
    pub model: Option<String>,
    pub budget_usd: Option<f64>,
    pub permission_mode: Option<String>,
    /// Remote Control name, `Some("")` for an automatic one, none for off.
    pub remote_control: Option<String>,
    pub fresh: Option<bool>,
    /// Turns before the monitor compacts the session's context. Resolved
    /// from `~/.parl/config.toml` by whoever launched the monitor, and
    /// recorded here so the monitor needs no user-config read of its own.
    /// `Some(0)` or `None` is off.
    pub auto_compact_turns: Option<u32>,
}

/// The claude session, as persisted in `fleet.json`, one per session row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OrchestratorSession {
    pub version: u8,
    /// This session's uuid — the map key, the `Party::Orchestrator`
    /// identity and the `orchestrators/<alias>-<short-uuid>/` directory
    /// suffix. `Uuid::nil()` only in rows written before the field existed
    /// (explicitly nil, never `Uuid::default()`: under the v4 feature that
    /// is a *random* uuid, so a legacy row would read as a fresh identity
    /// every load).
    #[serde(default = "crate::util::nil_uuid")]
    pub uuid: Uuid,
    /// The human handle; absent until a later stage derives one from the
    /// session's first prompt.
    pub alias: Option<String>,
    /// When the session's monitor last reported liveness; the monitor
    /// stamps it from its poll loop, so a row whose heartbeat stops while
    /// its pid is alive reads as a wedged monitor.
    pub last_heartbeat: Option<String>,
    /// The claude session id; none until the first child reports init.
    pub session_id: Option<String>,
    /// The monitor's pid while it runs; used to reap an orphan after a crash.
    pub pid: Option<i32>,
    /// The pid's process start time (epoch seconds), recorded when the
    /// monitor booted; the orphan reaper refuses a pid whose occupant
    /// started later, since that is a recycled pid, not the orphan.
    pub pid_started_at: Option<i64>,
    /// The model claude is running (what init reported, not what was asked for).
    pub model: Option<String>,
    pub claude_version: Option<String>,
    pub started_at: String,
    pub last_used_at: String,
    pub cwd: String,
    pub watcher: WatcherState,
    pub launch: LaunchOptions,
}

impl Default for OrchestratorSession {
    fn default() -> Self {
        Self::new("")
    }
}

impl OrchestratorSession {
    /// A fresh session record for a console opening in `cwd`.
    #[must_use]
    pub fn new(cwd: &str) -> Self {
        let now = now_iso();
        Self {
            version: SESSION_VERSION,
            uuid: Uuid::new_v4(),
            alias: None,
            last_heartbeat: None,
            session_id: None,
            pid: None,
            pid_started_at: None,
            model: None,
            claude_version: None,
            started_at: now.clone(),
            last_used_at: now,
            cwd: cwd.to_string(),
            watcher: WatcherState::default(),
            launch: LaunchOptions::default(),
        }
    }

    /// The session's layout key: its uuid and (optional) alias.
    #[must_use]
    pub fn key(&self) -> SessionKey {
        SessionKey::new(self.alias.clone(), self.uuid)
    }
}

/// Every orchestrator session of one fleet, as persisted in `fleet.json`
/// (`{"version":2,"sessions":{…}}`, keyed by session uuid).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FleetSessions {
    pub version: u8,
    pub sessions: HashMap<Uuid, OrchestratorSession>,
    /// Top-level keys this store does not model — the console's prefs under
    /// `"console"`, anything a newer writer adds — round-tripped untouched
    /// through load → mutate → save. `save` is the one serde boundary in
    /// this crate that would otherwise destroy what it does not know, and a
    /// newer writer must never destroy what an older reader wrote.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl FleetSessions {
    /// A fresh, empty store at the current version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: SESSION_VERSION,
            sessions: HashMap::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// The session used most recently, which is the one a console reopens
    /// and a monitor serves while nothing names a session explicitly;
    /// `None` when the map is empty.
    #[must_use]
    pub fn last_used(&self) -> Option<&OrchestratorSession> {
        self.sessions
            .values()
            .max_by_key(|s| crate::util::parse_ts_ms(&s.last_used_at).unwrap_or(0))
    }

    /// Insert (or replace) one session.
    pub fn upsert(&mut self, session: OrchestratorSession) {
        self.sessions.insert(session.uuid, session);
    }
}

/// `fleet.json` — there is no separate `orchestrator.json` any more.
#[must_use]
pub fn session_path(fleet_dir: &Path) -> PathBuf {
    FleetPaths::new(fleet_dir).fleet_json()
}

/// Load the session store, or none when the file is missing, unreadable, or
/// written by a version this code does not know (it starts fresh instead).
#[must_use]
pub fn load(fleet_dir: &Path) -> Option<FleetSessions> {
    let raw = std::fs::read_to_string(session_path(fleet_dir)).ok()?;
    let parsed: FleetSessions = serde_json::from_str(&raw).ok()?;
    (parsed.version == SESSION_VERSION).then_some(parsed)
}

/// The fleet's current session, when it has one: the most recently used row.
/// This is the session a console reopens and a monitor serves while nothing
/// names a session explicitly.
#[must_use]
pub fn resolve_session(fleet_dir: &Path) -> Option<OrchestratorSession> {
    load(fleet_dir).and_then(|store| store.last_used().cloned())
}

/// Persist the session store. Recency is the caller's business — `save`
/// does not stamp anything, so nothing a monitor writes can bump ownership.
///
/// # Errors
///
/// Returns an I/O error when the fleet directory cannot be created or the
/// session file cannot be written.
pub fn save(fleet_dir: &Path, store: &mut FleetSessions) -> std::io::Result<()> {
    store.version = SESSION_VERSION;
    let to_write = store.clone();
    if let Some(parent) = session_path(fleet_dir).parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_json(&session_path(fleet_dir), &to_write)
}

/// Every session of the fleet, most recently used first — what a console
/// lists when nothing names a session explicitly.
#[must_use]
pub fn list_sessions(fleet_dir: &Path) -> Vec<OrchestratorSession> {
    let mut sessions: Vec<OrchestratorSession> = load(fleet_dir)
        .map(|store| store.sessions.into_values().collect())
        .unwrap_or_default();
    sessions.sort_by(|a, b| {
        crate::util::parse_ts_ms(&b.last_used_at)
            .unwrap_or(0)
            .cmp(&crate::util::parse_ts_ms(&a.last_used_at).unwrap_or(0))
    });
    sessions
}

/// Create a fresh session row for a console opening in the fleet's repo
/// (cwd defaults to the directory holding the fleet), make it the most
/// recently used session, persist it, and return it.
///
/// # Errors
///
/// Returns an error when the store cannot be saved.
pub fn create_session(
    fleet_dir: &Path,
    alias: Option<&str>,
) -> anyhow::Result<OrchestratorSession> {
    let mut store = load(fleet_dir).unwrap_or_default();
    let cwd = fleet_dir
        .parent()
        .unwrap_or(fleet_dir)
        .to_string_lossy()
        .into_owned();
    let mut session = OrchestratorSession::new(&cwd);
    session.alias = alias.map(str::to_string);
    let created = session.clone();
    store.upsert(session);
    save(fleet_dir, &mut store).map_err(anyhow::Error::from)?;
    Ok(created)
}

/// Resolve `uuid-or-alias` to a session, the exact uuid first, then the
/// alias (sanitized both sides, so casing and punctuation are ignored).
/// An alias shared by several live sessions is an error naming the
/// candidates — never a silent pick, the same rule `find_run` applies.
///
/// [`session_by_key`] collapses the ambiguous case to `None` (it never
/// picks); callers that must tell "ambiguous" from "missing" use
/// [`resolve_session_by_key`].
#[must_use]
pub fn session_by_key(fleet_dir: &Path, key: &str) -> Option<OrchestratorSession> {
    resolve_session_by_key(fleet_dir, key).ok()
}

/// [`session_by_key`] that names the problem: the error carries the
/// candidate dirs and uuids when several live sessions share the alias.
///
/// # Errors
///
/// Returns an error when nothing matches `key`, or when several live
/// sessions share the alias (naming the candidates).
pub fn resolve_session_by_key(fleet_dir: &Path, key: &str) -> anyhow::Result<OrchestratorSession> {
    let store = load(fleet_dir).unwrap_or_default();
    let raw = key.trim();
    let sessions: Vec<OrchestratorSession> = store.sessions.into_values().collect();
    if let Ok(uuid) = Uuid::parse_str(raw)
        && let Some(session) = sessions.iter().find(|s| s.uuid == uuid)
    {
        return Ok(session.clone());
    }
    let wanted = crate::util::sanitize_name(raw);
    let by_alias: Vec<&OrchestratorSession> = sessions
        .iter()
        .filter(|s| {
            s.alias
                .as_deref()
                .map(crate::util::sanitize_name)
                .is_some_and(|alias| alias == wanted)
        })
        .collect();
    match by_alias.as_slice() {
        [] => Err(anyhow::anyhow!(
            "no orchestrator session named \"{raw}\" in {}",
            session_path(fleet_dir).display()
        )),
        [one] => Ok((*one).clone()),
        many => {
            let candidates = many
                .iter()
                .map(|s| format!("{} ({})", s.key().dir_name(), s.uuid))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "\"{raw}\" names several live sessions: {candidates} — \
                 use a session uuid to disambiguate"
            ))
        }
    }
}

/// Load-modify-save the session store under an exclusive lock on a stable
/// sidecar (`fleet.json.lock` — the store file itself is atomically
/// renamed by every write, so flocking it would lock a different inode on
/// each save). N monitors share this store: without the lock, one writer's
/// stale read can clobber another's row — losing a heartbeat, a pid, or a
/// `sessionId` until the next event rewrites it.
pub fn with_store_mutation<R>(
    fleet_dir: &Path,
    mutate: impl FnOnce(&mut FleetSessions) -> R,
) -> std::io::Result<R> {
    let path = session_path(fleet_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Locked file, never renamed; the layout module owns the main paths,
    // and this sidecar is a coordination detail of the store itself.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fleet_dir.join("fleet.json.lock"))?;
    lock_file.lock()?;
    let mut store = load(fleet_dir).unwrap_or_default();
    let result = mutate(&mut store);
    save(fleet_dir, &mut store)?;
    lock_file.unlock()?;
    Ok(result)
}

/// Stamp one session's `last_heartbeat` with now, the monitor's liveness
/// report. The monitor calls this from its poll loop; a row that vanished
/// in the meantime (its session was removed) is not an error — there is
/// nothing left to keep alive.
///
/// # Errors
///
/// Returns an I/O error when the store cannot be saved.
pub fn touch_heartbeat(fleet_dir: &Path, uuid: Uuid) -> std::io::Result<()> {
    with_store_mutation(fleet_dir, |store| {
        if let Some(session) = store.sessions.get_mut(&uuid) {
            session.last_heartbeat = Some(now_iso());
        }
    })
}

/// How healthy a session's monitor is, derived from its row the way runs
/// derive their view: pid liveness first, then heartbeat freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorHealth {
    /// A monitor owns the session and its heartbeat is fresh.
    Running,
    /// A monitor is alive but its heartbeat has stopped: it is wedged, and
    /// whatever it was driving has stalled with it.
    Wedged,
    /// No live monitor: none ever ran, or its pid is gone.
    Stopped,
}

/// How long a live monitor may go without a heartbeat before its session
/// reads as wedged. The monitor writes heartbeats on a 5 s cadence, so a
/// missed one recovers; two missed in a row put a row over this mark.
pub const HEARTBEAT_GRACE_MS: i64 = 15_000;

/// Derive a session's monitor health, named like [`resolve_session`]:
/// `liveness` is injected (tests fake it), `now_ms` is the clock. A row
/// with a live pid but no heartbeat yet is not judged — the monitor may
/// not have ticked; only a recorded heartbeat older than the grace marks
/// a wedge.
#[must_use]
pub fn monitor_health(
    session: &OrchestratorSession,
    liveness: impl Fn(Option<i32>) -> bool,
    now_ms: i64,
) -> MonitorHealth {
    if !liveness(session.pid) {
        return MonitorHealth::Stopped;
    }
    let Some(heartbeat) = &session.last_heartbeat else {
        return MonitorHealth::Running;
    };
    if crate::util::parse_ts_ms(heartbeat).is_some_and(|at| now_ms - at > HEARTBEAT_GRACE_MS) {
        MonitorHealth::Wedged
    } else {
        MonitorHealth::Running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_fleet(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_session_has_no_session_id_a_fresh_uuid_and_version_two() {
        let session = OrchestratorSession::new("/repo");
        assert_eq!(session.version, SESSION_VERSION);
        assert!(!session.uuid.is_nil(), "every new session gets an identity");
        assert_eq!(session.alias, None);
        assert_eq!(session.last_heartbeat, None);
        assert_eq!(session.session_id, None);
        assert_eq!(session.pid, None);
        assert_eq!(session.pid_started_at, None);
        assert_eq!(session.cwd, "/repo");
        assert_eq!(session.launch, LaunchOptions::default());
        assert!(session.watcher.cursors.is_empty());
        assert!(session.started_at.ends_with('Z'));
        // The layout key mirrors the session's identity.
        let key = session.key();
        assert_eq!(key.uuid, session.uuid);
        assert!(
            key.dir_name()
                .ends_with(&crate::util::short_uuid(&session.uuid))
        );
    }

    #[test]
    fn save_and_load_round_trip_a_map_of_sessions_with_cursors_and_launch() {
        let fleet = tmp_fleet("parl-session-");
        let mut store = FleetSessions::new();
        let mut session = OrchestratorSession::new("/repo");
        session.session_id = Some("sess-abc12345".into());
        session.pid = Some(4321);
        session.model = Some("claude-fable-5".into());
        session.claude_version = Some("2.1.251".into());
        session.watcher.cursors.insert(
            "auth-20260828141530".into(),
            RunCursor {
                events_offset: 128,
                last_view: Some("blocked".into()),
            },
        );
        session.launch = LaunchOptions {
            model: Some("fable".into()),
            budget_usd: Some(5.0),
            permission_mode: Some("acceptEdits".into()),
            remote_control: Some(String::new()),
            fresh: Some(true),
            auto_compact_turns: Some(40),
        };
        let session_uuid = session.uuid;
        store.upsert(session);
        // A second session shares the store; each keeps its own cursors.
        let mut other = OrchestratorSession::new("/elsewhere");
        other.watcher.cursors.insert(
            "db-9f8e7d6".into(),
            RunCursor {
                events_offset: 64,
                last_view: Some("running".into()),
            },
        );
        let other_uuid = other.uuid;
        store.upsert(other);
        save(&fleet, &mut store).unwrap();

        let loaded = load(&fleet).unwrap();
        assert_eq!(loaded.sessions.len(), 2);
        let session = &loaded.sessions[&session_uuid];
        assert_eq!(session.session_id.as_deref(), Some("sess-abc12345"));
        assert_eq!(session.pid, Some(4321));
        assert_eq!(session.model.as_deref(), Some("claude-fable-5"));
        let (run_id, cursor) = session.watcher.cursors.iter().next().unwrap();
        assert_eq!(run_id, "auth-20260828141530");
        assert_eq!(cursor.events_offset, 128);
        assert_eq!(cursor.last_view.as_deref(), Some("blocked"));
        assert_eq!(session.launch.budget_usd, Some(5.0));
        assert_eq!(
            session.launch.permission_mode.as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(session.launch.remote_control.as_deref(), Some(""));
        // The other session's cursors are its own.
        assert_eq!(
            loaded.sessions[&other_uuid]
                .watcher
                .cursors
                .get("db-9f8e7d6")
                .unwrap()
                .events_offset,
            64
        );
        // camelCase on disk (pretty-printed, like every atomic write),
        // matching the JSON the fleet tooling speaks.
        let raw = std::fs::read_to_string(session_path(&fleet)).unwrap();
        assert!(raw.contains(r#""version": 2"#), "{raw}");
        assert!(raw.contains(r#""sessionId": "sess-abc12345""#), "{raw}");
        assert!(raw.contains(r#""eventsOffset": 128"#), "{raw}");
        assert!(raw.contains(r#""lastUsedAt":"#), "{raw}");
        assert!(raw.contains(&format!("\"{session_uuid}\"")), "{raw}");
    }

    #[test]
    fn last_used_picks_the_newest_session_and_resolution_follows() {
        let fleet = tmp_fleet("parl-session-used-");
        // A console touches last_used_at when it opens a session.
        let mut store = FleetSessions::new();
        let mut older = OrchestratorSession::new("/repo");
        older.last_used_at = "2026-08-01T00:00:00.000Z".into();
        let mut newer = OrchestratorSession::new("/repo");
        newer.last_used_at = "2026-09-01T00:00:00.000Z".into();
        let newer_uuid = newer.uuid;
        store.upsert(older);
        store.upsert(newer);
        assert_eq!(store.last_used().unwrap().uuid, newer_uuid);
        save(&fleet, &mut store).unwrap();
        let resolved = resolve_session(&fleet).unwrap();
        assert_eq!(resolved.uuid, newer_uuid);
        // An empty store has no session to resolve.
        let none_fleet = tmp_fleet("parl-session-none-");
        assert_eq!(resolve_session(&none_fleet), None);
    }

    #[test]
    fn list_sessions_returns_every_row_most_recently_used_first() {
        let fleet = tmp_fleet("parl-session-list-");
        let mut store = FleetSessions::new();
        let mut older = OrchestratorSession::new("/repo");
        older.alias = Some("older".into());
        older.last_used_at = "2026-08-01T00:00:00.000Z".into();
        let mut mid = OrchestratorSession::new("/repo");
        mid.alias = Some("mid".into());
        mid.last_used_at = "2026-08-15T00:00:00.000Z".into();
        let mut newest = OrchestratorSession::new("/repo");
        newest.alias = Some("newest".into());
        newest.last_used_at = "2026-09-01T00:00:00.000Z".into();
        let newest_uuid = newest.uuid;
        store.upsert(older);
        store.upsert(newest);
        store.upsert(mid);
        save(&fleet, &mut store).unwrap();

        let listed = list_sessions(&fleet);
        let aliases: Vec<Option<String>> = listed.iter().map(|s| s.alias.clone()).collect();
        assert_eq!(
            aliases,
            vec![
                Some("newest".into()),
                Some("mid".into()),
                Some("older".into())
            ]
        );
        assert_eq!(listed[0].uuid, newest_uuid);
        // A store that never existed lists nothing.
        assert!(list_sessions(&tmp_fleet("parl-session-list-none-")).is_empty());
    }

    #[test]
    fn create_session_persists_a_fresh_row_and_makes_it_the_current_one() {
        let fleet = tmp_fleet("parl-session-create-");
        let session = create_session(&fleet, Some("My Session")).unwrap();
        assert_eq!(session.alias.as_deref(), Some("My Session"));
        assert!(session.uuid != uuid::Uuid::nil());
        assert_eq!(session.pid, None);
        // The cwd defaults to the directory holding the fleet.
        assert_eq!(
            session.cwd,
            fleet.parent().unwrap().to_string_lossy().into_owned()
        );
        // The row is on disk and is the one a reopened console resolves.
        let resolved = resolve_session(&fleet).unwrap();
        assert_eq!(resolved.uuid, session.uuid);
        assert_eq!(resolved.alias.as_deref(), Some("My Session"));
        // The layout key carries the new identity.
        assert_eq!(session.key().uuid, session.uuid);
        // Without an alias the row stays anonymous.
        let anon = create_session(&fleet, None).unwrap();
        assert_eq!(anon.alias, None);
        assert_eq!(resolve_session(&fleet).unwrap().uuid, anon.uuid);
    }

    #[test]
    fn session_by_key_resolves_uuid_then_alias_but_never_picks_an_ambiguous_one() {
        let fleet = tmp_fleet("parl-session-bykey-");
        let mut store = FleetSessions::new();
        let mut first = OrchestratorSession::new("/repo");
        first.alias = Some("shared".into());
        let first_uuid = first.uuid;
        let mut second = OrchestratorSession::new("/repo");
        second.alias = Some("shared".into());
        let second_uuid = second.uuid;
        let mut unique = OrchestratorSession::new("/repo");
        unique.alias = Some("Backup DB".into());
        let unique_uuid = unique.uuid;
        store.upsert(first);
        store.upsert(second);
        store.upsert(unique);
        save(&fleet, &mut store).unwrap();

        // The exact uuid resolves regardless of alias collisions.
        assert_eq!(
            session_by_key(&fleet, &first_uuid.to_string())
                .unwrap()
                .uuid,
            first_uuid
        );
        assert_eq!(
            session_by_key(&fleet, &second_uuid.to_string())
                .unwrap()
                .uuid,
            second_uuid
        );
        // A unique alias resolves, sanitized both sides.
        assert_eq!(
            session_by_key(&fleet, "backup db").unwrap().uuid,
            unique_uuid
        );
        // Two live sessions share the alias: never a silent pick.
        assert_eq!(session_by_key(&fleet, "shared"), None);
        let err = resolve_session_by_key(&fleet, "shared")
            .expect_err("the ambiguity is an error, not a guess")
            .to_string();
        assert!(
            err.contains("several live sessions")
                && err.contains(&first_uuid.to_string())
                && err.contains(&second_uuid.to_string()),
            "names the candidates: {err}"
        );
        // Nothing matches: missing is missing, and not an error anyone guesses through.
        assert_eq!(session_by_key(&fleet, "nope"), None);
        assert!(resolve_session_by_key(&fleet, "nope").is_err());
    }

    #[test]
    fn touch_heartbeat_stamps_only_liveness_and_tolerates_a_vacant_row() {
        let fleet = tmp_fleet("parl-session-heartbeat-");
        let session = create_session(&fleet, None).unwrap();
        let used_before = session.last_used_at.clone();
        touch_heartbeat(&fleet, session.uuid).unwrap();
        let store = load(&fleet).unwrap();
        let row = &store.sessions[&session.uuid];
        assert!(row.last_heartbeat.is_some(), "the heartbeat was stamped");
        assert_eq!(
            row.last_used_at, used_before,
            "a heartbeat never bumps recency: monitors do not own the session"
        );
        // An unknown (already removed) session is not an error.
        assert!(touch_heartbeat(&fleet, uuid::Uuid::new_v4()).is_ok());
        // A fleet with no store at all is not an error either.
        assert!(touch_heartbeat(&tmp_fleet("parl-session-hb-none-"), uuid::Uuid::new_v4()).is_ok());
    }

    #[test]
    fn monitor_health_derives_running_wedged_and_stopped() {
        let now = crate::util::now_ms();
        let stale =
            crate::util::iso_at(time::OffsetDateTime::now_utc() - time::Duration::seconds(60));
        let fresh = crate::util::now_iso();

        let mut running = OrchestratorSession::new("/repo");
        running.pid = Some(42);
        running.last_heartbeat = Some(fresh);
        assert_eq!(
            monitor_health(&running, |_| true, now),
            MonitorHealth::Running
        );

        // The heartbeat stopped while the pid stayed alive: a wedged monitor.
        let mut wedged = OrchestratorSession::new("/repo");
        wedged.pid = Some(42);
        wedged.last_heartbeat = Some(stale);
        assert_eq!(
            monitor_health(&wedged, |_| true, now),
            MonitorHealth::Wedged
        );

        // No pid at all, and a pid whose process is gone: stopped both.
        assert_eq!(
            monitor_health(
                &OrchestratorSession::new("/repo"),
                crate::fleet::run::is_alive,
                now
            ),
            MonitorHealth::Stopped
        );
        let mut dead = OrchestratorSession::new("/repo");
        dead.pid = Some(42);
        assert_eq!(
            monitor_health(&dead, |_| false, now),
            MonitorHealth::Stopped
        );

        // A live pid that has not ticked yet is not judged.
        let mut unticked = OrchestratorSession::new("/repo");
        unticked.pid = Some(42);
        assert_eq!(
            monitor_health(&unticked, |_| true, now),
            MonitorHealth::Running
        );
    }

    #[test]
    fn a_missing_file_and_a_foreign_version_read_as_no_session() {
        let fleet = tmp_fleet("parl-session-missing-");
        assert_eq!(load(&fleet), None);
        assert_eq!(resolve_session(&fleet), None);
        // A newer writer's version starts fresh.
        std::fs::write(session_path(&fleet), r#"{"version":3,"sessions":{}}"#).unwrap();
        assert_eq!(load(&fleet), None, "a newer writer starts fresh");
        // The v1 single-session shape is foreign too: no migration, no
        // guess at how it keys — it starts fresh.
        std::fs::write(
            session_path(&fleet),
            r#"{"version":1,"cwd":"/repo","sessionId":"s"}"#,
        )
        .unwrap();
        assert_eq!(load(&fleet), None);
    }

    #[test]
    fn unknown_top_level_keys_round_trip_through_store_saves() {
        let fleet = tmp_fleet("parl-session-extra-");
        // a real session row the heartbeat will touch
        let session = create_session(&fleet, Some("alpha")).unwrap();
        // a console's prefs key and a newer writer's unknown key
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(session_path(&fleet)).unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("console".into(), json!({"railMode": "wide"}));
        value
            .as_object_mut()
            .unwrap()
            .insert("somethingNew".into(), json!({ "deep": [1, 2] }));
        std::fs::write(session_path(&fleet), value.to_string()).unwrap();

        // load sees them, without swallowing the modeled keys
        let store = load(&fleet).unwrap();
        assert_eq!(
            store
                .extra
                .get("console")
                .and_then(|v| v.get("railMode"))
                .and_then(serde_json::Value::as_str),
            Some("wide")
        );
        assert!(store.extra.contains_key("somethingNew"));
        assert_eq!(store.sessions.len(), 1);

        // load → touch_heartbeat → save: the unknown keys come back intact
        touch_heartbeat(&fleet, session.uuid).unwrap();
        let raw = std::fs::read_to_string(session_path(&fleet)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["console"]["railMode"], "wide", "{raw}");
        assert_eq!(value["somethingNew"]["deep"][0], 1, "{raw}");
        // the modeled keys are still theirs, not captured by the flatten
        assert_eq!(value["version"], SESSION_VERSION, "{raw}");
        assert!(
            value["sessions"][session.uuid.to_string()].is_object(),
            "{raw}"
        );
        // and a later load still reads the heartbeat
        let store = load(&fleet).unwrap();
        assert!(
            store.sessions[&session.uuid].last_heartbeat.is_some(),
            "touch_heartbeat still lands in the row"
        );
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let fleet = tmp_fleet("parl-session-tolerant-");
        // A newer writer with extra fields, an older one without launch info.
        std::fs::write(
            session_path(&fleet),
            json!({
                "version": 2,
                "sessions": {
                    "9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c": {
                        "cwd": "/repo",
                        "someFutureField": {"deep": [1]},
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let store = load(&fleet).unwrap();
        let session = &store.sessions
            [&uuid::Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap()];
        assert_eq!(session.cwd, "/repo");
        assert_eq!(session.session_id, None);
        assert_eq!(session.launch, LaunchOptions::default());
        assert!(session.watcher.cursors.is_empty());
        // A session row without a uuid is a nil-uuid row, which is itself a
        // parseable (if degenerate) identity — never a failure.
        // A session row without a uuid is a nil-uuid row, which is itself a
        // parseable (if degenerate) identity — never a failure.
        assert!(session.uuid.is_nil(), "uuid was {:?}", session.uuid);
        // Corrupt JSON is not a store either.
        std::fs::write(session_path(&fleet), "{oops").unwrap();
        assert_eq!(load(&fleet), None);
    }
}
