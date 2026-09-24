//! claude stream-json wire types: the messages claude writes to stdout and
//! the user/control messages it accepts on stdin.
//!
//! Ported from the TypeScript `src/orchestrator/protocol.ts`. Shapes follow
//! the Claude Code CLI (2.1.x) / Agent SDK type definitions. Two rules carry
//! over:
//!
//! - Unknown message types are passed through, never rejected. Messages are
//!   [`serde_json::Value`]s end to end ([`ProtocolMessage`]); the typed
//!   structs below are *views* parsed out of a value, and anything that does
//!   not fit one still travels as the raw value.
//! - Claude validates its own names — `set_model` in particular returns its
//!   own error text for an unknown model. We surface that text verbatim
//!   instead of keeping a model list of our own.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// One decoded claude protocol message.
pub type ProtocolMessage = Value;

/// Decode one stdout line of the claude stream. Malformed JSON is an error;
/// callers that must tolerate junk lines use [`parse_claude_line`].
///
/// # Errors
///
/// Returns an error when `line` is not valid JSON.
pub fn decode_message(line: &str) -> anyhow::Result<ProtocolMessage> {
    serde_json::from_str(line).map_err(|e| anyhow::anyhow!("malformed claude stream line: {e}"))
}

/// One stdout line → message, or `None` when it is not a JSON object with a
/// string `type`.
pub fn parse_claude_line(line: &str) -> Option<ProtocolMessage> {
    let value = decode_message(line).ok()?;
    if !value.is_object() {
        return None;
    }
    value.get("type")?.as_str()?;
    Some(value)
}

// ---------------------------------------------------------------------------
// Message views (what we read from claude's stdout)

/// The `system/init` handshake: session, model, and what this claude offers.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SystemInitMessage {
    pub session_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<McpServerStatus>,
    pub capabilities: Vec<String>,
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
    #[serde(rename = "claude_code_version")]
    pub claude_code_version: Option<String>,
    pub uuid: Option<String>,
}

/// An MCP server's connection state, as reported in `system/init`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct McpServerStatus {
    pub name: String,
    #[serde(default)]
    pub status: String,
}

/// A `system/api_retry` notice: the API call failed and claude is retrying.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct SystemApiRetryMessage {
    pub attempt: u32,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    pub error: Option<String>,
    pub error_status: Option<i32>,
    pub session_id: Option<String>,
}

/// A `result`: how a turn ended, what it cost, and the session it belongs to.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct ResultMessage {
    pub subtype: String,
    pub result: Option<String>,
    pub is_error: Option<bool>,
    pub num_turns: Option<u32>,
    pub total_cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub session_id: Option<String>,
    pub errors: Vec<String>,
    pub stop_reason: Option<String>,
    pub permission_denials: Vec<Value>,
    pub usage: Option<Value>,
}

/// A `control_request` envelope. The body stays a [`Value`]: only
/// `can_use_tool` has a shape the fleet needs (see [`CanUseToolRequest`]).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct ControlRequestMessage {
    pub request_id: String,
    pub request: Value,
}

/// A permission prompt (or an `AskUserQuestion`) waiting for an answer.
///
/// `tool_name` and `tool_use_id` are required: a control request without them
/// is not a permission prompt and stays an untyped [`ControlRequestMessage`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CanUseToolRequest {
    pub tool_name: String,
    #[serde(default = "empty_object")]
    pub input: Value,
    pub tool_use_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, rename = "display_name")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Loosely typed: `{type:"addRules", rules:[{toolName, ruleContent}],
    /// behavior:"allow", destination:"session"}` and friends.
    #[serde(default)]
    pub permission_suggestions: Vec<Value>,
    #[serde(default)]
    pub blocked_path: Option<String>,
    #[serde(default)]
    pub decision_reason: Option<String>,
    #[serde(default, rename = "decision_reason_type")]
    pub decision_reason_type: Option<String>,
    #[serde(default, rename = "agent_id")]
    pub agent_id: Option<String>,
    #[serde(default, rename = "requires_user_interaction")]
    pub requires_user_interaction: Option<bool>,
    #[serde(default, rename = "suppress_always_allow_rule")]
    pub suppress_always_allow_rule: Option<bool>,
    /// Unknown fields ride along so the request can be echoed back faithfully.
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// A `control_response` envelope, resolving one of our control requests.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct ControlResponseMessage {
    pub response: ControlResponseBody,
}

/// The body of a [`ControlResponseMessage`].
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct ControlResponseBody {
    pub subtype: String,
    pub request_id: String,
    pub response: Option<Value>,
    pub error: Option<String>,
}

/// A `control_cancel_request`: claude withdrew a pending control request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct ControlCancelRequestMessage {
    pub request_id: String,
}

/// A slash command or skill the agent offers, from the initialize response.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentCommand {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "argumentHint")]
    pub argument_hint: Option<String>,
    pub aliases: Option<Vec<String>>,
}

/// A permission prompt (or an `AskUserQuestion`) awaiting an answer, as held
/// by the process and written to `state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PermissionRequest {
    pub request_id: String,
    pub request: CanUseToolRequest,
    pub received_at: String,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// Typed view of a message, or `None` when it does not fit.
macro_rules! try_view {
    ($name:ident, $ty:ty) => {
        pub fn $name(msg: &Value) -> Option<$ty> {
            serde_json::from_value(msg.clone()).ok()
        }
    };
}

try_view!(try_system_init, SystemInitMessage);
try_view!(try_system_api_retry, SystemApiRetryMessage);
try_view!(try_result, ResultMessage);
try_view!(try_control_request, ControlRequestMessage);
try_view!(try_control_response, ControlResponseMessage);
try_view!(try_control_cancel_request, ControlCancelRequestMessage);
try_view!(try_agent_command, AgentCommand);

/// The `can_use_tool` body of a control request, when that is what it is.
#[must_use]
pub fn try_can_use_tool(msg: &Value) -> Option<CanUseToolRequest> {
    let request = msg.get("request")?;
    serde_json::from_value(request.clone()).ok()
}

// ---------------------------------------------------------------------------
// Predicates

/// `system/init` — the handshake.
#[must_use]
pub fn is_system_init(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("system")
        && msg.get("subtype").and_then(Value::as_str) == Some("init")
}

/// A `control_request` with a request id to correlate against.
#[must_use]
pub fn is_control_request(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("control_request")
        && msg.get("request_id").is_some_and(Value::is_string)
}

/// A control request asking permission to use a tool.
#[must_use]
pub fn is_can_use_tool(msg: &Value) -> bool {
    is_control_request(msg)
        && msg
            .get("request")
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str)
            == Some("can_use_tool")
}

/// An `AskUserQuestion` permission request (the human answers in the app).
#[must_use]
pub fn is_ask_user_question(request: &CanUseToolRequest) -> bool {
    request.tool_name == "AskUserQuestion"
}

/// An assistant turn (committed content, not deltas).
#[must_use]
pub fn is_assistant(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("assistant")
}

/// A user-side message: a replayed turn or tool results.
#[must_use]
pub fn is_user(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("user")
}

/// A `result` — the end of one turn.
#[must_use]
pub fn is_result(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("result")
}

/// A partial stream event (deltas, block starts).
#[must_use]
pub fn is_stream_event(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("stream_event")
}

/// A `control_response` resolving one of our control requests.
#[must_use]
pub fn is_control_response(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("control_response")
}

/// A `control_cancel_request` withdrawing a pending control request.
#[must_use]
pub fn is_control_cancel_request(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("control_cancel_request")
}

/// A stream event carrying the model's reasoning rather than its answer.
#[must_use]
pub fn is_thinking_event(msg: &Value) -> bool {
    let Some(ev) = msg.get("event") else {
        return false;
    };
    if ev.get("type").and_then(Value::as_str) == Some("content_block_delta") {
        return ev
            .get("delta")
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            == Some("thinking_delta");
    }
    ev.get("type").and_then(Value::as_str) == Some("content_block_start")
        && ev
            .get("content_block")
            .and_then(|b| b.get("type"))
            .and_then(Value::as_str)
            == Some("thinking")
}

// ---------------------------------------------------------------------------
// Content accessors

/// Content blocks of an assistant or user message body, or none.
fn blocks_of(msg: &Value) -> &[Value] {
    msg.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// A `tool_use` block: the model calls a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    #[serde(default = "empty_object")]
    pub input: Value,
}

/// A `tool_result` block delivered in a user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub tool_use_id: String,
    pub text: String,
    pub is_error: bool,
}

/// Concatenated text blocks of an assistant message.
#[must_use]
pub fn text_of_assistant(msg: &Value) -> String {
    join_blocks(msg, "text", "text")
}

/// The model's reasoning blocks, when thinking is on.
#[must_use]
pub fn thinking_of_assistant(msg: &Value) -> String {
    join_blocks(msg, "thinking", "thinking")
}

fn join_blocks(msg: &Value, block_type: &str, field: &str) -> String {
    let parts: Vec<&str> = blocks_of(msg)
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some(block_type))
        .filter_map(|b| b.get(field).and_then(Value::as_str))
        .collect();
    if block_type == "thinking" {
        parts.join("\n")
    } else {
        parts.join("")
    }
}

/// The model's tool calls in an assistant message.
#[must_use]
pub fn tool_uses_of(msg: &Value) -> Vec<ToolUse> {
    blocks_of(msg)
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| serde_json::from_value(b.clone()).ok())
        .collect()
}

/// Text of a tool result block: a plain string, or the text blocks inside it.
#[must_use]
pub fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<&str>>()
            .join(""),
        _ => String::new(),
    }
}

/// Tool results carried by a user message.
#[must_use]
pub fn tool_results_of(msg: &Value) -> Vec<ToolResult> {
    blocks_of(msg)
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|b| ToolResult {
            tool_use_id: b
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            text: tool_result_text(b),
            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect()
}

/// Text of a user message that carries plain text (a replayed turn), or none
/// for tool results.
#[must_use]
pub fn user_text(msg: &Value) -> Option<String> {
    let content = msg.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    if blocks.is_empty()
        || blocks
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return None;
    }
    Some(
        blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<&str>>()
            .join(""),
    )
}

/// With --replay-user-messages, claude echoes our own user messages back;
/// those are not tool results.
#[must_use]
pub fn is_replayed_user_message(msg: &Value) -> bool {
    !msg.get("isSynthetic")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && msg.get("parent_tool_use_id").is_some_and(Value::is_null)
        && user_text(msg).is_some()
}

/// The text of a `thinking_delta` stream event, or none. Reasoning streams
/// like prose does; the monitor coalesces it the same way.
#[must_use]
pub fn thinking_delta_of(msg: &Value) -> Option<String> {
    delta_text_of(msg, "thinking_delta", "thinking")
}

/// The text carried by one `content_block_delta` of `kind`, read from
/// `field`.
fn delta_text_of(msg: &Value, kind: &str, field: &str) -> Option<String> {
    let ev = msg.get("event")?;
    if ev.get("type").and_then(Value::as_str) != Some("content_block_delta") {
        return None;
    }
    let delta = ev.get("delta")?;
    if delta.get("type").and_then(Value::as_str) != Some(kind) {
        return None;
    }
    delta.get(field)?.as_str().map(str::to_string)
}

/// The text of a stream delta, when this event carries one.
#[must_use]
pub fn text_delta_of(msg: &Value) -> Option<String> {
    delta_text_of(msg, "text_delta", "text")
}

// ---------------------------------------------------------------------------
// Builders (what we write to claude's stdin)

/// A user turn (or an async message injected mid-turn).
#[must_use]
pub fn user_message(text: &str) -> Value {
    json!({"type":"user","message":{"role":"user","content":text},"parent_tool_use_id":null})
}

/// A successful control response carrying an arbitrary payload.
#[must_use]
pub fn control_response(request_id: &str, response: &Value) -> Value {
    json!({"type":"control_response","response":{"subtype":"success","request_id":request_id,"response":response}})
}

/// Allow a tool call, optionally with the permission rules claude suggested.
#[must_use]
pub fn allow_response(
    request_id: &str,
    input: Value,
    updated_permissions: Option<&[Value]>,
) -> Value {
    let mut decision = Map::new();
    decision.insert("behavior".into(), json!("allow"));
    decision.insert("updatedInput".into(), input);
    if let Some(perms) = updated_permissions.filter(|p| !p.is_empty()) {
        decision.insert("updatedPermissions".into(), Value::Array(perms.to_vec()));
    }
    control_response(request_id, &Value::Object(decision))
}

/// Deny a tool call with a reason shown to the model.
#[must_use]
pub fn deny_response(request_id: &str, message: &str) -> Value {
    control_response(request_id, &json!({"behavior":"deny","message":message}))
}

/// AskUserQuestion is answered by allowing the tool with the original
/// `questions` echoed back plus `answers` keyed by question text.
#[must_use]
pub fn ask_user_question_response(request_id: &str, input: Value, answers: Value) -> Value {
    let mut merged = match input {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    merged.insert("answers".into(), answers);
    allow_response(request_id, Value::Object(merged), None)
}

/// A control request: `interrupt`, `set_permission_mode`, `initialize`, …
#[must_use]
pub fn control_request(request_id: &str, request: &Value) -> Value {
    json!({"type":"control_request","request_id":request_id,"request":request})
}

/// Stop the running turn; with `cancel_queued`, also drop queued messages.
#[must_use]
pub fn interrupt_request(request_id: &str, cancel_queued: bool) -> Value {
    let body = if cancel_queued {
        json!({"subtype":"interrupt","cancel_queued":true})
    } else {
        json!({"subtype":"interrupt"})
    };
    control_request(request_id, &body)
}

/// Change how prompts are handled mid-session.
#[must_use]
pub fn set_permission_mode_request(request_id: &str, mode: &str) -> Value {
    control_request(
        request_id,
        &json!({"subtype":"set_permission_mode","mode":mode}),
    )
}

/// Change the orchestrator's model live. Claude validates the name itself and
/// answers with its own error text for an unknown one; prefer this over
/// `apply_flag_settings`, which succeeds without validating.
#[must_use]
pub fn set_model_request(request_id: &str, model: &str) -> Value {
    control_request(request_id, &json!({"subtype":"set_model","model":model}))
}

/// Merge settings into the running session — `effort`, say. Unlike sending
/// `/effort` as a message, this changes nothing in the conversation.
#[must_use]
pub fn apply_flag_settings_request(request_id: &str, settings: &Value) -> Value {
    control_request(
        request_id,
        &json!({"subtype":"apply_flag_settings","settings":settings}),
    )
}

/// The SDK's session handshake; every field is optional, so a bare one is valid.
#[must_use]
pub fn initialize_request(request_id: &str, extra: Value) -> Value {
    let mut body = Map::new();
    body.insert("subtype".into(), json!("initialize"));
    if let Value::Object(map) = extra {
        body.extend(map);
    }
    control_request(request_id, &Value::Object(body))
}

/// Serialize one message as a single JSON line.
#[must_use]
pub fn serialize(msg: &Value) -> String {
    let mut line = serde_json::to_string(msg).unwrap_or_default();
    line.push('\n');
    line
}

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Correlation id for a control request: unique per call.
#[must_use]
pub fn new_request_id() -> String {
    let seq = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);
    // now_ms() is i64 epoch millis; clamp negatives to 0, then widen to u64.
    let millis = u64::try_from(crate::util::now_ms().max(0)).unwrap_or_default();
    let random: u64 = rand::random();
    format!(
        "req_{}_{}_{:0>6}",
        base36(millis),
        seq,
        base36(random % 36u64.pow(6))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::split_json_lines;
    use serde_json::json;

    #[test]
    fn stdin_shapes() {
        {
            assert_eq!(
                user_message("hi"),
                json!({"type":"user","message":{"role":"user","content":"hi"},"parent_tool_use_id":null})
            );
            assert_eq!(
                allow_response("r1", json!({"command":"ls"}), None),
                json!({"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"behavior":"allow","updatedInput":{"command":"ls"}}}})
            );
            let perms = json!([{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"ls *"}],"behavior":"allow","destination":"session"}]);
            assert_eq!(
                allow_response(
                    "r2",
                    json!({"command":"ls"}),
                    Some(perms.as_array().unwrap())
                ),
                json!({"type":"control_response","response":{"subtype":"success","request_id":"r2","response":{"behavior":"allow","updatedInput":{"command":"ls"},"updatedPermissions":perms}}})
            );
            assert_eq!(
                allow_response("r3", json!({}), Some(&[])),
                json!({"type":"control_response","response":{"subtype":"success","request_id":"r3","response":{"behavior":"allow","updatedInput":{}}}})
            );
            assert_eq!(
                deny_response("r4", "no"),
                json!({"type":"control_response","response":{"subtype":"success","request_id":"r4","response":{"behavior":"deny","message":"no"}}})
            );
            assert_eq!(
                interrupt_request("i1", false),
                json!({"type":"control_request","request_id":"i1","request":{"subtype":"interrupt"}})
            );
            assert_eq!(
                interrupt_request("i2", true).get("request"),
                Some(&json!({"subtype":"interrupt","cancel_queued":true}))
            );
            assert_eq!(
                set_permission_mode_request("p1", "acceptEdits").get("request"),
                Some(&json!({"subtype":"set_permission_mode","mode":"acceptEdits"}))
            );
            assert_eq!(
                initialize_request("n1", json!({})).get("request"),
                Some(&json!({"subtype":"initialize"}))
            );
            assert_eq!(
                initialize_request("n2", json!({"appendSystemPrompt":"x"})).get("request"),
                Some(&json!({"subtype":"initialize","appendSystemPrompt":"x"}))
            );
            assert_eq!(
                set_model_request("m1", "fable").get("request"),
                Some(&json!({"subtype":"set_model","model":"fable"}))
            );
        }
        {
            let input = json!({
                "questions": [{"question":"Which style?","header":"Style","options":[{"label":"A","description":""},{"label":"B","description":""}],"multiSelect":false}],
            });
            let msg = ask_user_question_response("q1", input.clone(), json!({"Which style?":"B"}));
            assert_eq!(msg["response"]["subtype"], "success");
            assert_eq!(msg["response"]["response"]["behavior"], "allow");
            assert_eq!(
                msg["response"]["response"]["updatedInput"],
                json!({"questions": input["questions"], "answers": {"Which style?": "B"}})
            );
        }
        {
            let line = serialize(&user_message("x"));
            assert!(line.ends_with('\n'));
            assert_eq!(line.find('\n'), Some(line.len() - 1));
            assert_ne!(new_request_id(), new_request_id());
            assert!(new_request_id().starts_with("req_"));
        }
    }

    #[test]
    fn parse_claude_output() {
        {
            assert_eq!(parse_claude_line("not json"), None);
            assert_eq!(parse_claude_line("[1,2]"), None);
            assert_eq!(parse_claude_line(r#"{"foo":1}"#), None);
            assert_eq!(
                parse_claude_line(r#"{"type":"mystery","x":1}"#),
                Some(json!({"type":"mystery","x":1}))
            );
            assert!(decode_message("not json").is_err());
        }
        {
            let a = split_json_lines(
                "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}\r\n{\"type\":\"assis",
                "",
            );
            assert_eq!(a.lines.len(), 1);
            let msg = parse_claude_line(&a.lines[0]).unwrap();
            assert!(is_system_init(&msg));
            let b = split_json_lines(
                "tant\",\"message\":{\"role\":\"assistant\",\"content\":[]},\"parent_tool_use_id\":null}\n",
                &a.rest,
            );
            assert_eq!(b.lines.len(), 1);
            assert!(is_assistant(&parse_claude_line(&b.lines[0]).unwrap()));
        }
        {
            let init = parse_claude_line(
                r#"{"type":"system","subtype":"init","session_id":"s","cwd":"/repo","model":"claude-fable-5",
                    "tools":["Bash"],"mcp_servers":[{"name":"fleet","status":"connected"}],
                    "capabilities":["interrupt_receipt_v1"],"permissionMode":"default",
                    "claude_code_version":"2.1.251","uuid":"u","futureField":{"x":1}}"#,
            )
            .unwrap();
            let view = try_system_init(&init).unwrap();
            assert_eq!(view.session_id, "s");
            assert_eq!(view.model.as_deref(), Some("claude-fable-5"));
            assert_eq!(view.capabilities, vec!["interrupt_receipt_v1".to_string()]);
            assert_eq!(view.mcp_servers.len(), 1);
            assert_eq!(view.mcp_servers[0].name, "fleet");

            let result = parse_claude_line(
                r#"{"type":"result","subtype":"success","result":"done","is_error":false,"num_turns":2,
                    "total_cost_usd":0.01,"duration_ms":900,"session_id":"s","stop_reason":null}"#,
            )
            .unwrap();
            let view = try_result(&result).unwrap();
            assert_eq!(view.subtype, "success");
            assert_eq!(view.result.as_deref(), Some("done"));
            assert_eq!(view.total_cost_usd, Some(0.01));
            assert_eq!(view.num_turns, Some(2));
            assert_eq!(view.stop_reason, None);
        }
        {
            let req = parse_claude_line(
                r#"{"type":"control_request","request_id":"abc","request":{"subtype":"can_use_tool",
                    "tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"t1","title":"Run ls"}}"#,
            )
            .unwrap();
            assert!(is_can_use_tool(&req));
            let body = try_can_use_tool(&req).unwrap();
            assert_eq!(body.tool_name, "Bash");
            assert_eq!(body.input["command"], "ls");
            assert_eq!(body.title.as_deref(), Some("Run ls"));
            assert!(!is_ask_user_question(&body));
            let as_ask = CanUseToolRequest {
                tool_name: "AskUserQuestion".into(),
                ..body
            };
            assert!(is_ask_user_question(&as_ask));

            let other = parse_claude_line(
                r#"{"type":"control_request","request_id":"x","request":{"subtype":"hook_callback"}}"#,
            )
            .unwrap();
            assert!(!is_can_use_tool(&other));
            assert_eq!(try_can_use_tool(&other), None);
        }
        {
            let msg = json!({
                "type":"assistant",
                "message":{"role":"assistant","content":[
                    {"type":"text","text":"Hello "},
                    {"type":"tool_use","id":"t1","name":"mcp__fleet__fleet_status","input":{}},
                    {"type":"text","text":"world"},
                ]},
                "parent_tool_use_id":null,
            });
            assert_eq!(text_of_assistant(&msg), "Hello world");
            let uses = tool_uses_of(&msg);
            assert_eq!(uses.len(), 1);
            assert_eq!(uses[0].name, "mcp__fleet__fleet_status");
            assert_eq!(uses[0].id, "t1");
            assert_eq!(
                thinking_of_assistant(&json!({
                    "type":"assistant",
                    "message":{"role":"assistant","content":[
                        {"type":"thinking","thinking":"hmm"},
                        {"type":"thinking","thinking":"ok"},
                    ]},
                })),
                "hmm\nok"
            );
        }
        {
            let replay = json!({"type":"user","message":{"role":"user","content":"hi there"},"parent_tool_use_id":null});
            assert!(is_replayed_user_message(&replay));
            assert_eq!(user_text(&replay).as_deref(), Some("hi there"));

            let blocks = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},"parent_tool_use_id":null});
            assert_eq!(user_text(&blocks).as_deref(), Some("ab"));

            let result = json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"ok\nmore"}],"is_error":false}
            ]},"parent_tool_use_id":null});
            assert!(!is_replayed_user_message(&result));
            assert_eq!(user_text(&result), None);
            assert_eq!(
                tool_results_of(&result),
                vec![ToolResult {
                    tool_use_id: "t1".into(),
                    text: "ok\nmore".into(),
                    is_error: false
                }]
            );

            let string_result = json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t2","content":"plain","is_error":true}
            ]},"parent_tool_use_id":null});
            assert_eq!(
                tool_results_of(&string_result),
                vec![ToolResult {
                    tool_use_id: "t2".into(),
                    text: "plain".into(),
                    is_error: true
                }]
            );

            let synthetic = json!({
                "type":"user","message":{"role":"user","content":"hi there"},
                "parent_tool_use_id":null,"isSynthetic":true,
            });
            assert!(!is_replayed_user_message(&synthetic));
            // missing parent_tool_use_id is not the same as null, as in TypeScript
            let unparented = json!({"type":"user","message":{"role":"user","content":"hi"}});
            assert!(!is_replayed_user_message(&unparented));
        }
        {
            let delta = json!({
                "type":"stream_event",
                "event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"tok"}},
                "parent_tool_use_id":null,
            });
            assert_eq!(text_delta_of(&delta).as_deref(), Some("tok"));
            assert!(is_stream_event(&delta));
            let json_delta = json!({
                "type":"stream_event",
                "event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}},
            });
            assert_eq!(text_delta_of(&json_delta), None);
            assert_eq!(
                text_delta_of(&json!({"type":"stream_event","event":{"type":"message_start"}})),
                None
            );
            assert!(is_thinking_event(&json!({
                "type":"stream_event",
                "event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"h"}},
            })));
            assert!(is_thinking_event(&json!({
                "type":"stream_event",
                "event":{"type":"content_block_start","content_block":{"type":"thinking"}},
            })));
            assert!(!is_thinking_event(&delta));

            let result = parse_claude_line(
                r#"{"type":"result","subtype":"success","result":"done","session_id":"s","total_cost_usd":0.01}"#,
            )
            .unwrap();
            assert!(is_result(&result));
            assert!(is_user(
                &parse_claude_line(r#"{"type":"user","message":{"role":"user","content":"x"},"parent_tool_use_id":null}"#)
                    .unwrap()
            ));
            assert!(is_control_cancel_request(
                &parse_claude_line(r#"{"type":"control_cancel_request","request_id":"r"}"#)
                    .unwrap()
            ));
            let response = try_control_response(
                &parse_claude_line(
                    r#"{"type":"control_response","response":{"subtype":"error","request_id":"r","error":"no"}}"#,
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(response.response.error.as_deref(), Some("no"));
            assert_eq!(response.response.subtype, "error");
        }
    }
}
