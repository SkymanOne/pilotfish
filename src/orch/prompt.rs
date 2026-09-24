//! The orchestrator's system prompt travels inside the binary: nothing is
//! ever copied into a project.
//!
//! The shipped template ([`ORCHESTRATOR_PROMPT_TEMPLATE`], embedded with
//! `include_str!`) is rendered with the fleet's placeholders and written under
//! the orchestrator directory for `--append-system-prompt-file`. Overrides,
//! in order: `$PILOTFISH_PROMPT` (a path; a dangling one is an error, it is
//! explicit user intent), then `<repo>/.pilotfish/orchestrator.md`, then
//! `~/.pilotfish/orchestrator.md`, then the embedded copy. The legacy
//! `~/.config/parl/orchestrator.md` is no longer read; when only that file
//! exists, resolution warns on stderr and names both paths, so a user with an
//! existing file is told to move it rather than silently ignored. Unknown
//! `{{PLACEHOLDER}}`s are left untouched.

use std::path::{Path, PathBuf};

use crate::orch::records::prompt_path;
use crate::paths::{BIN_NAME, STATE_DIR_NAME, env_var};

/// The shipped template, embedded in the binary.
pub const ORCHESTRATOR_PROMPT_TEMPLATE: &str = include_str!("../../prompts/orchestrator.md");

/// Workers that may run at once when nothing more specific is configured.
/// An alias of the enforcement side's constant (`[limits]
/// max_workers_per_session` in `~/.pilotfish/config.toml` backstops `spawn`'s
/// refusal): the prompt's advice and the enforced cap share one source of
/// truth, so they cannot drift apart.
pub const DEFAULT_MAX_WORKERS: usize = crate::paths::DEFAULT_MAX_WORKERS_PER_SESSION;

/// What fills the template's `{{PLACEHOLDER}}`s.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptVars {
    pub fleet_dir: String,
    pub repo_root: String,
    pub max_workers: Option<usize>,
    pub bin_name: Option<String>,
}

/// Render the shipped template for a fleet.
#[must_use]
pub fn render_orchestrator_prompt(vars: &PromptVars) -> String {
    render_prompt_template(ORCHESTRATOR_PROMPT_TEMPLATE, vars)
}

/// Fill a template's known `{{PLACEHOLDER}}`s; unknown ones stay as written.
///
/// Same scan as the TypeScript: a regex over `{{[A-Z_]+}}`, so a known
/// placeholder is replaced even when it sits inside an unclosed `{{` block.
#[must_use]
pub fn render_prompt_template(template: &str, vars: &PromptVars) -> String {
    let values = [
        ("FLEET_DIR", vars.fleet_dir.clone()),
        ("REPO_ROOT", vars.repo_root.clone()),
        (
            "MAX_WORKERS",
            vars.max_workers.unwrap_or(DEFAULT_MAX_WORKERS).to_string(),
        ),
        (
            "BIN_NAME",
            vars.bin_name
                .clone()
                .unwrap_or_else(|| BIN_NAME.to_string()),
        ),
    ];
    let Some(re) = placeholder_regex() else {
        return template.to_string();
    };
    re.replace_all(template, |caps: &regex::Captures| {
        let key = caps
            .get(1)
            .map_or(String::new(), |m| m.as_str().to_string());
        values.iter().find(|(k, _)| *k == key).map_or_else(
            || {
                caps.get(0)
                    .map_or(String::new(), |m| m.as_str().to_string())
            },
            |(_, value)| value.clone(),
        )
    })
    .into_owned()
}

fn placeholder_regex() -> Option<&'static regex::Regex> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\{\{([A-Z_]+)\}\}").ok())
        .as_ref()
}

/// Where the prompt comes from, in override order; `None` means the embedded
/// copy. An explicit `$PILOTFISH_PROMPT` that does not exist is an error — it is
/// user intent, and silently falling back would hide the mistake.
///
/// The env value and user config dir are injectable (tests).
///
/// # Errors
///
/// Returns an error when `$PILOTFISH_PROMPT` points at something that is not a file.
pub fn resolve_prompt_source(
    pilotfish_prompt: Option<&str>,
    repo_root: &Path,
    user_dir: Option<&Path>,
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(path) = pilotfish_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(Some(path));
        }
        anyhow::bail!(
            "$PILOTFISH_PROMPT is set to {}, which is not a file",
            path.display()
        );
    }
    let repo_override = repo_root.join(STATE_DIR_NAME).join("orchestrator.md");
    if repo_override.is_file() {
        return Ok(Some(repo_override));
    }
    if let Some(user_dir) = user_dir {
        let user_override = user_dir.join("orchestrator.md");
        if user_override.is_file() {
            return Ok(Some(user_override));
        }
    }
    Ok(None)
}

/// The legacy `~/.config/parl/orchestrator.md`, when it exists and the new
/// `~/.pilotfish/orchestrator.md` does not — the prompt moved, and a user who
/// only has the old file should be told, not silently ignored.
#[must_use]
pub fn legacy_config_prompt(home: Option<&Path>, user_dir: Option<&Path>) -> Option<PathBuf> {
    let legacy = home?.join(".config").join("parl").join("orchestrator.md");
    if !legacy.is_file() {
        return None;
    }
    let moved = user_dir
        .map(|dir| dir.join("orchestrator.md"))
        .is_some_and(|path| path.is_file());
    (!moved).then_some(legacy)
}

/// [`resolve_prompt_source`] against the real environment; warns on stderr
/// when only the legacy `~/.config/parl` prompt exists.
///
/// # Errors
///
/// Returns an error when `$PILOTFISH_PROMPT` points at something that is not a file.
pub fn prompt_source(repo_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir();
    let user_config_dir = crate::paths::user_dir();
    let resolved = resolve_prompt_source(
        std::env::var(env_var("PROMPT")).ok().as_deref(),
        repo_root,
        user_config_dir.as_deref(),
    )?;
    if let Some(legacy) = legacy_config_prompt(home.as_deref(), user_config_dir.as_deref()) {
        let target = user_config_dir.as_ref().map_or_else(
            || "~/.pilotfish/orchestrator.md".to_string(),
            |dir| dir.join("orchestrator.md").display().to_string(),
        );
        eprintln!(
            "warning: the orchestrator prompt moved — {} is no longer read; move it to {target}",
            legacy.display()
        );
    }
    Ok(resolved)
}

/// Render the prompt for the fleet rooted at `fleet_dir` working in
/// `repo_root`, substituting the same per-session worker cap `spawn`
/// enforces: `~/.pilotfish/config.toml`'s `[limits] max_workers_per_session`,
/// or the shared default when unset.
///
/// # Errors
///
/// Returns an error when the resolved override cannot be read or the user
/// config is malformed.
pub fn render_prompt(fleet_dir: &Path, repo_root: &Path) -> anyhow::Result<String> {
    render_prompt_with_user_dir(fleet_dir, repo_root, crate::paths::user_dir().as_deref())
}

/// [`render_prompt`] with the user config dir injected (tests), so a test
/// never resolves the ambient `~/.pilotfish`.
///
/// # Errors
///
/// Returns an error when the resolved override cannot be read or the user
/// config is malformed.
pub fn render_prompt_with_user_dir(
    fleet_dir: &Path,
    repo_root: &Path,
    user_config_dir: Option<&Path>,
) -> anyhow::Result<String> {
    let template = match prompt_source(repo_root)? {
        Some(path) => std::fs::read_to_string(&path)?,
        None => ORCHESTRATOR_PROMPT_TEMPLATE.to_string(),
    };
    let max_workers = crate::paths::load_user_config(user_config_dir)?.max_workers_per_session();
    Ok(render_prompt_template(
        &template,
        &PromptVars {
            fleet_dir: fleet_dir.to_string_lossy().into_owned(),
            repo_root: repo_root.to_string_lossy().into_owned(),
            max_workers: Some(max_workers),
            bin_name: None,
        },
    ))
}

/// Render and write to `<fleetDir>/orchestrators/<key>/prompt.md` (what
/// `--append-system-prompt-file` reads); returns the path.
///
/// # Errors
///
/// Returns an error when the prompt cannot be rendered or the file written.
pub fn write_prompt(
    fleet_dir: &Path,
    repo_root: &Path,
    key: &crate::paths::SessionKey,
) -> anyhow::Result<PathBuf> {
    let target = prompt_path(fleet_dir, key);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, render_prompt(fleet_dir, repo_root)?)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_prompt() {
        {
            let text = render_orchestrator_prompt(&PromptVars {
                fleet_dir: "/repo/.pilotfish".into(),
                repo_root: "/repo".into(),
                max_workers: None,
                bin_name: None,
            });
            assert!(!text.contains("{{"), "no unrendered placeholders: {text}");
            assert!(text.contains("`/repo/.pilotfish`"), "{text}");
            assert!(text.contains("`/repo`"), "{text}");
            assert!(
                text.contains(&format!("At most {DEFAULT_MAX_WORKERS} workers")),
                "{text}"
            );
            assert!(text.contains(&format!("`{BIN_NAME}`")), "{text}");
            for tool in [
                "fleet_spawn",
                "fleet_status",
                "fleet_wait",
                "fleet_output",
                "fleet_logs",
                "fleet_send",
                "fleet_followup",
                "fleet_answer",
                "fleet_stop",
                "fleet_report",
                "fleet_diff",
                "fleet_merge",
                "fleet_cleanup",
            ] {
                assert!(text.contains(&format!("`{tool}`")), "mentions {tool}");
            }
            assert!(text.contains(r#"<fleet-event kind="settled""#), "{text}");
            for kind in [
                "settled",
                "stopped",
                "error",
                "dead",
                "question",
                "answered_by_console",
                "question_resolved",
                "console_steer",
                "progress",
                "snapshot",
            ] {
                assert!(
                    text.contains(&format!("`{kind}`")),
                    "explains event kind {kind}"
                );
            }
            assert!(
                text.contains("Never merge a run that is not `settled`"),
                "{text}"
            );
            assert!(text.contains("Never edit files yourself"), "{text}");
            assert!(text.contains("AskUserQuestion"), "{text}");
            assert!(text.contains("exit 5"), "{text}");
            // the rewritten facts: pilotfish branch prefix and the new report layout
            assert!(text.contains("`pilotfish/<name>-<7 chars>`"), "{text}");
            assert!(text.contains("runs/<runId>/report.md"), "{text}");
            assert!(!text.contains("pi-fleet"), "{text}");
            assert!(!text.contains(".pi-fleet"), "{text}");
        }
        {
            let text = render_prompt_template(
                "{{BIN_NAME}} {{MAX_WORKERS}} {{FLEET_DIR}} {{REPO_ROOT}} {{UNKNOWN}}",
                &PromptVars {
                    fleet_dir: "/f".into(),
                    repo_root: "/r".into(),
                    max_workers: Some(5),
                    bin_name: Some("fleetx".into()),
                },
            );
            assert_eq!(text, "fleetx 5 /f /r {{UNKNOWN}}");
        }
        {
            // {{Foo}} and {{UNCLOSED never match the scan; {{NOPE}} and {{A_B}} are
            // well-shaped but unknown; the nested {{FLEET_DIR}} inside the unclosed
            // block is well-shaped and known, so it renders — as the regex scan
            // always did.
            let out = render_prompt_template(
                "a {{Foo}} b {{NOPE}} c {{A_B}} {{UNCLOSED d {{FLEET_DIR}} e",
                &PromptVars {
                    fleet_dir: "/f".into(),
                    repo_root: "/r".into(),
                    max_workers: None,
                    bin_name: None,
                },
            );
            assert_eq!(out, "a {{Foo}} b {{NOPE}} c {{A_B}} {{UNCLOSED d /f e");
        }
        {
            let root = std::env::temp_dir().join(format!(
                "pilotfish-prompt-cap-{}-{}",
                std::process::id(),
                crate::util::new_id("t").replace('_', "")
            ));
            std::fs::create_dir_all(root.join(STATE_DIR_NAME)).unwrap();
            std::fs::create_dir_all(root.join("user")).unwrap();
            let fleet_dir = root.join(STATE_DIR_NAME);

            // No user config anywhere: the shared default, identical to the
            // enforcement constant.
            let text = render_prompt_with_user_dir(&fleet_dir, &root, None).unwrap();
            assert!(
                text.contains(&format!("At most {DEFAULT_MAX_WORKERS} workers")),
                "{text}"
            );

            // A configured cap flows into the prompt instead of the default, so
            // the agent is told exactly what spawn will refuse.
            let user_dir = root.join("user");
            std::fs::create_dir_all(&user_dir).unwrap();
            std::fs::write(
                user_dir.join("config.toml"),
                "[limits]\nmax_workers_per_session = 5\n",
            )
            .unwrap();
            let text = render_prompt_with_user_dir(&fleet_dir, &root, Some(&user_dir)).unwrap();
            assert!(text.contains("At most 5 workers"), "{text}");
            assert!(
                !text.contains(&format!("At most {DEFAULT_MAX_WORKERS} workers")),
                "the configured cap replaces the default: {text}"
            );
            // The two constants are literally the same value, forever.
            assert_eq!(
                DEFAULT_MAX_WORKERS,
                crate::paths::DEFAULT_MAX_WORKERS_PER_SESSION
            );
        }
    }

    #[test]
    fn prompt_resolution() {
        {
            let repo = std::env::temp_dir().join(format!(
                "pilotfish-prompt-res-{}-{}",
                std::process::id(),
                crate::util::new_id("t").replace('_', "")
            ));
            let pilotfish = repo.join(STATE_DIR_NAME);
            std::fs::create_dir_all(&pilotfish).unwrap();
            // The legacy config home, and the new user-level `~/.pilotfish`.
            let home = std::env::temp_dir().join(format!(
                "pilotfish-prompt-legacy-{}-{}",
                std::process::id(),
                crate::util::new_id("t").replace('_', "")
            ));
            std::fs::create_dir_all(home.join(".config/parl")).unwrap();
            let user_root = std::env::temp_dir().join(format!(
                "pilotfish-prompt-user-{}-{}",
                std::process::id(),
                crate::util::new_id("t").replace('_', "")
            ));
            let user_dir = user_root.join(STATE_DIR_NAME);
            std::fs::create_dir_all(&user_dir).unwrap();

            // nothing anywhere: embedded
            assert_eq!(
                resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
                None
            );
            // ~/.pilotfish next, the new user location
            let user_override = user_dir.join("orchestrator.md");
            std::fs::write(&user_override, "user").unwrap();
            assert_eq!(
                resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
                Some(user_override.clone())
            );
            // the legacy ~/.config/parl location is no longer consulted, even
            // when the new one is empty
            let legacy = home.join(".config/parl/orchestrator.md");
            std::fs::write(&legacy, "legacy").unwrap();
            assert_eq!(
                resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
                Some(user_override)
            );
            // <repo>/.pilotfish beats the user config
            let repo_override = pilotfish.join("orchestrator.md");
            std::fs::write(&repo_override, "repo").unwrap();
            assert_eq!(
                resolve_prompt_source(None, &repo, Some(&user_dir)).unwrap(),
                Some(repo_override)
            );
            // $PILOTFISH_PROMPT beats everything
            let env_file = repo.join("custom-prompt.md");
            std::fs::write(&env_file, "env").unwrap();
            assert_eq!(
                resolve_prompt_source(env_file.to_str(), &repo, Some(&user_dir)).unwrap(),
                Some(env_file)
            );
            // a dangling $PILOTFISH_PROMPT is an error, not a silent fallback
            let err =
                resolve_prompt_source(repo.join("missing.md").to_str(), &repo, Some(&user_dir))
                    .expect_err("dangling override errors");
            assert!(err.to_string().contains("not a file"), "{err}");
        }
        {
            let home = std::env::temp_dir().join(format!(
                "pilotfish-prompt-warn-home-{}-{}",
                std::process::id(),
                crate::util::new_id("t").replace('_', "")
            ));
            let user_root = std::env::temp_dir().join(format!(
                "pilotfish-prompt-warn-user-{}-{}",
                std::process::id(),
                crate::util::new_id("t").replace('_', "")
            ));
            let user_dir = user_root.join(STATE_DIR_NAME);
            std::fs::create_dir_all(home.join(".config/parl")).unwrap();
            std::fs::create_dir_all(&user_dir).unwrap();
            let legacy = home.join(".config/parl/orchestrator.md");

            // no legacy file anywhere: nothing to warn about
            assert_eq!(legacy_config_prompt(Some(&home), Some(&user_dir)), None);
            assert_eq!(legacy_config_prompt(None, Some(&user_dir)), None);
            // legacy exists, the new location does not: the moved file is named
            std::fs::write(&legacy, "old").unwrap();
            assert_eq!(
                legacy_config_prompt(Some(&home), Some(&user_dir)),
                Some(legacy.clone())
            );
            // both exist: the move happened, no warning
            std::fs::write(user_dir.join("orchestrator.md"), "new").unwrap();
            assert_eq!(legacy_config_prompt(Some(&home), Some(&user_dir)), None);
        }
        {
            let root = std::env::temp_dir().join(format!(
                "pilotfish-prompt-{}-{}",
                std::process::id(),
                crate::util::new_id("t").replace('_', "")
            ));
            std::fs::create_dir_all(&root).unwrap();
            let fleet_dir = root.join(STATE_DIR_NAME);
            let key = crate::paths::SessionKey::default();
            let path = write_prompt(&fleet_dir, &root, &key).unwrap();
            assert_eq!(path, prompt_path(&fleet_dir, &key));
            assert!(
                path.starts_with(fleet_dir.join("orchestrators")),
                "{}",
                path.display()
            );
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.starts_with("# Fleet orchestrator"), "{text}");
        }
    }
}
