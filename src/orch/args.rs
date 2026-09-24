//! Building claude's argv: model, permission mode, budget, remote control,
//! the fleet MCP config, stream-json in/out.
//!
//! Ported from the TypeScript `src/orchestrator/args.ts`; the ordering
//! comment and the deliberately-missing `bypassPermissions` carry over.

use crate::orch::mcp_config::FLEET_TOOLS_ALLOW_PATTERN;
use crate::paths::env_var;

/// `PILOTFISH_CLAUDE_BIN` is an executable spec split on spaces
/// ("node /path/fake-claude.mjs"), so tests can point the orchestrator at a
/// scripted stand-in. Defaults to `claude` on PATH.
#[must_use]
pub fn claude_command() -> (String, Vec<String>) {
    claude_command_from_spec(std::env::var(env_var("CLAUDE_BIN")).ok().as_deref())
}

/// [`claude_command`] for an explicit spec (tests).
#[must_use]
pub fn claude_command_from_spec(spec: Option<&str>) -> (String, Vec<String>) {
    let raw = spec
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("claude");
    let mut parts = raw.split(' ').filter(|p| !p.is_empty());
    let bin = parts.next().unwrap_or("claude").to_string();
    (bin, parts.map(str::to_string).collect())
}

/// Pre-approved: every fleet tool, and read-only git. Everything else prompts
/// in the TUI.
pub const DEFAULT_ALLOWED_TOOLS: &[&str] = &[
    FLEET_TOOLS_ALLOW_PATTERN,
    "Bash(git diff *)",
    "Bash(git log *)",
    "Bash(git status *)",
    "Bash(git branch *)",
    "Bash(git show *)",
];

/// The orchestrator coordinates; it never edits.
pub const DEFAULT_DISALLOWED_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit"];

/// Permission modes worth offering. `bypassPermissions` is deliberately
/// absent: it needs an extra dangerous flag and would silence the approval
/// overlay.
pub const PERMISSION_MODES: &[&str] = &["default", "auto", "acceptEdits", "dontAsk", "plan"];

/// One line on what a mode means, for the picker.
#[must_use]
pub fn describe_permission_mode(mode: &str) -> &'static str {
    match mode {
        "auto" => "a classifier approves routine actions; the rest still ask you",
        "acceptEdits" => "file edits and common filesystem commands go through without asking",
        "dontAsk" => "nothing is asked: anything not already allowed is denied",
        "plan" => "read-only planning; no tool may change anything",
        _ => "every action outside the allowlist asks you",
    }
}

/// Everything [`build_claude_args`] needs beyond what the process owns.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClaudeArgsOptions {
    /// Rendered orchestrator prompt, handed to --append-system-prompt-file.
    pub prompt_file: String,
    /// JSON document for --mcp-config (see `fleet_mcp_config`).
    pub mcp_config_json: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub resume_session_id: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    /// Starting permission mode; without one claude uses its own default for -p.
    pub permission_mode: Option<String>,
    /// Register the session with Claude Code's Remote Control; an empty name
    /// means "on, name it yourself". `None` leaves it off.
    pub remote_control: Option<String>,
}

/// The exact argv for the orchestrator child (`claude` itself is the command).
#[must_use]
pub fn build_claude_args(o: &ClaudeArgsOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--replay-user-messages",
        "--permission-prompt-tool",
        "stdio",
        "--append-system-prompt-file",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    args.push(o.prompt_file.clone());
    args.push("--mcp-config".into());
    args.push(o.mcp_config_json.clone());
    args.push("--strict-mcp-config".into());
    if let Some(mode) = &o.permission_mode {
        args.push("--permission-mode".into());
        args.push(mode.clone());
    }
    if let Some(name) = &o.remote_control {
        // an empty name means "on, name it yourself"
        args.push("--remote-control".into());
        if !name.is_empty() {
            args.push(name.clone());
        }
    }
    if let Some(model) = &o.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if let Some(effort) = &o.effort {
        args.push("--effort".into());
        args.push(effort.clone());
    }
    if let Some(session) = &o.resume_session_id {
        args.push("--resume".into());
        args.push(session.clone());
    }
    if let Some(budget) = o.max_budget_usd
        && budget > 0.0
    {
        args.push("--max-budget-usd".into());
        args.push(format!("{budget}"));
    }
    // variadic lists go last so they cannot swallow a following option's value
    let disallowed = o.disallowed_tools.clone().unwrap_or_else(|| {
        DEFAULT_DISALLOWED_TOOLS
            .iter()
            .map(ToString::to_string)
            .collect()
    });
    if !disallowed.is_empty() {
        args.push("--disallowedTools".into());
        args.extend(disallowed);
    }
    let allowed = o.allowed_tools.clone().unwrap_or_else(|| {
        DEFAULT_ALLOWED_TOOLS
            .iter()
            .map(ToString::to_string)
            .collect()
    });
    if !allowed.is_empty() {
        args.push("--allowedTools".into());
        args.extend(allowed);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    #[test]
    fn build_claude_args_produces_the_exact_orchestrator_flag_set() {
        let args = build_claude_args(&ClaudeArgsOptions {
            prompt_file: "/p.md".into(),
            mcp_config_json: r#"{"mcpServers":{}}"#.into(),
            ..ClaudeArgsOptions::default()
        });
        assert_eq!(
            &args[..5],
            &[
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json"
            ]
        );
        for flag in [
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            "--strict-mcp-config",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag}");
        }
        assert_eq!(value_of(&args, "--permission-prompt-tool"), Some("stdio"));
        assert_eq!(
            value_of(&args, "--append-system-prompt-file"),
            Some("/p.md")
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                value_of(&args, "--mcp-config").unwrap_or_default()
            )
            .unwrap(),
            serde_json::json!({"mcpServers": {}})
        );
        assert!(!args.iter().any(|a| a == "--resume"));
        assert!(!args.iter().any(|a| a == "--model"));
        let d = args.iter().position(|a| a == "--disallowedTools").unwrap();
        assert_eq!(
            &args[d + 1..d + 1 + DEFAULT_DISALLOWED_TOOLS.len()],
            &["Edit", "Write", "NotebookEdit"]
        );
        let a = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert!(a > d, "allowed list is last");
        assert_eq!(&args[a + 1..], DEFAULT_ALLOWED_TOOLS);
        assert_eq!(DEFAULT_ALLOWED_TOOLS[0], "mcp__fleet__*");
    }

    #[test]
    fn optional_flags_appear_only_when_set() {
        let full = build_claude_args(&ClaudeArgsOptions {
            prompt_file: "/p.md".into(),
            mcp_config_json: "{}".into(),
            model: Some("sonnet".into()),
            resume_session_id: Some("sess-1".into()),
            max_budget_usd: Some(2.5),
            effort: Some("high".into()),
            allowed_tools: Some(vec!["X".into()]),
            disallowed_tools: Some(Vec::new()),
            permission_mode: Some("plan".into()),
            remote_control: Some(String::new()),
        });
        assert_eq!(value_of(&full, "--model"), Some("sonnet"));
        assert_eq!(value_of(&full, "--resume"), Some("sess-1"));
        assert_eq!(value_of(&full, "--max-budget-usd"), Some("2.5"));
        assert_eq!(value_of(&full, "--effort"), Some("high"));
        assert_eq!(value_of(&full, "--permission-mode"), Some("plan"));
        // empty remote control name: the flag with no value after it
        let rc = full.iter().position(|a| a == "--remote-control").unwrap();
        assert_eq!(full.get(rc + 1).map(String::as_str), Some("--model"));
        assert!(!full.iter().any(|a| a == "--disallowedTools"));
        assert_eq!(
            &full[full.iter().position(|a| a == "--allowedTools").unwrap() + 1..],
            &["X"]
        );
        // a zero budget is skipped, as in TypeScript
        let zero = build_claude_args(&ClaudeArgsOptions {
            prompt_file: "/p.md".into(),
            mcp_config_json: "{}".into(),
            max_budget_usd: Some(0.0),
            ..ClaudeArgsOptions::default()
        });
        assert!(!zero.iter().any(|a| a == "--max-budget-usd"));
    }

    #[test]
    fn claude_command_splits_the_bin_spec() {
        let (bin, prefix) = claude_command_from_spec(Some("node /x/fake.mjs"));
        assert_eq!(bin, "node");
        assert_eq!(prefix, vec!["/x/fake.mjs"]);
        let (bin, prefix) = claude_command_from_spec(None);
        assert_eq!(bin, "claude");
        assert!(prefix.is_empty());
        let (bin, prefix) = claude_command_from_spec(Some(""));
        assert_eq!(bin, "claude");
        assert!(prefix.is_empty());
    }

    #[test]
    fn permission_modes_are_offered_and_described() {
        assert_eq!(
            PERMISSION_MODES,
            &["default", "auto", "acceptEdits", "dontAsk", "plan"]
        );
        // bypassPermissions is deliberately absent
        assert!(!PERMISSION_MODES.contains(&"bypassPermissions"));
        assert_eq!(
            describe_permission_mode("acceptEdits"),
            "file edits and common filesystem commands go through without asking"
        );
        assert_eq!(
            describe_permission_mode("anything"),
            describe_permission_mode("default")
        );
    }
}
