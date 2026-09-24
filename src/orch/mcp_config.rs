//! The `.mcp.json` handed to claude: one stdio MCP server named `fleet`,
//! running this binary — the tools stay `mcp__fleet__*`.
//!
//! Ported from the TypeScript `src/orchestrator/mcpConfig.ts`. The TypeScript
//! needed interpreter indirection (tsx vs the built bundle); the Rust binary
//! is simply `std::env::current_exe()` with `["mcp"]`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Map, Value};

use crate::paths::env_var;

/// Per-server tool-call timeout: above fleet_wait's 600 s cap so long waits
/// are not cut off.
pub const FLEET_MCP_TIMEOUT_MS: u64 = 660_000;

/// The `mcp__<server>__<tool>` server name claude gives the fleet tools.
pub const FLEET_MCP_SERVER_NAME: &str = "fleet";

/// The allowlist pattern matching every fleet tool (pinned by test).
pub const FLEET_TOOLS_ALLOW_PATTERN: &str = "mcp__fleet__*";

/// The basics an MCP stdio server needs: it is spawned with exactly the
/// environment given here (the client does not merge the parent's), so PATH
/// and HOME are required for `git`, TMPDIR and the locale for everything
/// else. Deliberately narrow — the config travels on claude's command line,
/// so no secrets go in it.
const BASE_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "USER",
    "SHELL",
    "SystemRoot",
    "APPDATA",
];

/// Dev/test knobs, so workers spawned through MCP behave like ones spawned
/// from the CLI under test.
const PILOTFISH_ENV_KEYS: &[&str] = &[
    "PILOTFISH_DEV",
    "PILOTFISH_PI_BIN",
    "PILOTFISH_ASK_POLL_MS",
    "PILOTFISH_ASK_TIMEOUT_MS",
];

/// This binary, as the MCP config should spawn it.
///
/// The `pilotfish mcp` server: no shell is involved (argv arrays), so paths with
/// spaces need no quoting.
///
/// # Errors
///
/// Returns an error when the current executable cannot be located.
pub fn pilotfish_binary() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("locate the pilotfish binary for the fleet MCP server")
}

/// The `--mcp-config` document that makes claude spawn `pilotfish mcp` over stdio,
/// with the environment inherited from this process.
///
/// # Errors
///
/// Returns an error when the current executable cannot be located.
pub fn fleet_mcp_config(binary: &Path, fleet_dir: &Path) -> anyhow::Result<Value> {
    Ok(fleet_mcp_config_with(binary, fleet_dir, &|key| {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    }))
}

/// [`fleet_mcp_config`] against an explicit environment (tests).
#[must_use]
pub fn fleet_mcp_config_with_env(
    binary: &Path,
    fleet_dir: &Path,
    env: &HashMap<String, String>,
) -> Value {
    fleet_mcp_config_with(binary, fleet_dir, &|key| {
        env.get(key).filter(|value| !value.is_empty()).cloned()
    })
}

fn fleet_mcp_config_with(
    binary: &Path,
    fleet_dir: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Value {
    let mut passthrough = Map::new();
    passthrough.insert(
        env_var("DIR"),
        Value::String(fleet_dir.to_string_lossy().into_owned()),
    );
    for key in BASE_ENV_KEYS.iter().chain(PILOTFISH_ENV_KEYS.iter()) {
        if let Some(value) = lookup(key) {
            passthrough.insert((*key).to_string(), Value::String(value));
        }
    }

    let mut fleet = Map::new();
    fleet.insert("type".into(), Value::String("stdio".into()));
    fleet.insert(
        "command".into(),
        Value::String(binary.to_string_lossy().into_owned()),
    );
    fleet.insert(
        "args".into(),
        Value::Array(vec![Value::String("mcp".into())]),
    );
    fleet.insert("env".into(), Value::Object(passthrough));
    fleet.insert("timeout".into(), Value::from(FLEET_MCP_TIMEOUT_MS));

    let mut servers = Map::new();
    servers.insert(FLEET_MCP_SERVER_NAME.to_string(), Value::Object(fleet));
    let mut root = Map::new();
    root.insert("mcpServers".into(), Value::Object(servers));
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn mcp_config() {
        {
            let env = env_of(&[
                ("PILOTFISH_DEV", "1"),
                ("PILOTFISH_PI_BIN", "node fake.mjs"),
                ("HOME", "/h"),
                ("PATH", "/bin"),
                ("ANTHROPIC_API_KEY", "secret"),
            ]);
            let cfg = fleet_mcp_config_with_env(
                Path::new("/usr/local/bin/pilotfish"),
                Path::new("/repo/.pilotfish"),
                &env,
            );
            let fleet = &cfg["mcpServers"]["fleet"];
            assert_eq!(fleet["type"], "stdio");
            assert_eq!(fleet["command"], "/usr/local/bin/pilotfish");
            assert_eq!(fleet["args"], json!(["mcp"]));
            assert_eq!(
                fleet["env"],
                json!({
                    "PILOTFISH_DIR": "/repo/.pilotfish",
                    "PATH": "/bin",
                    "HOME": "/h",
                    "PILOTFISH_DEV": "1",
                    "PILOTFISH_PI_BIN": "node fake.mjs",
                })
            );
            assert!(
                fleet["env"].get("ANTHROPIC_API_KEY").is_none(),
                "no secrets on claude's command line"
            );
            assert_eq!(fleet["timeout"], json!(FLEET_MCP_TIMEOUT_MS));
            assert_eq!(FLEET_TOOLS_ALLOW_PATTERN, "mcp__fleet__*");
            assert_eq!(FLEET_MCP_SERVER_NAME, "fleet");
            // valid JSON document end to end
            serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&cfg).unwrap())
                .unwrap();
        }
        {
            let env = env_of(&[("PATH", "/bin"), ("PILOTFISH_DEV", ""), ("TMPDIR", " ")]);
            let cfg = fleet_mcp_config_with_env(
                Path::new("/pilotfish"),
                Path::new("/repo/.pilotfish"),
                &env,
            );
            // TMPDIR is " " — non-empty, so it passes through verbatim; the empty
            // PILOTFISH_DEV does not.
            assert_eq!(
                cfg["mcpServers"]["fleet"]["env"],
                json!({"PILOTFISH_DIR": "/repo/.pilotfish", "PATH": "/bin", "TMPDIR": " "})
            );
        }
    }
}
