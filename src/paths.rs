//! The `.parl` state layout: one directory under the repo root holding every
//! durable fact the fleet produces, plus the user-level `~/.parl` directory
//! holding the user's config. Nothing outside this module should spell the
//! directory name or the env-var prefix, so a future rename touches only the
//! constants below.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::util::short_uuid;

/// The fleet's state directory, created under the repository root.
pub const STATE_DIR_NAME: &str = ".parl";
/// Prefix for every environment variable this tool reads (`PARL_DIR`, …).
pub const ENV_PREFIX: &str = "PARL";
/// The command name users type; used in help text, hints, and the prompt.
pub const BIN_NAME: &str = "parl";
/// The fleet-level pi catalogue (`availableModels` + `commands`): a property
/// of the pi installation, byte-identical across runs, so it lives once
/// here instead of being copied into every `run.json`. Refreshed on demand
/// by any live worker monitor, and stamped with when pi last answered.
pub const PI_CACHE_FILE: &str = "pi-cache.json";

/// Env-var name from its suffix: `_DIR` -> `PARL_DIR`.
#[must_use]
pub fn env_var(suffix: &str) -> String {
    format!("{ENV_PREFIX}_{suffix}")
}

/// The key naming one orchestrator session's directory under
/// `orchestrators/`: `<alias|-default>-<short-uuid>`. A session's alias is
/// optional (a later stage derives it from the session's first prompt), so
/// the directory falls back to `default` — the name must stay readable in
/// `ls` either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKey {
    pub alias: Option<String>,
    pub uuid: Uuid,
}

impl SessionKey {
    /// A key for a session with `alias` (may be `None`) and `uuid`.
    #[must_use]
    pub fn new(alias: Option<String>, uuid: Uuid) -> Self {
        Self { alias, uuid }
    }

    /// `orchestrators/<alias>-<short-uuid>`: the sanitized alias (or
    /// `default`) plus the last 7 hex chars of the uuid.
    #[must_use]
    pub fn dir_name(&self) -> String {
        let alias = self
            .alias
            .as_deref()
            .map(crate::util::sanitize_name)
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "default".to_string());
        format!("{alias}-{}", short_uuid(&self.uuid))
    }
}

impl Default for SessionKey {
    fn default() -> Self {
        Self {
            alias: None,
            uuid: crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION,
        }
    }
}

/// Resolved `.parl` layout for one fleet.
///
/// Every path the console, monitors, or tools touch is derived here; see the
/// tree in AGENTS.md. An old `.pi-fleet` directory is ignored entirely — there
/// is no migration and none is wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetPaths {
    root: PathBuf,
}

impl FleetPaths {
    /// The layout rooted at an explicit directory (the `.parl` dir itself).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve the fleet dir for `cwd`: `$PARL_DIR` when set, else `<cwd>/.parl`.
    #[must_use]
    pub fn discover(cwd: &Path) -> Self {
        Self::discover_with_env(cwd, std::env::var(env_var("DIR")).ok().as_deref())
    }

    /// [`FleetPaths::discover`] with the env value injected (tests).
    #[must_use]
    pub fn discover_with_env(cwd: &Path, parl_dir: Option<&str>) -> Self {
        match parl_dir {
            Some(dir) if !dir.trim().is_empty() => Self::new(dir.trim()),
            _ => Self::new(cwd.join(STATE_DIR_NAME)),
        }
    }

    /// `.parl/` itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `fleet.json` — console + watcher cursors, the claude session id,
    /// remembered prefs.
    pub fn fleet_json(&self) -> PathBuf {
        self.root.join("fleet.json")
    }

    /// `pi-cache.json` — the fleet-level pi catalogue (models + commands),
    /// written by a worker monitor at boot and read back through run state
    /// loading so the console needs no other source.
    pub fn pi_cache(&self) -> PathBuf {
        self.root.join(PI_CACHE_FILE)
    }

    /// `console.lock` — single-instance lock for the TUI.
    pub fn console_lock(&self) -> PathBuf {
        self.root.join("console.lock")
    }

    /// `routing/` — model choices a spawn is waiting on the human for:
    /// `<id>.json` asked by the spawn, `<id>.answer.json` written back by the
    /// console. Created lazily.
    pub fn routing_dir(&self) -> PathBuf {
        self.root.join("routing")
    }

    /// `orchestrators/` — the per-session parent.
    pub fn orchestrators_dir(&self) -> PathBuf {
        self.root.join("orchestrators")
    }

    /// `orchestrators/<alias>-<short-uuid>/` — one session's whole state:
    /// `state.json`, `events.jsonl`, `inbox.jsonl`, `claude.log`, `prompt.md`.
    pub fn orchestrator_dir(&self, key: &SessionKey) -> PathBuf {
        self.orchestrators_dir().join(key.dir_name())
    }

    /// `orchestrators/<key>/state.json` — monitor pid, session id, model,
    /// cost, turns, activity, pending permission.
    pub fn orchestrator_state(&self, key: &SessionKey) -> PathBuf {
        self.orchestrator_dir(key).join("state.json")
    }

    /// `orchestrators/<key>/capabilities.json` — what the agent offers right
    /// now: tools, commands, MCP servers. Separate from `state.json` because
    /// it is asked for and rewritten, not snapshotted once at handshake.
    pub fn orchestrator_capabilities(&self, key: &SessionKey) -> PathBuf {
        self.orchestrator_dir(key).join("capabilities.json")
    }

    /// `orchestrators/<key>/events.jsonl` — the orchestrator transcript.
    pub fn orchestrator_events(&self, key: &SessionKey) -> PathBuf {
        self.orchestrator_dir(key).join("events.jsonl")
    }

    /// `orchestrators/<key>/inbox.jsonl` — console -> monitor.
    pub fn orchestrator_inbox(&self, key: &SessionKey) -> PathBuf {
        self.orchestrator_dir(key).join("inbox.jsonl")
    }

    /// `orchestrators/<key>/claude.log` — raw protocol both directions, plus
    /// the monitor's own diagnostics.
    pub fn claude_log(&self, key: &SessionKey) -> PathBuf {
        self.orchestrator_dir(key).join("claude.log")
    }

    /// `orchestrators/<key>/prompt.md` — the rendered orchestrator prompt.
    pub fn orchestrator_prompt(&self, key: &SessionKey) -> PathBuf {
        self.orchestrator_dir(key).join("prompt.md")
    }

    /// `runs/`.
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// `runs/<runId>/`.
    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(run_id)
    }

    /// `runs/<runId>/run.json` — the run's durable facts.
    pub fn run_json(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("run.json")
    }

    /// `runs/<runId>/events.jsonl` — the run transcript.
    pub fn run_events(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("events.jsonl")
    }

    /// `runs/<runId>/inbox.jsonl` — orchestrator/console -> monitor.
    pub fn run_inbox(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("inbox.jsonl")
    }

    /// `runs/<runId>/outbox.jsonl` — worker -> monitor.
    pub fn run_outbox(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("outbox.jsonl")
    }

    /// `runs/<runId>/report.md` — the worker's final report.
    pub fn run_report(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("report.md")
    }

    /// `runs/<runId>/pi.log` — raw pi RPC stream, plus the monitor's own
    /// diagnostics.
    pub fn pi_log(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("pi.log")
    }

    /// `runs/<runId>/session/` — pi session files.
    pub fn run_session_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("session")
    }

    /// `pi/extensions/fleet-worker.ts` — the worker extension, embedded in
    /// the binary and materialized here at spawn time (single-binary
    /// installs cannot rely on the package's `pi/` tree existing).
    pub fn pi_extension(&self) -> PathBuf {
        self.root
            .join("pi")
            .join("extensions")
            .join("fleet-worker.ts")
    }

    /// `pi/skills/fleet-worker-report/SKILL.md` — the report skill, embedded
    /// and materialized like [`FleetPaths::pi_extension`].
    pub fn pi_skill(&self) -> PathBuf {
        self.root
            .join("pi")
            .join("skills")
            .join("fleet-worker-report")
            .join("SKILL.md")
    }

    /// Create the layout and make sure git ignores it.
    ///
    /// Returns whether the `.gitignore` gained an entry. Subdirectories
    /// (`runs/`, `orchestrators/`) are created here too: they are part of the
    /// fixed layout and callers otherwise race to make them. Per-session
    /// directories under `orchestrators/` are created lazily, once a session
    /// exists.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` when a layout directory cannot be created or
    /// the `.gitignore` entry cannot be written.
    pub fn ensure(&self) -> std::io::Result<bool> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(self.runs_dir())?;
        std::fs::create_dir_all(self.orchestrators_dir())?;
        ensure_gitignore_entry(&git_root_of(&self.root), &format!("{STATE_DIR_NAME}/"))
    }
}

/// The repository root containing `dir`, or `dir` itself when it is not
/// inside a git work tree (best effort; `ensure` still writes the file).
fn git_root_of(dir: &Path) -> PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map_or_else(
            || dir.to_path_buf(),
            |out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()),
        )
}

/// The user-level config directory: `~/.parl`, or the `$PARL_HOME` override
/// wholesale (mirroring how `$PARL_DIR` overrides a fleet's location). Same
/// name as [`STATE_DIR_NAME`], different scope, deliberately — the project's
/// state lives under `<repo>/.parl`, the user's config under `~/.parl`.
/// `None` when neither the override nor a home is known, which callers read
/// as "no user config".
#[must_use]
pub fn user_dir() -> Option<PathBuf> {
    user_dir_with_env(
        std::env::var(env_var("HOME")).ok().as_deref(),
        dirs::home_dir().as_deref(),
    )
}

/// [`user_dir`] with the `$PARL_HOME` value and the home directory injected,
/// mirroring [`FleetPaths::discover_with_env`]: tests pass synthetic values
/// so resolution never touches the ambient environment, and the fallback
/// branch needs no ambient read either.
#[must_use]
pub fn user_dir_with_env(parl_home: Option<&str>, home: Option<&Path>) -> Option<PathBuf> {
    match parl_home {
        Some(dir) if !dir.trim().is_empty() => Some(PathBuf::from(dir.trim())),
        _ => home.map(|home| home.join(STATE_DIR_NAME)),
    }
}

/// User-level config: `~/.parl/config.toml`. Every field is optional — a
/// missing file, an empty file, or a file with only some keys all read as
/// defaults — but a malformed file is an error, because silently ignoring a
/// config the user wrote is worse than failing.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct UserConfig {
    /// Defaults for the orchestrator.
    pub orchestrator: OrchestratorConfig,
    /// Defaults for workers.
    pub worker: WorkerConfig,
    /// How a long orchestrator session keeps itself small.
    pub session: SessionConfig,
    /// Whether a brief's model is chosen for it, and how.
    pub routing: RoutingConfig,
    /// Fleet-wide limits.
    pub limits: LimitsConfig,
}

/// The `[orchestrator]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct OrchestratorConfig {
    /// The orchestrator model claude is launched with when nothing more
    /// specific is recorded.
    pub model: Option<String>,
}

/// The `[worker]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    /// The model spawned workers run with when no `--model` is passed.
    pub model: Option<String>,
    /// The provider spawned workers run under when no `--provider` is passed.
    pub provider: Option<String>,
}

/// Turns an orchestrator session runs before it is compacted, when
/// `[session] auto_compact_turns` is absent. Generous: compaction costs a
/// turn of its own, so it should be rare, and the point is to stop a session
/// degrading over hours rather than to keep it short.
pub const DEFAULT_AUTO_COMPACT_TURNS: u32 = 60;

/// The `[session]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Compact the orchestrator's context once it has run this many turns
    /// since the last compaction. `Some(0)` turns it off; `None` means
    /// [`DEFAULT_AUTO_COMPACT_TURNS`].
    pub auto_compact_turns: Option<u32>,
}

impl SessionConfig {
    /// The compaction threshold in turns, or `None` when it is switched off.
    #[must_use]
    pub fn auto_compact_turns(&self) -> Option<u32> {
        match self.auto_compact_turns {
            Some(0) => None,
            Some(turns) => Some(turns),
            None => Some(DEFAULT_AUTO_COMPACT_TURNS),
        }
    }
}

/// How sure the judgment has to be before it is acted on, when
/// `[routing] confidence_threshold` is absent. Below it the spawn keeps the
/// configured default rather than a guess.
pub const DEFAULT_ROUTING_CONFIDENCE: f64 = 0.6;

/// The `[routing]` section: choosing a worker's model, thinking level and
/// worktree from the brief, with TypeSafe's System One. Off unless asked
/// for, and inert without an API key either way.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Route at all. `--route` / `--no-route` override it per spawn.
    pub enabled: bool,
    /// The System One model to ask.
    pub model: String,
    /// How sure the choice has to be; see [`DEFAULT_ROUTING_CONFIDENCE`].
    pub confidence_threshold: Option<f64>,
    /// A different endpoint, for a proxy or a test stub.
    pub endpoint: Option<String>,
    /// The models routing may choose between, as `provider:id` or a bare
    /// `id` (any provider). Empty means every model pi offers under the
    /// configured `[worker] provider` — and pi can offer hundreds, more than
    /// one judgment can weigh, which is what this list is for.
    pub models: Vec<String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "jev-latest".to_string(),
            confidence_threshold: None,
            endpoint: None,
            models: Vec::new(),
        }
    }
}

impl RoutingConfig {
    /// The confidence a judgment needs before it is acted on. Validated at
    /// load, so what is here is already a probability.
    #[must_use]
    pub fn confidence_threshold(&self) -> f64 {
        self.confidence_threshold
            .unwrap_or(DEFAULT_ROUTING_CONFIDENCE)
    }
}

/// The default per-session worker cap when `[limits] max_workers_per_session`
/// is absent from the user config: the same number the orchestrator prompt
/// substitutes as `MAX_WORKERS`, so the prompt's advice and the enforced
/// refusal agree.
pub const DEFAULT_MAX_WORKERS_PER_SESSION: usize = 3;

/// The `[limits]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// How many live (non-terminal) workers one orchestrator session may
    /// have at once; `None` means the default. An enforced cap: `spawn`
    /// refuses once a session's live runs reach this number.
    pub max_workers_per_session: Option<usize>,
}

impl LimitsConfig {
    /// The per-session worker cap: the configured value, or
    /// [`DEFAULT_MAX_WORKERS_PER_SESSION`] when absent. Zero deliberately
    /// means "no spawning allowed", never "unlimited".
    #[must_use]
    pub fn max_workers_per_session(&self) -> usize {
        self.max_workers_per_session
            .unwrap_or(DEFAULT_MAX_WORKERS_PER_SESSION)
    }
}

impl UserConfig {
    /// The worker model, most specific wins: the explicit argument, then
    /// the config's `[worker] model`, then `None` (let pi decide).
    #[must_use]
    pub fn worker_model<'a>(&'a self, explicit: Option<&'a str>) -> Option<&'a str> {
        explicit.or(self.worker.model.as_deref())
    }

    /// The worker provider, same order as [`UserConfig::worker_model`].
    #[must_use]
    pub fn worker_provider<'a>(&'a self, explicit: Option<&'a str>) -> Option<&'a str> {
        explicit.or(self.worker.provider.as_deref())
    }

    /// The per-session worker cap, resolved from the `[limits]` section.
    #[must_use]
    pub fn max_workers_per_session(&self) -> usize {
        self.limits.max_workers_per_session()
    }

    /// The compaction threshold, resolved from the `[session]` section.
    #[must_use]
    pub fn auto_compact_turns(&self) -> Option<u32> {
        self.session.auto_compact_turns()
    }

    /// The orchestrator model, most specific wins: the explicit argument,
    /// then the project's persisted launch record (`fleet.json`), then the
    /// config's `[orchestrator] model`, then `None` (let claude decide).
    #[must_use]
    pub fn orchestrator_model<'a>(
        &'a self,
        explicit: Option<&'a str>,
        persisted: Option<&'a str>,
    ) -> Option<&'a str> {
        explicit
            .or(persisted)
            .or(self.orchestrator.model.as_deref())
    }
}

/// Load `~/.parl/config.toml` under `user_dir`. A missing or empty file
/// reads as defaults; a malformed one is an error naming the path and the
/// parse problem.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_user_config(user_dir: Option<&Path>) -> anyhow::Result<UserConfig> {
    use anyhow::Context as _;
    let Some(user_dir) = user_dir else {
        return Ok(UserConfig::default());
    };
    let path = user_dir.join("config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UserConfig::default());
        }
        Err(err) => {
            return Err(err).context(format!("reading user config {}", path.display()));
        }
    };
    if raw.trim().is_empty() {
        return Ok(UserConfig::default());
    }
    let config: UserConfig = toml::from_str(&raw)
        .map_err(|err| anyhow::anyhow!("parsing user config {}: {err}", path.display()))?;
    // a value that parses but cannot mean what it says is as malformed as
    // one that does not parse: `80` meant as a percentage must not quietly
    // become the default
    if let Some(threshold) = config.routing.confidence_threshold
        && !(0.0..=1.0).contains(&threshold)
    {
        anyhow::bail!(
            "user config {}: [routing] confidence_threshold = {threshold} is outside 0..1 \
(it is a probability: 0.6, not 60)",
            path.display()
        );
    }
    Ok(config)
}

/// A console lock whose heartbeat is older than this is a crashed console,
/// not a live one.
pub const CONSOLE_LOCK_STALE_MS: i64 = 15_000;

/// The pid holding `lock` (a fleet's `console.lock`), while its heartbeat is
/// fresh: `None` for a missing, malformed or stale lock. Whose pid it is,
/// the caller decides — the console refuses a second instance, a spawn only
/// wants to know whether anyone is there to ask.
#[must_use]
pub fn console_holder(lock: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(lock).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let pid = value.get("pid")?.as_u64()?;
    let ts = value.get("ts")?.as_str()?;
    let age = crate::util::now_ms().saturating_sub(crate::util::parse_ts_ms(ts).unwrap_or(0));
    (age <= CONSOLE_LOCK_STALE_MS).then_some(pid)
}

/// Set one `[routing]` key in `<user_dir>/config.toml` — `enabled`,
/// `confidence_threshold`, `models` — creating the file and the section when
/// they are missing.
///
/// Edits the document rather than re-serialising it, so a file the user
/// wrote by hand keeps its comments, its order and every key this code does
/// not know about. A file that does not parse is refused rather than
/// overwritten — silently replacing a config someone wrote is worse than
/// failing.
///
/// # Errors
///
/// Returns an error when the file cannot be read, does not parse, or cannot
/// be written.
pub fn set_routing(user_dir: &Path, key: &str, value: toml_edit::Item) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let path = user_dir.join("config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).context(format!("reading user config {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|err| anyhow::anyhow!("parsing user config {}: {err}", path.display()))?;
    let routing = doc
        .entry("routing")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(table) = routing.as_table_like_mut() else {
        anyhow::bail!("user config {}: `routing` is not a table", path.display());
    };
    table.insert(key, value);
    std::fs::create_dir_all(user_dir)
        .with_context(|| format!("creating {}", user_dir.display()))?;
    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("writing user config {}", path.display()))
}

/// Append `entry` to `<root>/.gitignore` unless a line already covers it.
///
/// Introduces the `# parl` marker on first touch. Returns whether the file
/// changed. Ported from the TypeScript `ensureGitignoreEntry`.
///
/// # Errors
///
/// Returns `std::io::Error` when the `.gitignore` cannot be opened or
/// written.
pub fn ensure_gitignore_entry(root: &Path, entry: &str) -> std::io::Result<bool> {
    use std::io::Write;
    let gitignore_path = root.join(".gitignore");
    let content = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    let lines: Vec<String> = content.split('\n').map(|l| l.trim().to_string()).collect();
    if lines.iter().any(|l| l == entry) {
        return Ok(false);
    }
    let needs_marker = !lines.iter().any(|l| l == "# parl");
    let addition = format!("{}{entry}\n", if needs_marker { "# parl\n" } else { "" });
    let prefix = if !content.is_empty() && !content.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore_path)?;
    file.write_all(format!("{prefix}{addition}").as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{RETRY_INTERVAL, git_sync, tmp_dir};
    use std::time::{Duration, Instant};

    /// `ensure` touches several fresh paths, and under this machine's
    /// parallel load a call has been observed to fail with NotFound on a
    /// path it had itself just created — occasionally for longer than a
    /// moment. It self-heals (`create_dir_all` rebuilds whatever vanished),
    /// so poll `Err` and return the first `Ok`; the bound keeps a real
    /// breakage loud. Operation-level, so a longer bound than the
    /// per-spawn git helper's.
    fn ensure_with_retry(paths: &FleetPaths) -> std::io::Result<bool> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match paths.ensure() {
                Ok(changed) => return Ok(changed),
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(err);
                    }
                    std::thread::sleep(RETRY_INTERVAL);
                }
            }
        }
    }

    #[test]
    fn layout_paths_are_derived_from_the_root() {
        let paths = FleetPaths::new("/repo/x/.parl");
        let uuid = uuid::Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap();
        let key = SessionKey::new(Some("s0".into()), uuid);
        let default_key = SessionKey::default();
        assert_eq!(paths.root(), Path::new("/repo/x/.parl"));
        assert_eq!(
            paths.fleet_json(),
            PathBuf::from("/repo/x/.parl/fleet.json")
        );
        assert_eq!(
            paths.console_lock(),
            PathBuf::from("/repo/x/.parl/console.lock")
        );
        assert_eq!(
            paths.orchestrators_dir(),
            PathBuf::from("/repo/x/.parl/orchestrators")
        );
        // A session's whole state sits in its own alias-prefixed directory.
        assert_eq!(key.dir_name(), "s0-f7a8b9c");
        assert_eq!(
            paths.orchestrator_dir(&key),
            PathBuf::from("/repo/x/.parl/orchestrators/s0-f7a8b9c")
        );
        assert_eq!(
            paths.orchestrator_state(&key),
            PathBuf::from("/repo/x/.parl/orchestrators/s0-f7a8b9c/state.json")
        );
        assert_eq!(
            paths.orchestrator_events(&key),
            PathBuf::from("/repo/x/.parl/orchestrators/s0-f7a8b9c/events.jsonl")
        );
        assert_eq!(
            paths.orchestrator_inbox(&key),
            PathBuf::from("/repo/x/.parl/orchestrators/s0-f7a8b9c/inbox.jsonl")
        );
        assert_eq!(
            paths.claude_log(&key),
            PathBuf::from("/repo/x/.parl/orchestrators/s0-f7a8b9c/claude.log")
        );
        assert_eq!(
            paths.orchestrator_prompt(&key),
            PathBuf::from("/repo/x/.parl/orchestrators/s0-f7a8b9c/prompt.md")
        );
        // An alias-less session dirs as `default-<short-uuid>`; the alias is
        // sanitized before it reaches the filesystem.
        assert_eq!(default_key.dir_name(), "default-0000000");
        let noisy = SessionKey::new(Some("My Session!".into()), uuid);
        assert_eq!(noisy.dir_name(), "my-session-f7a8b9c");
        assert_eq!(
            paths.run_json("a-1f2e3d4"),
            PathBuf::from("/repo/x/.parl/runs/a-1f2e3d4/run.json")
        );
        assert_eq!(
            paths.run_report("a-1f2e3d4"),
            PathBuf::from("/repo/x/.parl/runs/a-1f2e3d4/report.md")
        );
        assert_eq!(
            paths.pi_log("a-1f2e3d4"),
            PathBuf::from("/repo/x/.parl/runs/a-1f2e3d4/pi.log")
        );
        assert_eq!(
            paths.run_session_dir("a-1f2e3d4"),
            PathBuf::from("/repo/x/.parl/runs/a-1f2e3d4/session")
        );
        assert_eq!(
            paths.pi_extension(),
            PathBuf::from("/repo/x/.parl/pi/extensions/fleet-worker.ts")
        );
        assert_eq!(
            paths.pi_skill(),
            PathBuf::from("/repo/x/.parl/pi/skills/fleet-worker-report/SKILL.md")
        );
    }

    #[test]
    fn discover_prefers_parl_dir_env_over_cwd() {
        let cwd = Path::new("/repo");
        assert_eq!(
            FleetPaths::discover_with_env(cwd, None),
            FleetPaths::new("/repo/.parl")
        );
        assert_eq!(
            FleetPaths::discover_with_env(cwd, Some("/elsewhere/fleet")),
            FleetPaths::new("/elsewhere/fleet")
        );
        assert_eq!(
            FleetPaths::discover_with_env(cwd, Some("  ")),
            FleetPaths::new("/repo/.parl")
        );
        // The env names themselves are derived, never spelled in full.
        assert_eq!(env_var("DIR"), "PARL_DIR");
        assert_eq!(env_var("RUN"), "PARL_RUN");
        assert_eq!(env_var("HOME"), "PARL_HOME");
    }

    /// A temp user dir, the way production resolves `~/.parl`: with the
    /// injected `$PARL_HOME` the override wins wholesale, else `.parl` under
    /// the injected home, else nothing. Every branch is injectable, so a test
    /// can never land in the real home directory.
    #[test]
    fn user_dir_prefers_parl_home_and_falls_back_under_the_home() {
        let home = Path::new("/home/alice");
        assert_eq!(
            user_dir_with_env(None, Some(home)),
            Some(home.join(STATE_DIR_NAME))
        );
        assert_eq!(
            user_dir_with_env(Some("/elsewhere/config"), Some(home)),
            Some(PathBuf::from("/elsewhere/config"))
        );
        // A blank value is the variable set-but-empty: the fallback applies.
        assert_eq!(
            user_dir_with_env(Some("  "), Some(home)),
            Some(home.join(STATE_DIR_NAME))
        );
        // The override stands alone; without any home there is no `.parl`.
        assert_eq!(
            user_dir_with_env(Some("/elsewhere/config"), None),
            Some(PathBuf::from("/elsewhere/config"))
        );
        assert_eq!(user_dir_with_env(None, None), None);
    }

    /// The config file lives directly in the user dir and is read once.
    fn write_config(user_dir: &Path, body: &str) {
        std::fs::create_dir_all(user_dir).unwrap();
        std::fs::write(user_dir.join("config.toml"), body).unwrap();
    }

    #[test]
    fn user_config_reads_missing_empty_and_partial_files_as_defaults() {
        let tmp = tmp_dir("parl-cfg-missing-");
        assert_eq!(load_user_config(None).unwrap(), UserConfig::default());
        // No file at all.
        assert_eq!(load_user_config(Some(&tmp)).unwrap(), UserConfig::default());
        // An empty file.
        write_config(&tmp, "");
        assert_eq!(load_user_config(Some(&tmp)).unwrap(), UserConfig::default());
        // Only one key of one section: the rest stay absent.
        write_config(&tmp, "[worker]\nmodel = \"deepseek-v4-flash\"\n");
        let config = load_user_config(Some(&tmp)).unwrap();
        assert_eq!(config.worker.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(config.worker.provider, None);
        assert_eq!(config.orchestrator.model, None);
        // Unknown keys are tolerated, like every reader in this crate.
        write_config(&tmp, "[orchestrator]\nmodel = \"opus\"\nfuture = 1\n");
        let config = load_user_config(Some(&tmp)).unwrap();
        assert_eq!(config.orchestrator.model.as_deref(), Some("opus"));
    }

    #[test]
    fn user_config_reads_the_limits_section_and_defaults_the_cap() {
        let tmp = tmp_dir("parl-cfg-limits-");
        // No file, an empty file, and a file with other sections alone all
        // read as the default cap.
        assert_eq!(
            load_user_config(Some(&tmp)).unwrap().limits,
            LimitsConfig::default()
        );
        assert_eq!(
            load_user_config(Some(&tmp))
                .unwrap()
                .max_workers_per_session(),
            DEFAULT_MAX_WORKERS_PER_SESSION
        );
        write_config(&tmp, "");
        assert_eq!(
            load_user_config(Some(&tmp))
                .unwrap()
                .max_workers_per_session(),
            DEFAULT_MAX_WORKERS_PER_SESSION
        );
        write_config(&tmp, "[worker]\nmodel = \"deepseek-v4-flash\"\n");
        assert_eq!(
            load_user_config(Some(&tmp))
                .unwrap()
                .max_workers_per_session(),
            DEFAULT_MAX_WORKERS_PER_SESSION
        );
        // The configured cap wins; zero is a real value (no spawning allowed).
        write_config(&tmp, "[limits]\nmax_workers_per_session = 5\n");
        let config = load_user_config(Some(&tmp)).unwrap();
        assert_eq!(config.limits.max_workers_per_session, Some(5));
        assert_eq!(config.max_workers_per_session(), 5);
        write_config(&tmp, "[limits]\nmax_workers_per_session = 0\n");
        assert_eq!(
            load_user_config(Some(&tmp))
                .unwrap()
                .max_workers_per_session(),
            0,
            "zero means no spawning, not unlimited"
        );
        // All three sections parse together.
        write_config(
            &tmp,
            "[orchestrator]\nmodel = \"claude-opus-5\"\n\n[worker]\nmodel = \"deepseek-v4-flash\"\n\n[limits]\nmax_workers_per_session = 7\n",
        );
        let config = load_user_config(Some(&tmp)).unwrap();
        assert_eq!(config.orchestrator.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(config.max_workers_per_session(), 7);
        // A negative cap is not a usize: a malformed config stays a hard
        // error naming the path, never a silent fallback.
        write_config(&tmp, "[limits]\nmax_workers_per_session = -1\n");
        let err = load_user_config(Some(&tmp)).unwrap_err().to_string();
        assert!(err.contains("config.toml"), "names the file: {err}");
    }

    #[test]
    fn user_config_parses_both_sections() {
        let tmp = tmp_dir("parl-cfg-full-");
        write_config(
            &tmp,
            "[orchestrator]\nmodel = \"claude-opus-5\"\n\n[worker]\nmodel = \"deepseek-v4-flash\"\nprovider = \"opencode-go\"\n",
        );
        let config = load_user_config(Some(&tmp)).unwrap();
        assert_eq!(config.orchestrator.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(config.worker.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(config.worker.provider.as_deref(), Some("opencode-go"));
    }

    #[test]
    fn a_malformed_user_config_names_the_path_and_the_problem() {
        let tmp = tmp_dir("parl-cfg-bad-");
        write_config(&tmp, "[orchestrator\nmodel = \"x\"\n");
        let err = load_user_config(Some(&tmp))
            .expect_err("a malformed config errors, never silently defaults")
            .to_string();
        assert!(err.contains("config.toml"), "names the file: {err}");
        assert!(err.contains("line 1"), "names the parse problem: {err}");
    }

    #[test]
    fn a_confidence_threshold_outside_zero_to_one_is_refused_not_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[routing]\nconfidence_threshold = 80\n",
        )
        .unwrap();
        let err = load_user_config(Some(tmp.path())).unwrap_err().to_string();
        assert!(err.contains("confidence_threshold"), "{err}");
        assert!(err.contains("config.toml"), "names the file: {err}");

        std::fs::write(
            tmp.path().join("config.toml"),
            "[routing]\nconfidence_threshold = 0.8\nmodels = [\"anthropic:claude-opus-5\"]\n",
        )
        .unwrap();
        let config = load_user_config(Some(tmp.path())).unwrap();
        assert!((config.routing.confidence_threshold() - 0.8).abs() < 1e-9);
        assert_eq!(config.routing.models, vec!["anthropic:claude-opus-5"]);
    }

    #[test]
    fn switching_routing_keeps_the_rest_of_a_hand_written_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# my settings\n[worker]\nmodel = \"claude-opus-5\" # the good one\n\n[routing]\nmodel = \"jev-latest\"\n",
        )
        .unwrap();

        set_routing(tmp.path(), "enabled", toml_edit::value(true)).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# my settings"), "{raw}");
        assert!(raw.contains("# the good one"), "{raw}");
        let config = load_user_config(Some(tmp.path())).unwrap();
        assert!(config.routing.enabled);
        assert_eq!(config.routing.model, "jev-latest");
        assert_eq!(config.worker.model.as_deref(), Some("claude-opus-5"));

        set_routing(tmp.path(), "enabled", toml_edit::value(false)).unwrap();
        assert!(!load_user_config(Some(tmp.path())).unwrap().routing.enabled);

        set_routing(tmp.path(), "confidence_threshold", toml_edit::value(0.45)).unwrap();
        let models = ["anthropic:claude-opus-5", "opencode-go:deepseek-v4-flash"];
        set_routing(
            tmp.path(),
            "models",
            toml_edit::value(models.iter().copied().collect::<toml_edit::Array>()),
        )
        .unwrap();
        let config = load_user_config(Some(tmp.path())).unwrap();
        assert!((config.routing.confidence_threshold() - 0.45).abs() < 1e-9);
        assert_eq!(config.routing.models, models);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# the good one"), "{raw}");
    }

    #[test]
    fn switching_routing_creates_the_file_and_refuses_a_broken_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("fresh");
        set_routing(&home, "enabled", toml_edit::value(true)).unwrap();
        assert!(load_user_config(Some(&home)).unwrap().routing.enabled);

        let broken = tmp.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("config.toml"), "[routing\nenabled = ").unwrap();
        assert!(set_routing(&broken, "enabled", toml_edit::value(true)).is_err());
        assert_eq!(
            std::fs::read_to_string(broken.join("config.toml")).unwrap(),
            "[routing\nenabled = ",
            "a config that does not parse is left exactly as it was"
        );
    }

    #[test]
    fn auto_compact_turns_reads_absent_as_the_default_and_zero_as_off() {
        assert_eq!(
            SessionConfig::default().auto_compact_turns(),
            Some(DEFAULT_AUTO_COMPACT_TURNS)
        );
        assert_eq!(
            SessionConfig {
                auto_compact_turns: Some(0)
            }
            .auto_compact_turns(),
            None,
            "zero is off, never every turn"
        );
        assert_eq!(
            SessionConfig {
                auto_compact_turns: Some(12)
            }
            .auto_compact_turns(),
            Some(12)
        );
    }

    #[test]
    fn resolution_prefers_explicit_then_persisted_then_config_then_default() {
        let config = UserConfig {
            orchestrator: OrchestratorConfig {
                model: Some("claude-fable-5".into()),
            },
            worker: WorkerConfig {
                model: Some("deepseek-v4-flash".into()),
                provider: Some("opencode-go".into()),
            },
            session: SessionConfig::default(),
            routing: RoutingConfig::default(),
            limits: LimitsConfig {
                max_workers_per_session: Some(4),
            },
        };
        // Orchestrator: explicit beats the persisted record beats the config.
        assert_eq!(
            config.orchestrator_model(Some("opus"), Some("sonnet")),
            Some("opus")
        );
        assert_eq!(
            config.orchestrator_model(None, Some("sonnet")),
            Some("sonnet")
        );
        assert_eq!(
            config.orchestrator_model(None, None),
            Some("claude-fable-5")
        );
        // Worker: explicit beats the config; provider resolves independently.
        assert_eq!(config.worker_model(Some("glm-5.3")), Some("glm-5.3"));
        assert_eq!(config.worker_model(None), Some("deepseek-v4-flash"));
        assert_eq!(config.worker_provider(None), Some("opencode-go"));
        // Limits: the configured cap wins over the default.
        assert_eq!(config.max_workers_per_session(), 4);
        // Nothing anywhere: the empty config still yields defaults.
        assert_eq!(UserConfig::default().orchestrator_model(None, None), None);
        assert_eq!(UserConfig::default().worker_model(None), None);
        assert_eq!(UserConfig::default().worker_provider(None), None);
        assert_eq!(
            UserConfig::default().max_workers_per_session(),
            DEFAULT_MAX_WORKERS_PER_SESSION
        );
    }

    #[test]
    fn ensure_creates_layout_and_gitignores_once() {
        let root = tmp_dir("parl-paths-");
        // Both spawns transiently fail under full-suite parallel load; the
        // shared bounded retry covers them, and the rev-parse probe confirms
        // the repo answers before `ensure` consults it.
        git_sync(&root, &["init", "-q", "-b", "main"]);
        git_sync(&root, &["rev-parse", "--show-toplevel"]);
        let paths = FleetPaths::new(root.join(STATE_DIR_NAME));
        assert!(ensure_with_retry(&paths).unwrap());
        assert!(paths.root().is_dir());
        assert!(paths.runs_dir().is_dir());
        assert!(paths.orchestrators_dir().is_dir());
        // Per-session directories are created lazily, by whoever owns a key.
        assert!(
            paths
                .orchestrator_dir(&SessionKey::default())
                .parent()
                .is_some()
        );
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gitignore.contains("# parl\n.parl/"), "{gitignore}");
        // Second run: already covered, no change.
        assert!(!ensure_with_retry(&paths).unwrap());
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(gitignore.matches(".parl/").count(), 1);
        // An existing unrelated entry survives and gets the marker once.
        std::fs::write(root.join(".gitignore"), "node_modules/\n.parl/\ndist/").unwrap();
        assert!(!ensure_gitignore_entry(&root, ".parl/").unwrap());
        assert!(!ensure_gitignore_entry(&root, ".parl/").unwrap());
    }
}
