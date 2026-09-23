//! Fuzzy command palette over everything the selected session can do: the
//! console's own commands, the agent's own commands passed through verbatim,
//! the orchestrator's MCP servers and tools (for reference), models for
//! `/model`, and jump-to-session entries. Ranked with `nucleo-matcher`.
//!
//! The palette owns ranking and grouping; the app owns open/close, the query
//! buffer, and executing the chosen [`PaletteAction`].

use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::tui::completions::{AgentCommandOption, COMMANDS, CommandSpec, command_forms};

/// Which section an entry came from, so the renderer can group and label it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteGroup {
    /// The console's own commands (`/stop`, `/thinking`, …).
    Console,
    /// Whatever the agent on the other end offers, verbatim. Workers carry a
    /// `source` (`skill`, `prompt`, `extension`); the orchestrator's commands
    /// do not.
    Agent { source: Option<String> },
    /// MCP servers and their tools, shown for reference with their status.
    Servers,
    /// Models the selected session can switch to.
    Models,
    /// Jump to another session.
    Sessions,
}

impl PaletteGroup {
    /// The renderer's section label for this group.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Console => "console".to_string(),
            Self::Agent { source: None } => "agent".to_string(),
            Self::Agent { source: Some(s) } => format!("agent · {s}"),
            Self::Servers => "mcp".to_string(),
            Self::Models => "models".to_string(),
            Self::Sessions => "sessions".to_string(),
        }
    }
}

/// What running a palette entry does. Delivery differences by target (an
/// orchestrator command is a user message; a worker command is a `command`
/// envelope) are the app's business, not the palette's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    /// A console command by long form; the app prefills the composer when the
    /// spec takes an argument, and runs it otherwise.
    ConsoleCommand(String),
    /// The agent's own command, passed through verbatim; `takes_argument`
    /// prefills the composer so the user can type the argument.
    AgentCommand { name: String, takes_argument: bool },
    /// Switch the selected session's model, live.
    Model {
        model_id: String,
        provider: Option<String>,
    },
    /// Jump to the nth dashboard row.
    JumpTo(usize),
    /// Shown for reference only: selecting it does nothing.
    Reference,
}

/// One palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    /// What the user sees and matches against.
    pub label: String,
    pub detail: String,
    pub group: PaletteGroup,
    pub action: PaletteAction,
}

/// An MCP server and the tools the orchestrator actually has from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInfo {
    pub name: String,
    pub status: String,
    /// Tool names as claude reported them (`mcp__<server>__<tool>`).
    pub tools: Vec<String>,
}

/// Everything the palette offers, for the session currently selected.
#[derive(Debug, Clone, Default)]
pub struct PaletteContext {
    /// Worker-only console commands are offered only for a worker.
    pub target_is_worker: bool,
    /// The orchestrator's slash commands and skills (initialize handshake).
    pub orchestrator_commands: Vec<AgentCommandOption>,
    /// The worker's commands, skills and prompt templates (`get_commands`).
    pub worker_commands: Vec<AgentCommandOption>,
    /// The orchestrator's MCP servers, tools and connection status.
    pub mcp_servers: Vec<McpServerInfo>,
    /// For a worker, the real list from `get_available_models`; for the
    /// orchestrator, empty (the known aliases are offered instead).
    pub worker_models: Vec<crate::fleet::run::WorkerModel>,
    /// Session names in dashboard order (orchestrator first); the index is
    /// the row to jump to.
    pub sessions: Vec<String>,
}

/// A narrowed palette: `m` opens it over models only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteScope {
    #[default]
    All,
    Models,
}

/// The model aliases the orchestrator understands; anything else typed into
/// `/model` is claude's problem to validate, and its error text is shown.
pub const ORCHESTRATOR_MODEL_ALIASES: &[&str] = &["opus", "sonnet", "haiku", "fable", "opusplan"];

/// The palette's entries, grouped in display order: console, agent, servers,
/// models, sessions. With [`PaletteScope::Models`], only the model entries.
#[must_use]
pub fn build_items(ctx: &PaletteContext, scope: PaletteScope) -> Vec<PaletteItem> {
    let mut items = Vec::new();
    if scope == PaletteScope::All {
        // The console's own commands: worker-only ones need a worker.
        for spec in COMMANDS {
            if spec.worker_only && !ctx.target_is_worker {
                continue;
            }
            items.push(PaletteItem {
                label: spec.name.to_string(),
                detail: format!("{}  ({})", spec.detail, forms_and_hint(spec)),
                group: PaletteGroup::Console,
                action: PaletteAction::ConsoleCommand(spec.name.to_string()),
            });
        }
        // Whatever the agent itself offers — passed through verbatim, never
        // filtered and never hard-coded.
        let agent = if ctx.target_is_worker {
            &ctx.worker_commands
        } else {
            &ctx.orchestrator_commands
        };
        for command in agent {
            // a console command of the same name is the one that runs — the
            // console intercepts it before anything reaches the agent — so the
            // agent's entry would be a second row that does the first's job
            let label = format!("/{}", command.name);
            if crate::tui::completions::resolve_command(&label).is_some() {
                continue;
            }
            items.push(PaletteItem {
                label,
                detail: [
                    command.description.clone(),
                    command
                        .source
                        .as_ref()
                        .map_or(String::new(), |s| format!("[{s}]")),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("  "),
                group: PaletteGroup::Agent {
                    source: command.source.clone(),
                },
                action: PaletteAction::AgentCommand {
                    name: command.name.clone(),
                    takes_argument: command.argument_hint.is_some(),
                },
            });
        }
        // MCP servers and their tools: what the orchestrator actually has.
        for server in &ctx.mcp_servers {
            items.push(PaletteItem {
                label: server.name.clone(),
                detail: if server.tools.is_empty() {
                    server.status.clone()
                } else {
                    format!(
                        "{} · {} tool{}",
                        server.status,
                        server.tools.len(),
                        if server.tools.len() == 1 { "" } else { "s" }
                    )
                },
                group: PaletteGroup::Servers,
                action: PaletteAction::Reference,
            });
            for tool in &server.tools {
                items.push(PaletteItem {
                    label: tool.clone(),
                    detail: format!("{} · {}", server.name, server.status),
                    group: PaletteGroup::Servers,
                    action: PaletteAction::Reference,
                });
            }
        }
    }
    // Models for `/model`: a worker's real list, or the orchestrator's aliases.
    if ctx.target_is_worker {
        for model in &ctx.worker_models {
            items.push(PaletteItem {
                label: model.name.clone().unwrap_or_else(|| model.id.clone()),
                detail: format!("{} · {}", model.id, model.provider),
                group: PaletteGroup::Models,
                action: PaletteAction::Model {
                    model_id: model.id.clone(),
                    provider: Some(model.provider.clone()),
                },
            });
        }
    } else {
        for alias in ORCHESTRATOR_MODEL_ALIASES {
            items.push(PaletteItem {
                label: (*alias).to_string(),
                detail: "alias · any model id is accepted".to_string(),
                group: PaletteGroup::Models,
                action: PaletteAction::Model {
                    model_id: (*alias).to_string(),
                    provider: None,
                },
            });
        }
    }
    if scope == PaletteScope::All {
        for (index, name) in ctx.sessions.iter().enumerate() {
            items.push(PaletteItem {
                label: name.clone(),
                detail: "jump to session".to_string(),
                group: PaletteGroup::Sessions,
                action: PaletteAction::JumpTo(index),
            });
        }
    }
    items
}

fn forms_and_hint(spec: &CommandSpec) -> String {
    let forms = command_forms(spec);
    if spec.takes_argument {
        format!("{forms} <arg>")
    } else {
        forms
    }
}

/// One ranked match: the item's index in the built list, and its score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ranked {
    pub index: usize,
    pub score: u16,
}

/// Rank `items` against `query`, best first. An empty query keeps the build
/// order (grouped). Non-matching items are dropped. Ties keep build order.
#[must_use]
pub fn ranked(query: &str, items: &[PaletteItem]) -> Vec<Ranked> {
    if query.trim().is_empty() {
        return items
            .iter()
            .enumerate()
            .map(|(index, _)| Ranked { index, score: 0 })
            .collect();
    }
    let mut config = Config::DEFAULT;
    // Typing a command prefix should surface that command first.
    config.prefer_prefix = true;
    let mut matcher = Matcher::new(config);
    let mut hay_buf = Vec::new();
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(query.trim(), &mut needle_buf);
    let mut out: Vec<Ranked> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            matcher
                .fuzzy_match(Utf32Str::new(&item.label, &mut hay_buf), needle)
                .map(|score| Ranked { index, score })
        })
        .collect();
    // Highest score first; ties keep the grouped build order.
    out.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::run::WorkerModel;

    fn orchestrator_ctx() -> PaletteContext {
        PaletteContext {
            target_is_worker: false,
            orchestrator_commands: vec![
                AgentCommandOption::from_orchestrator(
                    "model",
                    Some("Set the model"),
                    Some("<model>"),
                ),
                AgentCommandOption::from_orchestrator("usage", Some("Show usage"), None),
            ],
            mcp_servers: vec![McpServerInfo {
                name: "fleet".into(),
                status: "connected".into(),
                tools: vec![
                    "mcp__fleet__fleet_spawn".into(),
                    "mcp__fleet__fleet_stop".into(),
                ],
            }],
            sessions: vec!["orchestrator".into(), "add-auth".into()],
            ..PaletteContext::default()
        }
    }

    fn worker_ctx() -> PaletteContext {
        PaletteContext {
            target_is_worker: true,
            worker_commands: vec![
                AgentCommandOption::from_worker("skill:review", "Review the diff", "skill"),
                AgentCommandOption::from_worker("list-todos", "List todos", "prompt"),
            ],
            worker_models: vec![
                WorkerModel {
                    provider: "anthropic".into(),
                    id: "claude-opus-5".into(),
                    name: Some("Opus".into()),
                    thinking_levels: Vec::new(),
                    context_window: None,
                    cost: None,
                },
                WorkerModel {
                    provider: "openai".into(),
                    id: "gpt-5.6".into(),
                    name: None,
                    thinking_levels: Vec::new(),
                    context_window: None,
                    cost: None,
                },
            ],
            sessions: vec!["orchestrator".into(), "db".into()],
            ..PaletteContext::default()
        }
    }

    fn groups_of(items: &[PaletteItem]) -> Vec<PaletteGroup> {
        items.iter().map(|i| i.group.clone()).collect()
    }

    #[test]
    fn an_agent_command_a_console_command_shadows_is_listed_once() {
        // claude offers /model, and so does the console, whose version runs
        let items = build_items(&orchestrator_ctx(), PaletteScope::All);
        let models: Vec<&PaletteItem> = items.iter().filter(|i| i.label == "/model").collect();
        assert_eq!(models.len(), 1, "{models:?}");
        assert!(matches!(models[0].action, PaletteAction::ConsoleCommand(_)));
        assert!(
            items.iter().any(|i| i.label == "/usage"),
            "an agent command nothing shadows still rides along"
        );
    }

    #[test]
    fn console_group_hides_worker_only_commands_on_the_orchestrator() {
        let items = build_items(&orchestrator_ctx(), PaletteScope::All);
        let console: Vec<&str> = items
            .iter()
            .filter(|i| i.group == PaletteGroup::Console)
            .map(|i| i.label.as_str())
            .collect();
        assert!(!console.contains(&"/stop"), "{console:?}");
        assert!(console.contains(&"/help"));
        assert!(
            console.contains(&"/model"),
            "/model is a console command now"
        );

        let items = build_items(&worker_ctx(), PaletteScope::All);
        let console: Vec<&str> = items
            .iter()
            .filter(|i| i.group == PaletteGroup::Console)
            .map(|i| i.label.as_str())
            .collect();
        assert!(console.contains(&"/answer"));
        assert!(console.contains(&"/stop"));
    }

    #[test]
    fn groups_come_in_display_order() {
        let items = build_items(&orchestrator_ctx(), PaletteScope::All);
        let groups = groups_of(&items);
        let first = |g: &PaletteGroup| groups.iter().position(|x| x == g).unwrap();
        assert!(first(&PaletteGroup::Console) < first(&PaletteGroup::Agent { source: None }));
        assert!(first(&PaletteGroup::Agent { source: None }) < first(&PaletteGroup::Servers));
        assert!(first(&PaletteGroup::Servers) < first(&PaletteGroup::Models));
        assert!(first(&PaletteGroup::Models) < first(&PaletteGroup::Sessions));
    }

    #[test]
    fn agent_commands_pass_through_verbatim_for_both_targets() {
        let items = build_items(&orchestrator_ctx(), PaletteScope::All);
        let usage = items.iter().find(|i| i.label == "/usage").unwrap();
        assert_eq!(
            usage.group,
            PaletteGroup::Agent { source: None },
            "claude's commands have no source"
        );
        assert_eq!(
            usage.action,
            PaletteAction::AgentCommand {
                name: "usage".into(),
                takes_argument: false
            }
        );

        let items = build_items(&worker_ctx(), PaletteScope::All);
        let skill = items.iter().find(|i| i.label == "/skill:review").unwrap();
        assert_eq!(
            skill.group,
            PaletteGroup::Agent {
                source: Some("skill".into())
            }
        );
        assert!(skill.detail.contains("[skill]"));
        // pi command names are whatever the worker reports, odd or not
        assert!(items.iter().any(|i| i.label == "/list-todos"));
    }

    #[test]
    fn models_come_from_the_worker_list_or_the_orchestrator_aliases() {
        let items = build_items(&worker_ctx(), PaletteScope::Models);
        assert_eq!(items.len(), 2, "{items:?}");
        let opus = items.iter().find(|i| i.label == "Opus").unwrap();
        assert_eq!(
            opus.action,
            PaletteAction::Model {
                model_id: "claude-opus-5".into(),
                provider: Some("anthropic".into())
            }
        );
        // unnamed models fall back to their id
        assert!(items.iter().any(|i| i.label == "gpt-5.6"));

        let items = build_items(&orchestrator_ctx(), PaletteScope::Models);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["opus", "sonnet", "haiku", "fable", "opusplan"],
            "the known aliases, nothing else invented"
        );
        assert_eq!(
            items[0].action,
            PaletteAction::Model {
                model_id: "opus".into(),
                provider: None
            }
        );
    }

    #[test]
    fn servers_show_status_and_their_tools_for_reference() {
        let items = build_items(&orchestrator_ctx(), PaletteScope::All);
        let server = items.iter().find(|i| i.label == "fleet").unwrap();
        assert_eq!(server.group, PaletteGroup::Servers);
        assert_eq!(server.action, PaletteAction::Reference);
        assert!(server.detail.contains("connected"));
        assert!(server.detail.contains("2 tools"));
        let tool = items
            .iter()
            .find(|i| i.label == "mcp__fleet__fleet_spawn")
            .unwrap();
        assert_eq!(tool.action, PaletteAction::Reference);
        assert!(tool.detail.contains("fleet"));
    }

    #[test]
    fn sessions_offer_a_jump_to_their_dashboard_row() {
        let items = build_items(&worker_ctx(), PaletteScope::All);
        let jump = items.iter().find(|i| i.label == "db").unwrap();
        assert_eq!(jump.group, PaletteGroup::Sessions);
        assert_eq!(
            jump.action,
            PaletteAction::JumpTo(1),
            "row index, orchestrator at 0"
        );
    }

    #[test]
    fn empty_query_keeps_the_grouped_build_order() {
        let items = build_items(&orchestrator_ctx(), PaletteScope::All);
        let ranked = ranked("", &items);
        assert_eq!(ranked.len(), items.len());
        assert!(ranked.windows(2).all(|w| w[0].index < w[1].index));
    }

    #[test]
    fn fuzzy_ranking_prefers_prefix_matches_and_drops_the_rest() {
        let items = vec![
            PaletteItem {
                label: "/followup".into(),
                detail: String::new(),
                group: PaletteGroup::Console,
                action: PaletteAction::ConsoleCommand("/followup".into()),
            },
            PaletteItem {
                label: "/stop".into(),
                detail: String::new(),
                group: PaletteGroup::Console,
                action: PaletteAction::ConsoleCommand("/stop".into()),
            },
            PaletteItem {
                label: "mcp__x__stop_tool".into(),
                detail: String::new(),
                group: PaletteGroup::Servers,
                action: PaletteAction::Reference,
            },
        ];
        let matches = ranked("stop", &items);
        assert_eq!(matches.len(), 2, "followup does not match");
        // "/stop" starts with the query and outranks a mid-string match
        assert_eq!(items[matches[0].index].label, "/stop");
        assert!(matches[0].score > matches[1].score);

        // a scattered query still finds its target: mdl → /model
        let items = build_items(&orchestrator_ctx(), PaletteScope::All);
        let matches = ranked("mdl", &items);
        assert_eq!(items[matches[0].index].label, "/model");
    }

    #[test]
    fn scope_models_hides_everything_but_models() {
        let items = build_items(&orchestrator_ctx(), PaletteScope::Models);
        assert!(items.iter().all(|i| i.group == PaletteGroup::Models));
        // and `m` on a worker shows pi's models, not console commands
        let items = build_items(&worker_ctx(), PaletteScope::Models);
        assert!(items.iter().all(|i| i.group == PaletteGroup::Models));
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn group_labels_name_their_source() {
        assert_eq!(PaletteGroup::Console.label(), "console");
        assert_eq!(PaletteGroup::Agent { source: None }.label(), "agent");
        assert_eq!(
            PaletteGroup::Agent {
                source: Some("skill".into())
            }
            .label(),
            "agent · skill"
        );
        assert_eq!(PaletteGroup::Servers.label(), "mcp");
        assert_eq!(PaletteGroup::Models.label(), "models");
        assert_eq!(PaletteGroup::Sessions.label(), "sessions");
    }
}
