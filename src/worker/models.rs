//! `pi --list-models` and model checking: refuse a `--model` pattern pi
//! cannot resolve before a worktree exists, naming the closest models it
//! does have. A worker spawned with a bad model dies a minute later, after a
//! worktree and a branch exist, with the reason buried in its state file —
//! the names are cheap to ask for, so a spawn checks first.
//! (Ported from the TypeScript `src/models.ts`.)

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use crate::paths::env_var;

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
    async fn parses_the_second_column_and_dedupes() {
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

    #[tokio::test]
    async fn known_model_passes_and_unknown_names_the_closest() {
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

    #[tokio::test]
    async fn an_unaskable_pi_never_blocks_a_spawn() {
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

    #[test]
    fn stem_heuristic_matches_version_variants_only() {
        assert!(shares_stem("glm-5.3", "glm-5.3-max"));
        assert!(shares_stem("glm/5.3", "glm-5.3-max"));
        assert!(!shares_stem("glm-5.3-max", "gpt-6"));
    }
}
