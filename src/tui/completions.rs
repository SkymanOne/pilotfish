//! Composer completions: slash commands, `@` mentions of workers and
//! repository files. Pure functions over the current input so the popup is
//! testable without a terminal.
//!
//! Ported from the TypeScript `src/tui/completions.ts`, with one addition:
//! `/model` switches the selected session's model live (the orchestrator
//! validates the name itself, so anything typed is accepted).

use crate::util::first_line;

/// What a suggestion is, so the renderer can colour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    Command,
    Agent,
    Worker,
    File,
}

/// One row of the completion popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// Text that replaces the token being completed.
    pub value: String,
    pub label: String,
    pub detail: String,
    pub kind: SuggestionKind,
}

/// The token the cursor completes: where it starts and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionState {
    pub items: Vec<Suggestion>,
    /// Index in the input where the replaced token starts.
    pub start: usize,
    pub token: String,
}

/// One console command the composer offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub detail: &'static str,
    /// Commands that take an argument get a trailing space when accepted.
    pub takes_argument: bool,
    /// Only offered when a worker is selected.
    pub worker_only: bool,
    /// Short forms, e.g. `/q` for `/quit`.
    pub aliases: &'static [&'static str],
}

/// The console's own commands, in palette order. Console-only surface: the
/// agents' own commands ride beside these, never inside them.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/answer",
        detail: "answer the worker's pending question",
        takes_argument: true,
        worker_only: true,
        aliases: &["/a"],
    },
    CommandSpec {
        name: "/followup",
        detail: "queue a message for after its current work",
        takes_argument: true,
        worker_only: true,
        aliases: &["/f"],
    },
    CommandSpec {
        name: "/stop",
        detail: "abort the worker",
        takes_argument: false,
        worker_only: true,
        aliases: &["/s"],
    },
    CommandSpec {
        name: "/remove",
        detail: "remove the worker: worktree, branch, dashboard row",
        takes_argument: false,
        worker_only: true,
        aliases: &["/rm", "/r"],
    },
    CommandSpec {
        name: "/session",
        detail: "switch sessions: /session new [alias], or /session <uuid-or-alias>",
        takes_argument: true,
        worker_only: false,
        aliases: &[],
    },
    CommandSpec {
        name: "/sessions",
        detail: "list every session: alias, short uuid, workers, health",
        takes_argument: false,
        worker_only: false,
        aliases: &[],
    },
    CommandSpec {
        name: "/thinking",
        detail: "set the reasoning level of the selected session",
        takes_argument: true,
        worker_only: false,
        aliases: &["/t"],
    },
    CommandSpec {
        name: "/model",
        detail: "switch the model of the selected session, live",
        takes_argument: true,
        worker_only: false,
        aliases: &[],
    },
    CommandSpec {
        name: "/permissions",
        detail: "how the orchestrator's tool use is approved (auto asks you less)",
        takes_argument: true,
        worker_only: false,
        aliases: &["/perm", "/p"],
    },
    CommandSpec {
        name: "/mouse",
        detail: "release the mouse to select and copy, or take it back for the wheel",
        takes_argument: false,
        worker_only: false,
        aliases: &[],
    },
    CommandSpec {
        name: "/clear",
        detail: "clear this session's transcript from the console (the file is kept)",
        takes_argument: false,
        worker_only: false,
        aliases: &[],
    },
    CommandSpec {
        name: "/routing",
        detail: "model routing: switch it on or off, and set the TypeSafe key it uses",
        takes_argument: false,
        worker_only: false,
        aliases: &["/jev"],
    },
    CommandSpec {
        name: "/verbose",
        detail: "show an older turn's reasoning and tool output in full, or fold it again",
        takes_argument: false,
        worker_only: false,
        aliases: &[],
    },
    CommandSpec {
        name: "/trim",
        detail: "cut the orchestrator's transcript file down to its recent tail",
        takes_argument: false,
        worker_only: false,
        aliases: &[],
    },
    CommandSpec {
        name: "/help",
        detail: "keys and commands",
        takes_argument: false,
        worker_only: false,
        aliases: &["/h", "/?"],
    },
    CommandSpec {
        name: "/quit",
        detail: "close the console (workers keep running)",
        takes_argument: false,
        worker_only: false,
        aliases: &["/q"],
    },
    CommandSpec {
        name: "/shutdown",
        detail: "stop a session's orchestrator and its workers: bare = this one, then exit",
        takes_argument: false,
        worker_only: false,
        aliases: &["/sd"],
    },
];

/// Resolve a typed command word, long form or alias, to its spec.
#[must_use]
pub fn resolve_command(word: &str) -> Option<&'static CommandSpec> {
    let token = word.trim().to_lowercase();
    COMMANDS
        .iter()
        .find(|c| c.name == token || c.aliases.contains(&token.as_str()))
}

/// Everything a command answers to, for matching and for display.
#[must_use]
pub fn command_forms(spec: &CommandSpec) -> String {
    let mut forms = String::from(spec.name);
    for alias in spec.aliases {
        forms.push(' ');
        forms.push_str(alias);
    }
    forms
}

/// The whitespace-delimited token the cursor sits at the end of.
#[must_use]
pub fn active_token(input: &str) -> (&str, usize) {
    let start = input
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(idx, ch)| idx + ch.len_utf8());
    let token = &input[start..];
    (token, start)
}

/// Prefix matches first, then substring matches; both case-insensitive.
/// `keys` are evaluated space-split, so `/quit /q` prefix-matches on either.
pub fn rank<T: Clone>(items: &[T], query: &str, key: impl Fn(&T) -> String) -> Vec<T> {
    if query.is_empty() {
        return items.to_vec();
    }
    let q = query.to_lowercase();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    for item in items {
        let value = key(item).to_lowercase();
        if value.split(' ').any(|form| form.starts_with(&q)) {
            prefix.push(item.clone());
        } else if value.contains(&q) {
            contains.push(item.clone());
        }
    }
    prefix.extend(contains);
    prefix
}

/// A command the underlying agent offers: a claude slash command or skill, or
/// a pi command. Passed through verbatim — never filtered, never hard-coded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommandOption {
    pub name: String,
    pub description: String,
    /// "skill", "prompt", "extension" for pi; the argument hint for claude.
    pub source: Option<String>,
    pub argument_hint: Option<String>,
}

impl AgentCommandOption {
    /// Build from the orchestrator's `AgentCommand` wire shape.
    #[must_use]
    pub fn from_orchestrator(
        name: &str,
        description: Option<&str>,
        argument_hint: Option<&str>,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.unwrap_or_default().to_string(),
            source: None,
            argument_hint: argument_hint.map(str::to_string),
        }
    }

    /// Build from a worker's `WorkerCommand` (pi's `get_commands`).
    #[must_use]
    pub fn from_worker(name: &str, description: &str, source: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            source: Some(source.to_string()),
            argument_hint: None,
        }
    }
}

/// What the composer is aimed at: worker-only commands are hidden otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionTarget {
    #[default]
    Orchestrator,
    Worker,
}

/// Everything `completions_for` needs besides the input.
#[derive(Debug, Clone, Default)]
pub struct CompletionContext {
    pub target: CompletionTarget,
    pub workers: Vec<(String, String)>,
    pub files: Vec<String>,
    /// Commands the selected agent offers, passed through to it verbatim.
    pub agent_commands: Vec<AgentCommandOption>,
}

/// The console's own commands must all fit, with room for the agent's.
pub const MAX_SUGGESTIONS: usize = 24;

/// What to offer for the current input, or `None` when nothing applies.
#[must_use]
pub fn completions_for(input: &str, ctx: &CompletionContext) -> Option<CompletionState> {
    let (token, start) = active_token(input);

    if let Some(query) = token.strip_prefix('/').filter(|_| start == 0) {
        let available: Vec<&CommandSpec> = COMMANDS
            .iter()
            .filter(|c| !c.worker_only || ctx.target == CompletionTarget::Worker)
            .collect();
        // match the long form and every alias, so "/q" finds "/quit"
        let items: Vec<Suggestion> = rank(&available, token, |c| command_forms(c))
            .into_iter()
            .map(|c| Suggestion {
                value: if c.takes_argument {
                    format!("{} ", c.name)
                } else {
                    c.name.to_string()
                },
                label: c.name.to_string(),
                detail: format!("{}  ({})", c.detail, c.aliases.join(", ")),
                kind: SuggestionKind::Command,
            })
            .collect();
        // then whatever the agent itself offers: claude's slash commands and
        // skills, pi's skills, prompt templates and extension commands
        let agent_items: Vec<Suggestion> = rank(&ctx.agent_commands, query, |c| c.name.clone())
            .into_iter()
            // a console command of the same name is the one that runs
            .filter(|c| resolve_command(&format!("/{}", c.name)).is_none())
            .map(|c| Suggestion {
                value: if c.argument_hint.is_some() {
                    format!("/{} ", c.name)
                } else {
                    format!("/{}", c.name)
                },
                label: format!("/{}", c.name),
                detail: [
                    c.description.clone(),
                    c.argument_hint.clone().unwrap_or_default(),
                    c.source
                        .as_ref()
                        .map_or(String::new(), |s| format!("[{s}]")),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("  "),
                kind: SuggestionKind::Agent,
            })
            .collect();
        let mut all = items;
        all.extend(agent_items);
        all.truncate(MAX_SUGGESTIONS);
        return (!all.is_empty()).then_some(CompletionState {
            items: all,
            start,
            token: token.to_string(),
        });
    }

    if let Some(query) = token.strip_prefix('@') {
        let mut items: Vec<Suggestion> = rank(&ctx.workers, query, |w| w.0.clone())
            .into_iter()
            .map(|(name, detail)| Suggestion {
                value: format!("@{name}"),
                label: format!("@{name}"),
                detail,
                kind: SuggestionKind::Worker,
            })
            .collect();
        items.extend(
            rank(&ctx.files, query, Clone::clone)
                .into_iter()
                .map(|file| Suggestion {
                    value: format!("@{file}"),
                    label: format!("@{file}"),
                    detail: String::new(),
                    kind: SuggestionKind::File,
                }),
        );
        items.truncate(MAX_SUGGESTIONS);
        return (!items.is_empty()).then_some(CompletionState {
            items,
            start,
            token: token.to_string(),
        });
    }

    None
}

/// Put the chosen suggestion into the input in place of the token.
#[must_use]
pub fn apply_suggestion(input: &str, state: &CompletionState, suggestion: &Suggestion) -> String {
    format!("{}{}", &input[..state.start], suggestion.value)
}

/// One-line summary of a tool call's arguments, for the `⚙ tool …` line.
#[must_use]
pub fn summarize_args(args: &serde_json::Value) -> String {
    const CLIP: usize = 80;
    let Some(map) = args.as_object() else {
        return String::new();
    };
    // fleet tools take `name`/`target`; pi tools take the others
    let primary = map
        .get("command")
        .or_else(|| map.get("path"))
        .or_else(|| map.get("file_path"))
        .or_else(|| map.get("pattern"))
        .or_else(|| map.get("url"))
        .or_else(|| map.get("name"))
        .or_else(|| map.get("target"));
    let raw = primary.map_or_else(
        || serde_json::to_string(args).unwrap_or_default(),
        |value| {
            value.as_str().map_or_else(
                || serde_json::to_string(value).unwrap_or_default(),
                str::to_string,
            )
        },
    );
    let line = first_line(&raw).trim().to_string();
    if line.chars().count() > CLIP {
        let cut: String = line.chars().take(CLIP - 1).collect();
        format!("{cut}…")
    } else {
        line
    }
}

/// Directories `@` completion never offers.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    ".pilotfish",
    ".pi-fleet",
    ".next",
    "build",
    "coverage",
    "target",
];

/// Repository files for `@` completion: git's list when there is one, else a
/// bounded walk. Errors leave the file list empty; completion is a courtesy.
pub async fn list_repo_files(cwd: &std::path::Path) -> Vec<String> {
    const MAX_FILES: usize = 5000;
    let tracked = crate::git::git_raw(
        &["ls-files", "--cached", "--others", "--exclude-standard"],
        cwd,
    )
    .await;
    if tracked.ok() {
        // a repo without a .gitignore still lists node_modules; nobody wants those
        let files: Vec<String> = tracked
            .stdout
            .lines()
            .filter(|line| !line.is_empty() && !is_noise(line))
            .take(MAX_FILES)
            .map(str::to_string)
            .collect();
        if !files.is_empty() {
            return files;
        }
    }
    let mut out = Vec::new();
    walk(cwd, cwd, 0, MAX_FILES, &mut out);
    out
}

fn is_noise(file: &str) -> bool {
    file.split('/').any(|part| SKIP_DIRS.contains(&part))
}

fn walk(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    max: usize,
    out: &mut Vec<String>,
) {
    if depth > 4 || out.len() >= max {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let full = entry.path();
        let relative = full
            .strip_prefix(root)
            .unwrap_or(&full)
            .to_string_lossy()
            .into_owned();
        if file_type.is_dir() {
            walk(root, &full, depth + 1, max, out);
        } else if out.len() < max {
            out.push(relative);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(target: CompletionTarget) -> CompletionContext {
        CompletionContext {
            target,
            ..CompletionContext::default()
        }
    }

    #[test]
    fn console_commands_carry_their_aliases_and_argument_hints() {
        let quit = resolve_command("/q").unwrap();
        assert_eq!(quit.name, "/quit");
        assert_eq!(resolve_command("/rm").unwrap().name, "/remove");
        assert_eq!(resolve_command("/shutdown").unwrap().name, "/shutdown");
        assert_eq!(resolve_command("/nope"), None);
        assert!(resolve_command("/model").unwrap().takes_argument);
    }

    #[test]
    fn worker_only_commands_hide_on_the_orchestrator() {
        let input = "/";
        let state = completions_for(input, &ctx(CompletionTarget::Orchestrator)).unwrap();
        let labels: Vec<&str> = state.items.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"/help"));
        assert!(!labels.contains(&"/answer"), "worker-only: {labels:?}");
        assert!(!labels.contains(&"/stop"));

        let state = completions_for(input, &ctx(CompletionTarget::Worker)).unwrap();
        let labels: Vec<&str> = state.items.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"/answer"));
        assert!(labels.contains(&"/stop"));
        // and accepting an argument-taking command ends in a space to type into
        let answer = state.items.iter().find(|s| s.label == "/answer").unwrap();
        assert_eq!(answer.value, "/answer ");
        let stop = state.items.iter().find(|s| s.label == "/stop").unwrap();
        assert_eq!(stop.value, "/stop");
    }

    #[test]
    fn session_commands_are_offered_everywhere_with_their_argument_hint() {
        // the session surface is console-wide: offered on both targets
        for target in [CompletionTarget::Orchestrator, CompletionTarget::Worker] {
            let state = completions_for("/", &ctx(target)).unwrap();
            let labels: Vec<&str> = state.items.iter().map(|s| s.label.as_str()).collect();
            assert!(labels.contains(&"/sessions"), "{labels:?}");
            assert!(labels.contains(&"/session"), "{labels:?}");
            assert!(labels.contains(&"/shutdown"));
        }
        // every console command fits, with room for the agent's
        assert!(
            COMMANDS.len() <= MAX_SUGGESTIONS,
            "the cap must hold them all"
        );
        // /session takes an argument: accepting it opens the composer
        let state = completions_for("/session", &ctx(CompletionTarget::Orchestrator)).unwrap();
        assert_eq!(state.items[0].label, "/session");
        assert_eq!(state.items[0].value, "/session ", "an argument follows");
        // aliases still resolve
        assert_eq!(resolve_command("/sd").unwrap().name, "/shutdown");
    }

    #[test]
    fn typing_narrows_by_long_form_and_alias() {
        let mut c = ctx(CompletionTarget::Orchestrator);
        c.agent_commands = vec![AgentCommandOption::from_orchestrator(
            "model",
            Some("Set the model"),
            Some("<model>"),
        )];
        let state = completions_for("/q", &c).unwrap();
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].label, "/quit");
        assert_eq!(state.start, 0);
        assert_eq!(state.token, "/q");
    }

    #[test]
    fn agent_commands_pass_through_after_the_console_owns() {
        let mut c = ctx(CompletionTarget::Orchestrator);
        c.agent_commands = vec![
            AgentCommandOption::from_orchestrator("usage", Some("Show usage"), Some("<scope>")),
            AgentCommandOption::from_worker("skill:review", "Review the diff", "skill"),
        ];
        let state = completions_for("/", &c).unwrap();
        let labels: Vec<&str> = state.items.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"/usage"));
        assert!(labels.contains(&"/skill:review"));
        // console commands come first, the agent's ride after
        let first_agent = labels.iter().position(|l| *l == "/usage").unwrap();
        assert!(first_agent > labels.iter().position(|l| *l == "/help").unwrap());
        // a pi command's source is shown
        let skill = state
            .items
            .iter()
            .find(|s| s.label == "/skill:review")
            .unwrap();
        assert!(skill.detail.contains("[skill]"), "{}", skill.detail);
        assert_eq!(skill.value, "/skill:review", "no argument hint: no space");
        let usage = state.items.iter().find(|s| s.label == "/usage").unwrap();
        assert_eq!(usage.value, "/usage ", "claude's hint makes room for it");
    }

    #[test]
    fn at_completes_workers_then_files() {
        let mut c = ctx(CompletionTarget::Orchestrator);
        c.workers = vec![
            ("add-auth".into(), "running".into()),
            ("add-tests".into(), "blocked".into()),
        ];
        c.files = vec![
            "src/main.rs".into(),
            "src/cli.rs".into(),
            "README.md".into(),
        ];
        let state = completions_for("@ad", &c).unwrap();
        let labels: Vec<&str> = state.items.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["@add-auth", "@add-tests", "@README.md"],
            "workers first; \"readme\" holds \"ad\" as a substring"
        );
        let state = completions_for("@README", &c).unwrap();
        assert_eq!(state.items[0].label, "@README.md");
        // @ completes mid-input (only / must open the line), replacing the token
        let state = completions_for("hi @ad", &c).unwrap();
        assert_eq!(state.start, 3);
        assert_eq!(state.items[0].label, "@add-auth");
    }

    #[test]
    fn ranking_puts_prefix_matches_before_substring_matches() {
        let items = vec!["stop", "post", "step"];
        let ranked = rank(&items, "st", |s| (*s).to_string());
        assert_eq!(ranked, vec!["stop", "step", "post"]);
        // case-insensitive both ways
        let ranked = rank(&items, "ST", |s| (*s).to_string());
        assert_eq!(ranked, vec!["stop", "step", "post"]);
    }

    #[test]
    fn apply_suggestion_replaces_only_the_token() {
        let state = completions_for("/q", &ctx(CompletionTarget::Orchestrator)).unwrap();
        let out = apply_suggestion(
            "/q",
            &state,
            &Suggestion {
                value: "/quit".into(),
                label: String::new(),
                detail: String::new(),
                kind: SuggestionKind::Command,
            },
        );
        assert_eq!(out, "/quit");
    }

    #[test]
    fn active_token_finds_the_word_the_cursor_ends_on() {
        let (token, start) = active_token("hello @wor");
        assert_eq!((token, start), ("@wor", 6));
        assert_eq!(active_token(""), ("", 0));
        assert_eq!(active_token("done "), ("", 5));
    }

    #[test]
    fn summarize_args_picks_the_primary_argument_and_clips() {
        assert_eq!(
            summarize_args(&serde_json::json!({"command": "git status\nmore"})),
            "git status"
        );
        let long = "x".repeat(200);
        let summary = summarize_args(&serde_json::json!({"path": long}));
        assert_eq!(summary.chars().count(), 80);
        assert!(summary.ends_with('…'));
        assert_eq!(summarize_args(&serde_json::json!({"name": "db"})), "db");
        assert_eq!(summarize_args(&serde_json::json!({"n": 1})), r#"{"n":1}"#);
        assert_eq!(summarize_args(&serde_json::Value::Null), "");
    }
}
