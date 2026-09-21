//! Building a session's transcript from its `events.jsonl`: prompts,
//! reasoning, replies, tool calls and fleet events as renderable blocks, with
//! the streamed text that has not landed yet kept apart. One fold per side:
//! the orchestrator's records (`orch::records::EventRecord`, including
//! claude's own messages verbatim) and a worker's monitor events.
//!
//! Ported from the TypeScript `src/tui/model.ts` (`reduceOrchestrator`, the
//! orchestrator view state) and `src/console/transcript.ts` (the worker
//! side). Markdown spans are the renderer's job: text blocks carry the raw
//! markdown and `tui::markdown` styles it.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::fleet::event::FleetEvent;
use crate::orch::protocol::{
    is_assistant, is_replayed_user_message, is_result, is_stream_event, is_system_init, is_user,
    text_delta_of, text_of_assistant, thinking_of_assistant, tool_results_of, tool_uses_of,
    user_text,
};
use crate::orch::records::{Activity, EventRecord, OrchestratorEvent};
use crate::util::{first_line, parse_ts_ms};

/// What kind of line a block is, so the renderer can colour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// A prompt we (or someone) sent: `> text`.
    User,
    /// A fleet event, a steer, a question: `⚑ …`, `▶ …`, `? …`.
    Fleet,
    /// Assistant prose — raw markdown for the renderer to style.
    Text,
    /// Reasoning, dimmed and abridged.
    Thinking,
    /// A tool call: `⚙ name args…`.
    Tool,
    /// A tool result preview: `↳ name: body`.
    ToolResult,
    /// A passing note: progress, settings changes, retries.
    System,
    /// A failure.
    Error,
}

/// The fields of the `<task-notification>` block claude injects as a plain
/// user message when a background task it started finishes. Claude writes
/// it, not the human, so it must never render as a prompt — `None` for
/// anything that is not one of these blocks.
///
/// `tool-use-id` is dropped: it correlates the notification with a tool call
/// the console cannot act on, and it costs a row to say nothing.
fn task_notification_fields(text: &str) -> Option<Vec<(String, String)>> {
    let body = text
        .trim()
        .strip_prefix("<task-notification>")?
        .strip_suffix("</task-notification>")?;
    let mut fields = Vec::new();
    for line in body.lines() {
        let Some(rest) = line.trim().strip_prefix('<') else {
            continue;
        };
        let Some((tag, rest)) = rest.split_once('>') else {
            continue;
        };
        if tag.is_empty() || tag.starts_with('/') || tag == "tool-use-id" {
            continue;
        }
        let Some(value) = rest.strip_suffix(&format!("</{tag}>")) else {
            continue;
        };
        fields.push((tag.to_string(), value.trim().to_string()));
    }
    (!fields.is_empty()).then_some(fields)
}

/// One transcript block: kind plus the text to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
}

/// How much of a tool call and of its output the transcript shows. A command
/// is written by the model and is the thing worth reading, so it gets room;
/// output can be megabytes, so it gets a preview.
const TOOL_ARGS_LINES: usize = 10;
const TOOL_ARGS_CHARS: usize = 1200;
const TOOL_RESULT_LINES: usize = 4;
const TOOL_RESULT_CHARS: usize = 600;

/// Reasoning is long and secondary: show the head of it, dimmed.
const THINKING_LINES: usize = 8;

/// Blocks kept per session; older ones drop off the top.
const MAX_BLOCKS: usize = 500;

/// A session's transcript: committed blocks, the text still streaming, and —
/// for the orchestrator — the turn facts the status line shows.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    blocks: Vec<Block>,
    /// Worker streaming: in-flight assistant text by content index.
    open: BTreeMap<usize, String>,
    /// Orchestrator streaming: coalesced `stream_text` not yet committed.
    partial: String,
    /// Orchestrator: `tool_use_id → tool name`, so results can name their tool.
    tool_names: HashMap<String, String>,
    /// Orchestrator: texts we rendered ourselves; their replay is suppressed.
    pending_echoes: Vec<String>,
    // Orchestrator turn facts (the worker's live in `RunState` instead).
    activity: Option<Activity>,
    turn_active: bool,
    session_id: Option<String>,
    model: Option<String>,
    cost_usd: f64,
    num_turns: u32,
    exited: bool,
}

impl Transcript {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The committed blocks, oldest first.
    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Text the model is still streaming, not yet a committed block. A fresh
    /// join per call: the session view asks once per frame, the joined text
    /// is just the in-flight deltas, and ownership beats a leak per frame.
    #[must_use]
    pub fn partial(&self) -> Option<String> {
        if !self.open.is_empty() {
            return Some(self.open.values().cloned().collect());
        }
        (!self.partial.is_empty()).then(|| self.partial.clone())
    }

    /// What the orchestrator is doing right now, or none between turns.
    #[must_use]
    pub const fn activity(&self) -> Option<&Activity> {
        self.activity.as_ref()
    }

    /// Is a turn in flight?
    #[must_use]
    pub const fn turn_active(&self) -> bool {
        self.turn_active
    }

    /// The orchestrator's claude session id, once the handshake lands.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The model claude reported.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Running spend, from the last result message.
    #[must_use]
    pub const fn cost_usd(&self) -> f64 {
        self.cost_usd
    }

    /// Completed turns, from the last result message.
    #[must_use]
    pub const fn num_turns(&self) -> u32 {
        self.num_turns
    }

    /// The claude child is gone.
    #[must_use]
    pub const fn exited(&self) -> bool {
        self.exited
    }

    // -----------------------------------------------------------------------
    // Local events — things that happen in the app rather than on the wire

    /// A prompt we sent: rendered immediately, its replay suppressed.
    pub fn push_sent(&mut self, text: &str) {
        self.pending_echoes.push(text.to_string());
        self.activity = Some(crate::orch::records::Activity {
            kind: crate::orch::records::ActivityKind::Thinking,
            label: None,
            since: crate::util::now_ms(),
        });
        self.turn_active = true;
        self.gap();
        self.push(BlockKind::User, &format!("> {text}"));
    }

    /// A fleet batch, shown as one rail-friendly line; the full batch text is
    /// what goes to the orchestrator, so its replay is suppressed.
    pub fn push_fleet(&mut self, events: &[FleetEvent], batch_text: &str) {
        self.pending_echoes.push(batch_text.to_string());
        self.gap();
        let summary = events
            .iter()
            .map(|e| format!("{} {}", e.kind, e.name))
            .collect::<Vec<_>>()
            .join(" · ");
        self.push(BlockKind::Fleet, &format!("⚑ {summary}"));
    }

    /// A passing note, kept in the transcript to look back at.
    pub fn push_notice(&mut self, text: &str) {
        self.push(BlockKind::System, text);
    }

    /// A failure, kept in the transcript.
    pub fn push_error(&mut self, text: &str) {
        self.push(BlockKind::Error, text);
    }

    /// The orchestrator's claude child is gone.
    pub fn orchestrator_exit(&mut self, code: Option<i32>, signal: Option<&str>) {
        self.exited = true;
        self.turn_active = false;
        self.activity = None;
        let why = match (code, signal) {
            (Some(code), Some(signal)) => format!("code {code}, {signal}"),
            (Some(code), None) => format!("code {code}"),
            (None, Some(signal)) => signal.to_string(),
            (None, None) => "?".to_string(),
        };
        self.push(BlockKind::Error, &format!("orchestrator exited ({why})"));
    }

    // -----------------------------------------------------------------------
    // Orchestrator side: `orchestrator/events.jsonl`

    /// Fold one orchestrator record into the transcript.
    pub fn apply_orchestrator_record(&mut self, record: &EventRecord) {
        match record.decode() {
            OrchestratorEvent::StreamText { text } => {
                self.partial.push_str(&text);
                self.turn_active = true;
                if self
                    .activity
                    .as_ref()
                    .is_none_or(|a| a.kind != crate::orch::records::ActivityKind::Responding)
                {
                    self.activity = Some(Activity {
                        kind: crate::orch::records::ActivityKind::Responding,
                        label: None,
                        since: crate::util::now_ms(),
                    });
                }
            }
            OrchestratorEvent::Activity { activity } => {
                self.activity = activity;
                if self.activity.is_some() {
                    self.turn_active = true;
                }
            }
            // Permission records are state, not transcript: the overlay draws them.
            OrchestratorEvent::PermissionRequest { .. }
            | OrchestratorEvent::PermissionResolved { .. } => {}
            OrchestratorEvent::Notice { text, error } => {
                if error.unwrap_or(false) {
                    self.push(BlockKind::Error, &text);
                } else {
                    self.push(BlockKind::System, &text);
                }
            }
            OrchestratorEvent::Exit { code, signal } => {
                self.orchestrator_exit(code, signal.as_deref());
            }
            OrchestratorEvent::Passthrough(message) => {
                self.apply_claude_message(&message);
            }
        }
    }

    /// Fold one claude message (or an unknown future type) into the transcript.
    fn apply_claude_message(&mut self, msg: &Value) {
        if is_system_init(msg) {
            let fresh = self.session_id.is_none();
            self.session_id = msg
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| self.session_id.clone());
            if let Some(model) = msg.get("model").and_then(Value::as_str) {
                self.model = Some(model.to_string());
            }
            // claude's init message carries servers and, separately, tools
            let servers: Vec<(String, String)> = msg
                .get("mcp_servers")
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |list| {
                    list.iter()
                        .filter_map(|s| {
                            Some((
                                s.get("name")?.as_str()?.to_string(),
                                s.get("status")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            ))
                        })
                        .collect()
                });
            if fresh {
                let servers = servers
                    .iter()
                    .map(|(name, status)| format!("{name}:{status}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.push(
                    BlockKind::System,
                    &format!(
                        "· session {} · {} · mcp {}",
                        self.session_id
                            .as_deref()
                            .unwrap_or("")
                            .get(..8)
                            .unwrap_or(""),
                        self.model.as_deref().unwrap_or("default model"),
                        if servers.is_empty() { "none" } else { &servers },
                    ),
                );
            }
            return;
        }

        if is_stream_event(msg) {
            if let Some(delta) = text_delta_of(msg) {
                self.partial.push_str(&delta);
                self.turn_active = true;
                if self
                    .activity
                    .as_ref()
                    .is_none_or(|a| a.kind != crate::orch::records::ActivityKind::Responding)
                {
                    self.activity = Some(Activity {
                        kind: crate::orch::records::ActivityKind::Responding,
                        label: None,
                        since: crate::util::now_ms(),
                    });
                }
                return;
            }
            // thinking deltas: mark the activity, nothing to draw yet
            if crate::orch::protocol::is_thinking_event(msg) {
                self.turn_active = true;
                if self
                    .activity
                    .as_ref()
                    .is_none_or(|a| a.kind != crate::orch::records::ActivityKind::Thinking)
                {
                    self.activity = Some(Activity {
                        kind: crate::orch::records::ActivityKind::Thinking,
                        label: None,
                        since: crate::util::now_ms(),
                    });
                }
            }
            return;
        }

        if is_assistant(msg) {
            self.turn_active = true;
            self.partial.clear();
            self.push_thinking(&thinking_of_assistant(msg));
            let text = text_of_assistant(msg).trim().to_string();
            if !text.is_empty() {
                self.gap();
                // markdown styling is the renderer's job; the block is raw
                for line in text.split('\n') {
                    self.blocks.push(Block {
                        kind: BlockKind::Text,
                        text: line.to_string(),
                    });
                }
                self.trim();
            }
            let tools = tool_uses_of(msg);
            for tool in &tools {
                self.tool_names.insert(tool.id.clone(), tool.name.clone());
                // the command itself is the thing worth reading, so it is not cut
                self.push_block(
                    BlockKind::Tool,
                    &format!("⚙ {} ", tool.name),
                    &tool_args_text(&tool.input),
                    TOOL_ARGS_LINES,
                    TOOL_ARGS_CHARS,
                );
            }
            if let Some(last) = tools.last() {
                self.activity = Some(Activity {
                    kind: crate::orch::records::ActivityKind::Tool,
                    label: Some(last.name.clone()),
                    since: crate::util::now_ms(),
                });
            }
            return;
        }

        if is_user(msg) {
            if is_replayed_user_message(msg) {
                let text = user_text(msg).unwrap_or_default();
                // our own message coming back; we already rendered it
                if let Some(at) = self.pending_echoes.iter().position(|e| *e == text) {
                    self.pending_echoes.remove(at);
                    return;
                }
                if let Some(fields) = task_notification_fields(&text) {
                    self.push(BlockKind::System, "· task-notification");
                    for (tag, value) in fields {
                        self.push_block(
                            BlockKind::System,
                            &format!("  {tag}: "),
                            &value,
                            TOOL_RESULT_LINES,
                            TOOL_RESULT_CHARS,
                        );
                    }
                    return;
                }
                self.push(BlockKind::User, &format!("> {text}"));
                return;
            }
            for result in tool_results_of(msg) {
                let name = self
                    .tool_names
                    .get(&result.tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                // tool output can be one enormous line; show a bounded preview
                let body = result.text.trim().to_string();
                let body = if body.is_empty() {
                    if result.is_error {
                        "(error)".to_string()
                    } else {
                        "(no output)".to_string()
                    }
                } else {
                    body
                };
                self.push_block(
                    BlockKind::ToolResult,
                    &format!("  ↳ {name}: "),
                    &body,
                    TOOL_RESULT_LINES,
                    TOOL_RESULT_CHARS,
                );
            }
            return;
        }

        if is_result(msg) {
            self.turn_active = false;
            self.partial.clear();
            self.activity = None;
            if let Some(cost) = msg.get("total_cost_usd").and_then(Value::as_f64) {
                self.cost_usd = cost;
            }
            if let Some(turns) = msg.get("num_turns").and_then(Value::as_u64) {
                self.num_turns = u32::try_from(turns).unwrap_or(u32::MAX);
            }
            if msg
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let subtype = msg.get("subtype").and_then(Value::as_str).unwrap_or("?");
                let errors = msg
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                self.push(
                    BlockKind::Error,
                    &format!(
                        "! turn failed ({subtype}){}",
                        (!errors.is_empty())
                            .then_some(format!(": {errors}"))
                            .unwrap_or_default()
                    ),
                );
            }
            return;
        }

        if msg.get("type").and_then(Value::as_str) == Some("system")
            && msg.get("subtype").and_then(Value::as_str) == Some("api_retry")
        {
            let attempt = msg.get("attempt").and_then(Value::as_u64).unwrap_or(0);
            let max = msg.get("max_retries").and_then(Value::as_u64).unwrap_or(0);
            let error = msg
                .get("error")
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            self.push(
                BlockKind::System,
                &format!("↻ api retry {attempt}/{max}{error}"),
            );
        }
    }

    // -----------------------------------------------------------------------
    // Worker side: `runs/<id>/events.jsonl`

    /// Fold one worker monitor event into the transcript.
    pub fn apply_worker_event(&mut self, event: &Value) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "task_prompt" => {
                let brief = first_line(event.get("brief").and_then(Value::as_str).unwrap_or(""));
                self.push(BlockKind::Fleet, &format!("▶ task: {}", clip(brief, 200)));
            }
            "steering_delivered" => {
                let source = event
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = event.get("message").and_then(Value::as_str).unwrap_or("");
                self.push(BlockKind::Fleet, &format!("▶ {source}: {message}"));
            }
            "abort_requested" => self.push(BlockKind::System, "■ abort requested"),
            "worker_question" | "worker_dialog" => {
                let question = event.get("question").and_then(Value::as_str).unwrap_or("");
                let options = event.get("options").and_then(Value::as_array);
                let options = match options {
                    Some(list) if !list.is_empty() => format!(
                        " [{}]",
                        list.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    ),
                    _ => String::new(),
                };
                self.push(
                    BlockKind::Fleet,
                    &format!("? {}{options}", clip(question, 300)),
                );
            }
            "worker_progress" => {
                let message = event.get("message").and_then(Value::as_str).unwrap_or("");
                self.push(BlockKind::System, &format!("· {}", clip(message, 200)));
            }
            "thinking_requested" => {
                let level = event.get("level").and_then(Value::as_str).unwrap_or("?");
                let source = event
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                self.push(
                    BlockKind::System,
                    &format!("· thinking level → {level} ({source})"),
                );
            }
            "thinking_unavailable" => {
                let level = event.get("level").and_then(Value::as_str).unwrap_or("?");
                let now = event
                    .get("level_now")
                    .and_then(Value::as_str)
                    .unwrap_or("unchanged");
                let model = event
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("this model");
                let available: Vec<&str> = event
                    .get("available")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let has = if available.is_empty() {
                    String::new()
                } else {
                    format!(" — it has {}", available.join(", "))
                };
                self.push(
                    BlockKind::Error,
                    &format!("! {model} has no {level} thinking, still {now}{has}"),
                );
            }
            "thinking_rejected" => {
                let error = event.get("error").and_then(Value::as_str).unwrap_or("");
                self.push(
                    BlockKind::System,
                    &format!("! thinking level rejected: {error}"),
                );
            }
            "model_requested" => {
                let model = event.get("model").and_then(Value::as_str).unwrap_or("?");
                let source = event
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                self.push(BlockKind::System, &format!("· model → {model} ({source})"));
            }
            "model_rejected" => {
                let error = event.get("error").and_then(Value::as_str).unwrap_or("");
                self.push(BlockKind::System, &format!("! model rejected: {error}"));
            }
            "model_unresolved" => {
                let model = event.get("model").and_then(Value::as_str).unwrap_or("?");
                self.push(
                    BlockKind::System,
                    &format!("! no configured model matches {model}"),
                );
            }
            "command_delivered" => {
                let source = event
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = event.get("message").and_then(Value::as_str).unwrap_or("");
                self.push(
                    BlockKind::Fleet,
                    &format!("▶ command ({source}): {message}"),
                );
            }
            "answer_delivered" => {
                let source = event
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = event.get("message").and_then(Value::as_str).unwrap_or("");
                self.push(BlockKind::Fleet, &format!("▶ answer ({source}): {message}"));
            }
            "worker_question_resolved" => {
                let how = event.get("how").and_then(Value::as_str).unwrap_or("");
                match how {
                    "timeout" => self.push(
                        BlockKind::System,
                        "! no answer in time; worker proceeds on its own judgment",
                    ),
                    "aborted" => self.push(BlockKind::System, "! question aborted"),
                    _ => {}
                }
            }
            "dialog_cancelled" => self.push(BlockKind::System, "· dialog cancelled"),
            "control_dropped" => {
                let control = event
                    .get("control")
                    .and_then(Value::as_str)
                    .unwrap_or("control");
                let source = event
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let reason = event.get("reason").and_then(Value::as_str).unwrap_or("");
                self.push(
                    BlockKind::System,
                    &format!("! {control} from {source} dropped: {reason}"),
                );
            }
            "message_update" => {
                // the monitor stores the delta under `ev`; raw RPC uses `assistantMessageEvent`
                let a = event
                    .get("ev")
                    .or_else(|| event.get("assistantMessageEvent"));
                let Some(a) = a else { return };
                let index: usize = a
                    .get("contentIndex")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(0);
                match a.get("type").and_then(Value::as_str) {
                    Some("text_start") => {
                        self.open.insert(index, String::new());
                    }
                    Some("text_delta") => {
                        let delta = a.get("delta").and_then(Value::as_str).unwrap_or("");
                        self.open.entry(index).or_default().push_str(delta);
                    }
                    Some("text_end") => {
                        // the content is authoritative when present; else what we buffered
                        let full = match a.get("content").and_then(Value::as_str) {
                            Some(content) => content.to_string(),
                            None => self.open.remove(&index).unwrap_or_default(),
                        };
                        self.open.remove(&index);
                        for line in full.split('\n').filter(|l| !l.trim().is_empty()) {
                            self.push(BlockKind::Text, line);
                        }
                    }
                    _ => {}
                }
            }
            "tool_execution_start" => {
                let name = event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let args = event.get("args").cloned().unwrap_or(Value::Null);
                self.push_block(
                    BlockKind::Tool,
                    &format!("⚙ {name} "),
                    &tool_args_text(&args),
                    TOOL_ARGS_LINES,
                    TOOL_ARGS_CHARS,
                );
            }
            "tool_execution_end" => {
                let body = result_text_of(event);
                let body = body.trim();
                let body = if body.is_empty() {
                    if event
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        "(error)"
                    } else {
                        "(no output)"
                    }
                } else {
                    body
                };
                self.push_block(
                    BlockKind::ToolResult,
                    "  ↳ ",
                    body,
                    TOOL_RESULT_LINES,
                    TOOL_RESULT_CHARS,
                );
            }
            "agent_settled" => self.push(BlockKind::System, "● settled"),
            "run_failed" => {
                let error = event
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the worker stopped");
                for (i, line) in error.split('\n').filter(|l| !l.is_empty()).enumerate() {
                    self.push(
                        BlockKind::Error,
                        &if i == 0 {
                            format!("✖ {line}")
                        } else {
                            format!("  {line}")
                        },
                    );
                }
            }
            "extension_error" => {
                let error = event.get("error").and_then(Value::as_str).unwrap_or("");
                self.push(
                    BlockKind::System,
                    &format!("! extension error: {}", clip(error, 120)),
                );
            }
            "auto_retry_start" => {
                let attempt = event.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                let max = event
                    .get("maxAttempts")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.push(BlockKind::System, &format!("↻ retry {attempt}/{max}"));
            }
            "compaction_start" => self.push(BlockKind::System, "⌁ compacting context"),
            _ => {}
        }
    }

    /// Parse-and-fold whole JSONL lines (bad lines skipped), as a poll or a
    /// replay does.
    pub fn apply_worker_lines(&mut self, lines: &[String]) {
        for line in lines {
            if let Ok(event) = serde_json::from_str::<Value>(line) {
                self.apply_worker_event(&event);
            }
        }
    }

    /// Rebuild a worker transcript from the whole file. Errors leave an empty
    /// transcript: a worker with no events yet is normal.
    pub fn replay_worker(path: &std::path::Path) -> Self {
        let mut transcript = Self::new();
        let (lines, _) = crate::util::read_new_lines(path, 0);
        transcript.apply_worker_lines(&lines);
        if transcript.blocks.len() > MAX_BLOCKS {
            let excess = transcript.blocks.len() - MAX_BLOCKS;
            transcript.blocks.drain(..excess);
        }
        transcript
    }

    // -----------------------------------------------------------------------
    // Shared block plumbing

    /// The one way a block is made, so every block is safe to draw: agent
    /// text reaches here from tool output, model prose and worker events,
    /// and [`crate::util::visible_line`] is what stops a stray control
    /// character tearing the frame.
    fn push(&mut self, kind: BlockKind, text: &str) {
        for line in text.split('\n') {
            self.blocks.push(Block {
                kind,
                text: crate::util::visible_line(line),
            });
        }
        self.trim();
    }

    /// A blank block between groups, so a turn does not read as one wall.
    /// Never two in a row, and never before a first block.
    fn gap(&mut self) {
        if self.blocks.is_empty() || self.blocks.last().is_some_and(|b| b.text.is_empty()) {
            return;
        }
        self.blocks.push(Block {
            kind: BlockKind::System,
            text: String::new(),
        });
    }

    /// Push text over as many lines as it takes, up to a bound. What is left
    /// out is counted rather than hidden behind a bare ellipsis.
    fn push_block(
        &mut self,
        kind: BlockKind,
        prefix: &str,
        text: &str,
        max_lines: usize,
        max_chars: usize,
    ) {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut shown: Vec<String> = Vec::new();
        let mut budget = max_chars;
        let mut cut_mid_line = false;
        for line in &lines {
            if shown.len() >= max_lines || budget == 0 {
                break;
            }
            if line.len() > budget {
                shown.push(line.chars().take(budget).collect());
                cut_mid_line = true;
                break;
            }
            budget = budget.saturating_sub(line.len());
            shown.push((*line).to_string());
        }
        let indent = " ".repeat(prefix.chars().count().min(6));
        if shown.is_empty() {
            self.push(kind, prefix.trim_end());
        } else {
            for (i, line) in shown.iter().enumerate() {
                if i == 0 {
                    self.push(kind, &format!("{prefix}{line}"));
                } else {
                    self.push(kind, &format!("{indent}{line}"));
                }
            }
        }
        let rest_lines = lines.len().saturating_sub(shown.len());
        let shown_chars: usize = shown
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>()
            .saturating_sub(1);
        let rest_chars = text.len().saturating_sub(shown_chars);
        if rest_lines > 0 {
            self.push(
                kind,
                &format!(
                    "{indent}… {rest_lines} more {}",
                    if rest_lines == 1 { "line" } else { "lines" }
                ),
            );
        } else if cut_mid_line && rest_chars > 0 {
            self.push(kind, &format!("{indent}… {rest_chars} more characters"));
        }
    }

    /// Reasoning is long and secondary: show the head of it, dimmed, and say
    /// what was left out.
    fn push_thinking(&mut self, text: &str) {
        let lines: Vec<&str> = text.split('\n').filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return;
        }
        self.gap();
        let shown = lines.len().min(THINKING_LINES);
        for (i, line) in lines.iter().take(shown).enumerate() {
            self.blocks.push(Block {
                kind: BlockKind::Thinking,
                text: format!("{}{line}", if i == 0 { "✻ " } else { "  " }),
            });
        }
        if lines.len() > shown {
            let more = lines.len() - shown;
            self.blocks.push(Block {
                kind: BlockKind::Thinking,
                text: format!(
                    "  … {more} more line{} of thinking",
                    if more == 1 { "" } else { "s" }
                ),
            });
        }
        self.trim();
    }

    fn trim(&mut self) {
        if self.blocks.len() > MAX_BLOCKS {
            let excess = self.blocks.len() - MAX_BLOCKS;
            self.blocks.drain(..excess);
        }
    }
}

/// The arguments as written, not a one-line digest of them.
#[must_use]
pub fn tool_args_text(input: &Value) -> String {
    let Some(map) = input.as_object() else {
        return String::new();
    };
    let primary = map
        .get("command")
        .or_else(|| map.get("path"))
        .or_else(|| map.get("file_path"))
        .or_else(|| map.get("pattern"))
        .or_else(|| map.get("url"))
        .or_else(|| map.get("name"))
        .or_else(|| map.get("target"));
    match primary {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => serde_json::to_string(input).unwrap_or_default(),
    }
}

/// The text of a tool result event, if any (`result.content[].text`).
#[must_use]
pub fn result_text_of(event: &Value) -> String {
    event
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Clip to `max` characters, ellipsis on the cut.
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        text.to_string()
    }
}

/// How old a session's last movement is, for the "working… 8s" line.
#[must_use]
pub fn age_since(ts: Option<&str>, fallback: &str, now_ms: i64) -> i64 {
    let parsed = ts
        .and_then(parse_ts_ms)
        .or_else(|| parse_ts_ms(fallback))
        .unwrap_or(now_ms);
    (now_ms - parsed).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::event::FleetEventKind;
    use crate::orch::records::OrchestratorEvent;

    fn record(event: &OrchestratorEvent) -> EventRecord {
        event.to_record()
    }

    fn claude_message(value: Value) -> EventRecord {
        record(&OrchestratorEvent::Passthrough(value))
    }

    fn assistant(text: &str) -> Value {
        serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            "session_id": "s",
        })
    }

    #[test]
    fn sent_prompts_render_and_their_replay_is_suppressed() {
        let mut t = Transcript::new();
        t.push_sent("hello there");
        assert_eq!(t.blocks()[0].kind, BlockKind::User);
        assert_eq!(t.blocks()[0].text, "> hello there");
        assert!(t.turn_active());
        // the same text coming back as a replayed user message is dropped
        let replay = serde_json::json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [{"type": "text", "text": "hello there"}]},
        });
        t.apply_claude_message(&replay);
        let users = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::User)
            .count();
        assert_eq!(users, 1, "the replay is suppressed: {:?}", t.blocks());
        // an unknown message (not from us) still renders
        let other = serde_json::json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [{"type": "text", "text": "hi again"}]},
        });
        t.apply_claude_message(&other);
        let users = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::User)
            .count();
        assert_eq!(users, 2);
    }

    #[test]
    fn fleet_batches_render_as_one_line_and_suppress_the_batch_replay() {
        let mut t = Transcript::new();
        let events = vec![
            FleetEvent::new(FleetEventKind::Settled, "r1", "db", vec![]),
            FleetEvent::new(FleetEventKind::Question, "r2", "api", vec![]),
        ];
        t.push_fleet(&events, "FULL BATCH TEXT");
        assert_eq!(
            t.blocks().last().unwrap().text,
            "⚑ settled db · question api"
        );
        assert!(t.pending_echoes.contains(&"FULL BATCH TEXT".to_string()));
    }

    #[test]
    fn partial_is_owned_and_stable_across_calls_no_matter_how_often_the_view_asks() {
        let mut t = Transcript::new();
        let update =
            |body: Value| serde_json::json!({"type": "message_update", "ev": body}).to_string();
        t.apply_worker_lines(&[
            update(serde_json::json!({"type": "text_start", "contentIndex": 0})),
            update(serde_json::json!({"type": "text_delta", "contentIndex": 0, "delta": "st"})),
            update(serde_json::json!({"type": "text_delta", "contentIndex": 0, "delta": "rea"})),
            update(serde_json::json!({"type": "text_delta", "contentIndex": 0, "delta": "ming"})),
        ]);
        // the session view calls this every frame while a worker streams;
        // each call must hand back its own string, all saying the same thing
        let first = t.partial().unwrap();
        let second = t.partial().unwrap();
        assert_eq!(first, "streaming");
        assert_eq!(second, "streaming");
        assert_eq!(first, second);
        // the orchestrator's coalesced partial reads the same way
        let mut o = Transcript::new();
        o.apply_orchestrator_record(&record(&OrchestratorEvent::StreamText {
            text: "hal".into(),
        }));
        o.apply_orchestrator_record(&record(&OrchestratorEvent::StreamText {
            text: "lo".into(),
        }));
        assert_eq!(o.partial().as_deref(), Some("hallo"));
    }

    #[test]
    fn streamed_text_accumulates_then_lands_as_text_blocks() {
        let mut t = Transcript::new();
        t.apply_orchestrator_record(&record(&OrchestratorEvent::StreamText {
            text: "hel".into(),
        }));
        t.apply_orchestrator_record(&record(&OrchestratorEvent::StreamText {
            text: "lo".into(),
        }));
        assert_eq!(t.partial().as_deref(), Some("hello"));
        assert!(t.activity().is_some(), "streaming marks the activity");
        assert!(t.turn_active());
        // the committed assistant message clears the partial
        t.apply_orchestrator_record(&claude_message(assistant("hello\nworld")));
        assert_eq!(t.partial(), None);
        let texts: Vec<&str> = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::Text)
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(texts, vec!["hello", "world"]);
    }

    #[test]
    fn a_task_notification_is_a_system_block_not_a_prompt() {
        let mut t = Transcript::new();
        let msg = serde_json::json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": concat!(
                "<task-notification>\n",
                "<task-id>bp8o9ho35</task-id>\n",
                "<tool-use-id>toolu_01KrKcxKBLorGKu9aZtwHATU</tool-use-id>\n",
                "<status>completed</status>\n",
                "<summary>Background command \"cargo test\" completed (exit code 0)</summary>\n",
                "</task-notification>",
            )},
        });
        t.apply_claude_message(&msg);
        assert!(
            t.blocks().iter().all(|b| b.kind == BlockKind::System),
            "claude wrote it, not the human: {:?}",
            t.blocks()
        );
        let text: Vec<&str> = t.blocks().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(text[0], "· task-notification");
        assert!(
            text.iter().any(|l| l.contains("task-id: bp8o9ho35")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("status: completed")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("cargo test")),
            "the summary is the point: {text:?}"
        );
        assert!(
            !text.iter().any(|l| l.contains("toolu_")),
            "the correlation id costs a row and says nothing: {text:?}"
        );
    }

    #[test]
    fn tool_output_progress_lines_cannot_tear_the_frame() {
        let mut t = Transcript::new();
        let msg = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "git rebase"}},
            ]},
        });
        t.apply_claude_message(&msg);
        // the real payload: git draws progress with carriage returns
        let result = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "t1",
                "content": "Rebasing (1/6)\rRebasing (2/6)\rRebasing (6/6)\rSuccessfully rebased and updated refs/heads/feat/escrow.\n=== exit: 0 ===",
            }]},
        });
        t.apply_claude_message(&result);
        let texts: Vec<&str> = t.blocks().iter().map(|b| b.text.as_str()).collect();
        assert!(
            texts.iter().all(|l| !l.chars().any(char::is_control)),
            "no block carries a control character: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|l| l.contains("Successfully rebased and updated")),
            "what the progress finally said survives: {texts:?}"
        );
        assert!(
            !texts.iter().any(|l| l.contains("Rebasing (1/6)")),
            "the overwritten steps do not: {texts:?}"
        );
    }

    #[test]
    fn a_real_prompt_is_still_a_prompt() {
        let mut t = Transcript::new();
        let msg = serde_json::json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": "<task-notification> is what I want to discuss"},
        });
        t.apply_claude_message(&msg);
        assert_eq!(t.blocks()[0].kind, BlockKind::User);
    }

    #[test]
    fn tool_calls_show_the_command_in_full_and_results_are_previews() {
        let mut t = Transcript::new();
        let msg = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "git status\nsecond line"}},
            ]},
        });
        t.apply_claude_message(&msg);
        let tool_lines: Vec<&str> = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::Tool)
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(tool_lines, vec!["⚙ Bash git status", "      second line"]);
        assert_eq!(
            t.activity().map(|a| a.label.clone()),
            Some(Some("Bash".to_string()))
        );
        let result = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "line1\nline2\nline3\nline4\nline5\nline6"},
                ]},
            ]},
        });
        t.apply_claude_message(&result);
        let results: Vec<&str> = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::ToolResult)
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(
            results.len(),
            5,
            "4 lines plus the counted remainder: {results:?}"
        );
        assert_eq!(results[0], "  ↳ Bash: line1");
        assert_eq!(results[4], "      … 2 more lines", "indent capped at 6");
    }

    #[test]
    fn a_cut_mid_line_counts_the_characters_left_out() {
        let mut t = Transcript::new();
        let long = "x".repeat(700);
        t.apply_worker_lines(&[serde_json::json!({
            "type": "tool_execution_end", "result": {"content": [{"text": long}]},
        })
        .to_string()]);
        let results: Vec<&str> = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::ToolResult)
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(results[1].contains("100 more characters"), "{results:?}");
    }

    #[test]
    fn thinking_is_shown_as_a_counted_head() {
        let mut t = Transcript::new();
        let body = (0..12)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": body}]},
        });
        t.apply_claude_message(&msg);
        let thinking: Vec<&str> = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::Thinking)
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(thinking.len(), 9, "8 lines plus the count: {thinking:?}");
        assert!(thinking[0].starts_with("✻ line0"));
        assert_eq!(thinking[8], "  … 4 more lines of thinking");
    }

    #[test]
    fn system_init_records_the_session_and_model() {
        let mut t = Transcript::new();
        let init = serde_json::json!({
            "type": "system", "subtype": "init",
            "session_id": "sess-abcdef12-3456",
            "model": "fake-model",
            "tools": ["Bash", "mcp__fleet__fleet_spawn"],
            "mcp_servers": [{"name": "fleet", "status": "connected"}],
        });
        t.apply_claude_message(&init);
        assert_eq!(t.session_id(), Some("sess-abcdef12-3456"));
        assert_eq!(t.model(), Some("fake-model"));
        assert!(t.blocks()[0].text.contains("mcp fleet:connected"));
        // a re-init (new session id) does not repeat the banner
        let init2 = serde_json::json!({
            "type": "system", "subtype": "init",
            "session_id": "sess-ffff", "model": "other", "tools": [],
        });
        t.apply_claude_message(&init2);
        assert_eq!(t.session_id(), Some("sess-ffff"));
        assert_eq!(t.blocks().len(), 1);
    }

    #[test]
    fn results_update_cost_and_turns_and_report_failures() {
        let mut t = Transcript::new();
        let result = serde_json::json!({
            "type": "result", "subtype": "success",
            "total_cost_usd": 0.12, "num_turns": 3, "is_error": false,
        });
        t.apply_claude_message(&result);
        assert!((t.cost_usd() - 0.12).abs() < 1e-9);
        assert_eq!(t.num_turns(), 3);
        assert!(!t.turn_active());
        assert_eq!(t.activity(), None);
        let failed = serde_json::json!({
            "type": "result", "subtype": "error_during_execution",
            "is_error": true, "errors": ["boom", "crash"],
        });
        t.apply_claude_message(&failed);
        assert_eq!(
            t.blocks().last().unwrap().text,
            "! turn failed (error_during_execution): boom; crash"
        );
    }

    #[test]
    fn exit_records_the_end() {
        let mut t = Transcript::new();
        t.apply_orchestrator_record(&record(&OrchestratorEvent::Exit {
            code: None,
            signal: Some("SIGTERM".into()),
        }));
        assert!(t.exited());
        assert!(!t.turn_active());
        assert_eq!(
            t.blocks().last().unwrap().text,
            "orchestrator exited (SIGTERM)"
        );
    }

    #[test]
    fn worker_events_fold_into_their_blocks() {
        let mut t = Transcript::new();
        let events = [
            serde_json::json!({"type": "task_prompt", "brief": "fix the tests\nmore"}),
            serde_json::json!({"type": "tool_execution_start", "toolName": "bash", "args": {"command": "cargo test"}}),
            serde_json::json!({"type": "tool_execution_end", "result": {"content": [{"text": "ok"}]}}),
            serde_json::json!({"type": "worker_progress", "message": "halfway"}),
            serde_json::json!({"type": "worker_question", "question": "bcrypt or argon2?", "options": ["bcrypt", "argon2"]}),
            serde_json::json!({"type": "steering_delivered", "source": "console", "message": "use argon2"}),
            serde_json::json!({"type": "run_failed", "error": "disk full"}),
        ];
        t.apply_worker_lines(&events.iter().map(Value::to_string).collect::<Vec<_>>());
        let texts: Vec<&str> = t.blocks().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "▶ task: fix the tests",
                "⚙ bash cargo test",
                "  ↳ ok",
                "· halfway",
                "? bcrypt or argon2? [bcrypt | argon2]",
                "▶ console: use argon2",
                "✖ disk full",
            ],
            "{texts:?}"
        );
    }

    #[test]
    fn worker_streaming_lands_as_text_blocks_on_text_end() {
        let mut t = Transcript::new();
        let update =
            |body: Value| serde_json::json!({"type": "message_update", "ev": body}).to_string();
        t.apply_worker_lines(&[
            update(serde_json::json!({"type": "text_start", "contentIndex": 0})),
            update(serde_json::json!({"type": "text_delta", "contentIndex": 0, "delta": "he"})),
            update(serde_json::json!({"type": "text_delta", "contentIndex": 0, "delta": "llo"})),
        ]);
        assert_eq!(t.partial().as_deref(), Some("hello"));
        t.apply_worker_lines(&[update(serde_json::json!({
            "type": "text_end", "contentIndex": 0, "content": "hello\nworld",
        }))]);
        assert_eq!(t.partial(), None);
        let texts: Vec<&str> = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::Text)
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(texts, vec!["hello", "world"]);
    }

    #[test]
    fn question_resolved_and_dialog_events_render() {
        let mut t = Transcript::new();
        t.apply_worker_lines(&[
            serde_json::json!({"type": "worker_dialog", "method": "select", "question": "Pick one", "options": ["a", "b"]}).to_string(),
            serde_json::json!({"type": "dialog_cancelled"}).to_string(),
            serde_json::json!({"type": "worker_question_resolved", "how": "timeout"}).to_string(),
            serde_json::json!({"type": "agent_settled"}).to_string(),
        ]);
        let texts: Vec<&str> = t.blocks().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "? Pick one [a | b]",
                "· dialog cancelled",
                "! no answer in time; worker proceeds on its own judgment",
                "● settled",
            ]
        );
    }

    #[test]
    fn long_output_is_counted_not_hidden() {
        let mut t = Transcript::new();
        let long = format!("{}\n{}", "x".repeat(700), "y".repeat(700));
        t.apply_worker_lines(&[serde_json::json!({
            "type": "tool_execution_end", "result": {"content": [{"text": long}]},
        })
        .to_string()]);
        let results: Vec<&str> = t
            .blocks()
            .iter()
            .filter(|b| b.kind == BlockKind::ToolResult)
            .map(|b| b.text.as_str())
            .collect();
        // line 1 cut at 600 chars; a second line remains, so it is counted
        assert!(
            results.last().unwrap().contains("1 more line"),
            "{results:?}"
        );
    }

    #[test]
    fn the_transcript_is_capped_and_trims_the_oldest() {
        let mut t = Transcript::new();
        for i in 0..600 {
            t.push_notice(&format!("note {i}"));
        }
        assert_eq!(t.blocks().len(), MAX_BLOCKS);
        assert_eq!(t.blocks()[0].text, "note 100");
    }

    #[test]
    fn assistant_with_tools_builds_a_tool_block() {
        let mut t = Transcript::new();
        let msg = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t9", "name": "Read", "input": {"file_path": "src/main.rs"}},
            ]},
        });
        t.apply_claude_message(&msg);
        assert!(t.blocks().last().unwrap().text.contains("Read src/main.rs"));
        assert_eq!(t.tool_names.get("t9").map(String::as_str), Some("Read"));
    }

    #[test]
    fn replay_worker_reads_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "parl-tui-transcript-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\nnot json\n",
                serde_json::json!({"type": "worker_progress", "message": "one"}),
                serde_json::json!({"type": "worker_progress", "message": "two"}),
            ),
        )
        .unwrap();
        let t = Transcript::replay_worker(&path);
        assert_eq!(t.blocks().len(), 2);
        assert_eq!(t.blocks()[1].text, "· two");
        // a missing file is an empty transcript, not an error
        assert_eq!(
            Transcript::replay_worker(&dir.join("none")).blocks().len(),
            0
        );
    }

    #[test]
    fn age_since_never_goes_negative() {
        assert_eq!(
            age_since(Some("2026-09-30T11:59:52.000Z"), "", 1_790_769_600_000),
            8_000
        );
        assert_eq!(
            age_since(None, "2026-09-30T11:59:52.000Z", 1_790_769_600_000),
            8_000
        );
        assert_eq!(age_since(None, "bogus", 1_790_769_600_000), 0);
    }
}
