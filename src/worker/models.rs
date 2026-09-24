//! Asking pi what it offers: the `--list-models` one-shot and model
//! checking, plus the on-demand fleet catalogue fetch that lets a fresh
//! fleet route and the console's `/routing` panel work before any worker
//! has ever booted. A bad `--model` is refused before a worktree exists,
//! naming the closest models pi does have. (Ported from the TypeScript
//! `src/models.ts`.)

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::fleet::run::{PiCache, WorkerCommand, WorkerModel, read_pi_cache};
use crate::paths::{FleetPaths, env_var};
use crate::worker::monitor::materialize_worker_files;
use crate::worker::rpc::{CommandEntry, ModelRef, RpcMessage, parse_line};

/// How long the listing may take before the check gives up and allows the spawn.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// The listing is asked for at most once per pi binary and process; only a
/// non-empty answer is worth remembering (an empty one means pi could not be
/// asked). Keyed by the spec so tests pointing at different fakes never share
/// an answer.
static CACHE: LazyLock<Mutex<HashMap<String, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `PILOTFISH_PI_BIN` is an executable spec split on spaces
/// ("node /path/fake-pi.mjs"); the default is plain `pi`.
#[must_use]
pub fn pi_bin_spec() -> String {
    std::env::var(env_var("PI_BIN")).unwrap_or_else(|_| "pi".into())
}

/// [`ModelRef`]s pi reported (`get_available_models` data), slimmed to what
/// the console's model switcher and routing need. Refs without an id are
/// dropped; the provider is empty when pi did not name one.
#[must_use]
pub fn worker_models(models: &[ModelRef]) -> Vec<WorkerModel> {
    models
        .iter()
        .filter_map(|m| {
            m.id.clone().map(|id| WorkerModel {
                provider: m.provider.clone().unwrap_or_default(),
                id,
                name: m.name.clone(),
                thinking_levels: m.thinking_levels(),
                context_window: m.context_window,
                cost: m.cost,
            })
        })
        .collect()
}

/// `get_commands` entries, as the fleet cache stores them. Entries without a
/// name are dropped.
#[must_use]
pub fn worker_commands(entries: &[CommandEntry]) -> Vec<WorkerCommand> {
    entries
        .iter()
        .filter_map(|entry| {
            entry.name.clone().map(|name| WorkerCommand {
                name,
                description: entry.description.clone().unwrap_or_default(),
                source: entry
                    .source
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            })
        })
        .collect()
}

/// The fleet's pi catalogue, fetched from pi on demand when the cache holds
/// no models yet: the one-shot runs `pi --mode rpc --no-session` with the
/// same materialised extension and skill a worker boots with and cwd at the
/// repo root, so the commands it writes are the list a worker would report.
/// The answers are merged into the fleet cache one field at a time, exactly
/// as a monitor writes it — and an empty answer replaces nothing. Returns
/// the models the cache now offers.
///
/// Never an error: the catalogue is derived data, and every failure mode
/// (no pi, an answer that never comes, a write that fails) just reports
/// nothing and leaves any existing cache alone.
pub async fn ensure_pi_catalogue(fleet_dir: &Path, pi_spec: &str) -> Vec<WorkerModel> {
    // the cache is the fast path; only a missing models list is worth the
    // cost of starting pi
    if let Some(cache) = read_pi_cache(fleet_dir)
        && !cache.available_models.is_empty()
    {
        return cache.available_models;
    }
    refresh_pi_catalogue(fleet_dir, pi_spec).await
}

/// Ask pi for its catalogue now, whatever the cache holds, and merge the
/// answers into it one field at a time (an empty answer replaces nothing).
/// The write stamps `fetchedAt`, so a stale catalogue is fresh again after
/// it. Returns the models pi answered with — empty when it could not be
/// asked, which leaves the cache as it was.
pub async fn refresh_pi_catalogue(fleet_dir: &Path, pi_spec: &str) -> Vec<WorkerModel> {
    let Some(fresh) = ask_pi_catalogue(fleet_dir, pi_spec).await else {
        return Vec::new();
    };
    if fresh.available_models.is_empty() && fresh.commands.is_empty() {
        return Vec::new();
    }
    // read-modify-write under the cache's lock, one field per answer: a
    // monitor that just wrote a field must not be clobbered, and a field pi
    // did not answer is left as it was
    let models = fresh.available_models.clone();
    let _ = crate::fleet::run::update_pi_cache(fleet_dir, |cache| {
        if !fresh.available_models.is_empty() {
            cache.available_models = fresh.available_models;
        }
        if !fresh.commands.is_empty() {
            cache.commands = fresh.commands;
        }
    });
    models
}

/// One `get_available_models` + `get_commands` round trip against a fresh
/// pi. `--no-session` keeps every byte out of the session store, and the
/// child is killed the moment the answers are in hand or [`LIST_TIMEOUT`]
/// lapses, so no pi lingers on. `None` when pi cannot be asked at all.
async fn ask_pi_catalogue(fleet_dir: &Path, pi_spec: &str) -> Option<PiCache> {
    let paths = FleetPaths::new(fleet_dir);
    let (extension, skill) = materialize_worker_files(&paths).ok()?;
    let mut parts = pi_spec.split_whitespace();
    let bin = parts.next()?;
    let mut command = Command::new(bin);
    command
        .args(parts)
        .arg("--mode")
        .arg("rpc")
        .arg("--no-session")
        .arg("--extension")
        .arg(extension.to_string_lossy().into_owned())
        .arg("--skill")
        .arg(skill.to_string_lossy().into_owned())
        .current_dir(fleet_dir.parent().unwrap_or(fleet_dir))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return None;
    };
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    // the whole exchange is a [`tokio::time::timeout`] future: dropping it
    // (on timeout, or on the way out) drops the child, which `kill_on_drop`
    // turns into a kill
    let Ok(fresh) = tokio::time::timeout(LIST_TIMEOUT, async move {
        let _ = stdin
            .write_all(b"{\"type\":\"get_available_models\"}\n{\"type\":\"get_commands\"}\n")
            .await;
        // ending stdin tells pi to exit once it has answered, which ends
        // the read below even when an answer never comes
        drop(stdin);
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut buf = Vec::new();
        let mut models: Vec<ModelRef> = Vec::new();
        let mut commands: Vec<CommandEntry> = Vec::new();
        let mut got_models = false;
        let mut got_commands = false;
        while !got_models || !got_commands {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let mut line = String::from_utf8_lossy(&buf).into_owned();
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            if let Some(RpcMessage::Response(response)) = parse_line(&line) {
                let command = response.command.as_deref().unwrap_or("").to_string();
                let success = response.success == Some(true);
                match (command.as_str(), success) {
                    ("get_available_models", true) => {
                        models = response.available_models();
                        got_models = true;
                    }
                    ("get_commands", true) => {
                        commands = response.commands();
                        got_commands = true;
                    }
                    _ => {}
                }
            }
        }
        if !got_models && !got_commands {
            return None;
        }
        Some(PiCache {
            fetched_at: crate::util::now_iso(),
            available_models: worker_models(&models),
            commands: worker_commands(&commands),
        })
    })
    .await
    else {
        return None;
    };
    fresh
}

/// Model names pi reports, or an empty list when it cannot be asked.
pub async fn list_models(pi_bin: &str) -> Vec<String> {
    let cached = CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(pi_bin)
        .cloned();
    if let Some(cached) = cached {
        return cached;
    }
    let mut argv = pi_bin.split_whitespace();
    let Some(bin) = argv.next() else {
        return Vec::new();
    };
    let mut command = tokio::process::Command::new(bin);
    command.args(argv).arg("--list-models");
    let Ok(Ok(output)) = tokio::time::timeout(LIST_TIMEOUT, command.output()).await else {
        // Spawn failure, or the child outlived its welcome (dropping the
        // future kills it): either way pi is unaskable.
        return Vec::new();
    };
    // "provider  model  context  max-out  thinking  images" — the name is the
    // second whitespace column, under a header line.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for line in stdout.lines().skip(1) {
        if let Some(name) = line.split_whitespace().nth(1)
            && seen.insert(name.to_string())
        {
            names.push(name.to_string());
        }
    }
    if !names.is_empty() {
        CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pi_bin.to_string(), names.clone());
    }
    names
}

/// `None` when the model is fine or pi cannot be asked; otherwise a message
/// naming the closest models, so the caller can pick one straight away.
///
/// # Errors
///
/// Never today: an unaskable pi reports `Ok(None)`, and the signature keeps
/// room for a hard failure in the listing path.
pub async fn check_model(pi_bin: &str, pattern: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(pattern) = pattern.filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let models = list_models(pi_bin).await;
    // An empty listing means pi could not be asked, which is not the user's problem.
    if models.is_empty() || models.iter().any(|m| m == pattern) {
        return Ok(None);
    }
    let near = closest(pattern, &models);
    let suffix = if near.is_empty() {
        String::new()
    } else {
        format!("; did you mean {}?", near.join(", "))
    };
    Ok(Some(format!(
        "unknown model \"{pattern}\"{suffix} (pi --list-models shows all {})",
        models.len()
    )))
}

/// Models whose name contains, or is contained by, what was asked for.
fn closest(model: &str, models: &[String]) -> Vec<String> {
    let wanted = model.to_lowercase();
    let mut scored: Vec<String> = models
        .iter()
        .filter(|m| {
            let name = m.to_lowercase();
            name.contains(&wanted) || wanted.contains(&name) || shares_stem(&name, &wanted)
        })
        .cloned()
        .collect();
    scored.sort_by_key(|m| m.len().abs_diff(model.len()));
    scored.truncate(3);
    scored
}

/// "glm-5.3-max" and "glm-5.3" share a stem; "glm-5.3-max" and "gpt-6" do not.
fn shares_stem(a: &str, b: &str) -> bool {
    let stem = |s: &str| -> String { s.split(['-', '/']).take(2).collect::<Vec<_>>().join("-") };
    stem(a) == stem(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::{PiCache, WorkerCommand, read_pi_cache, write_pi_cache};
    use crate::util::new_id;
    use std::fmt::Write as _;
    use std::path::PathBuf;

    fn write_fake_pi(body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pilotfish-models-{}-{}",
            std::process::id(),
            new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-pi.sh");
        std::fs::write(&script, body).unwrap();
        script
    }

    fn tmp_fleet() -> FleetPaths {
        let dir = std::env::temp_dir().join(format!(
            "pilotfish-catalogue-{}-{}",
            std::process::id(),
            new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        FleetPaths::new(dir)
    }

    /// A pi spec pointing at the real fake pi fixture, through a `sh` wrapper
    /// (the spec splits on spaces, so a script is how it travels). With
    /// `drop_commands`, `get_commands` lines never reach the fake.
    fn rpc_fake_pi(drop_commands: bool) -> String {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake-pi-pilotfish.mjs");
        let mut body = String::from("#!/bin/sh\n");
        if drop_commands {
            body.push_str(
                "{ while IFS= read -r line || [ -n \"$line\" ]; do\n  case \"$line\" in\n    *get_commands*) ;;\n    *) printf '%s\\n' \"$line\" ;;\n  esac\ndone } | "
            );
        }
        let _ = writeln!(body, "node {}\n", fixture.to_string_lossy().into_owned());
        format!("sh {}", write_fake_pi(&body).display())
    }

    fn listing_script(rows: &[&str]) -> String {
        let mut body = String::from(
            "#!/bin/sh\ncase \" $* \" in *--list-models*)\n  echo 'provider           model                context'\n",
        );
        for r in rows {
            let _ = writeln!(body, "  echo '{r}'");
        }
        body.push_str(";;\nesac\n");
        write_fake_pi(&body).to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn model_check() {
        {
            let pi = listing_script(&[
                "  fake               glm-5.3                1M",
                "  fake               glm-5.3                1M",
                "  fake               glm-5.3-flash                1M",
            ]);
            let pi = format!("sh {pi}");
            let models = list_models(&pi).await;
            assert_eq!(models, vec!["glm-5.3", "glm-5.3-flash"]);
            // The listing is cached per pi spec: a second ask does not run pi again.
            let again = list_models(&pi).await;
            assert_eq!(again, models);
        }
        {
            let pi = format!(
                "sh {}",
                listing_script(&[
                    "  fake               glm-5.3                1M",
                    "  fake               glm-5.3-flash                1M",
                    "  fake               claude-sonnet-5                1M",
                ])
            );
            // prime the cache; the checks below then read the one real listing
            list_models(&pi).await;
            assert_eq!(check_model(&pi, None).await.unwrap(), None);
            assert_eq!(check_model(&pi, Some("glm-5.3")).await.unwrap(), None);
            let bad = check_model(&pi, Some("glm-5.3-max"))
                .await
                .unwrap()
                .unwrap();
            assert!(bad.contains("unknown model \"glm-5.3-max\""), "{bad}");
            assert!(bad.contains("did you mean glm-5.3-flash, glm-5.3"), "{bad}");
            assert!(bad.contains("shows all 3"), "{bad}");
            // Nothing similar at all: still refused, no suggestions.
            let alien = check_model(&pi, Some("zort-9000")).await.unwrap().unwrap();
            assert!(alien.contains("unknown model \"zort-9000\""), "{alien}");
            assert!(!alien.contains("did you mean"), "{alien}");
        }
        {
            assert_eq!(
                list_models("definitely-not-a-real-pi-bin").await,
                Vec::<String>::new()
            );
            assert_eq!(
                check_model("definitely-not-a-real-pi-bin", Some("anything-at-all"))
                    .await
                    .unwrap(),
                None
            );
        }
        {
            assert!(shares_stem("glm-5.3", "glm-5.3-max"));
            assert!(shares_stem("glm/5.3", "glm-5.3-max"));
            assert!(!shares_stem("glm-5.3-max", "gpt-6"));
        }
    }

    #[tokio::test]
    async fn catalogue_fetch() {
        {
            let fleet = tmp_fleet();
            let pi = rpc_fake_pi(false);
            let models = ensure_pi_catalogue(fleet.root(), &pi).await;
            assert_eq!(models.len(), 2);
            let cache = read_pi_cache(fleet.root()).unwrap();
            assert_eq!(
                cache
                    .available_models
                    .iter()
                    .map(|m| format!("{}:{}", m.provider, m.id))
                    .collect::<Vec<_>>(),
                vec!["fakeprovider:glm-5.3", "fakeprovider:glm-5.3-flash"],
                "the one-shot writes what the fake pi answered"
            );
            assert_eq!(
                cache
                    .commands
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>(),
                vec!["skill:fleet-worker-report", "compact-notes", "session-name"],
                "and the commands with it, like a booted worker would"
            );
            // a catalogue already on disk is never re-fetched
            assert_eq!(
                ensure_pi_catalogue(fleet.root(), "no-such-pi").await,
                models
            );
        }
        {
            let fleet = tmp_fleet();
            let kept = vec![WorkerCommand {
                name: "keep-me".to_string(),
                description: "kept".to_string(),
                source: "test".to_string(),
            }];
            write_pi_cache(
                fleet.root(),
                &PiCache {
                    available_models: Vec::new(),
                    commands: kept.clone(),
                    ..PiCache::default()
                },
            )
            .unwrap();
            // this pi answers models but never answers get_commands: the models
            // land and the pre-existing commands survive the read-modify-write
            let models = ensure_pi_catalogue(fleet.root(), &rpc_fake_pi(true)).await;
            assert_eq!(models.len(), 2);
            let cache = read_pi_cache(fleet.root()).unwrap();
            assert_eq!(cache.available_models.len(), 2);
            assert_eq!(cache.commands, kept);
        }
        {
            let fleet = tmp_fleet();
            assert!(
                ensure_pi_catalogue(fleet.root(), "definitely-not-a-real-pi-bin")
                    .await
                    .is_empty()
            );
            assert!(
                !crate::fleet::run::pi_cache_json_path(fleet.root()).exists(),
                "no catalogue file is written when pi cannot be asked"
            );
        }
    }
}
