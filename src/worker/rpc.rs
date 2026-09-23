//! pi RPC wire types: the commands we send to `pi --mode rpc` on stdin, and
//! the responses and events parsed from its stdout. The protocol is pinned by
//! pi's `docs/rpc.md`; where the TypeScript port and the docs disagreed, the
//! docs won (e.g. `streamingBehavior` is camelCase, `set_model` takes
//! `modelId`).
//!
//! Parsing is deliberately tolerant: an unknown event type or an unexpected
//! field is never an error. Everything that is not a response or an extension
//! UI request parses into the catch-all [`RpcEvent`], which keeps the raw JSON
//! so the monitor can mirror selected types and log every line verbatim.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One parsed line of the pi RPC stream.
#[derive(Debug, Clone)]
pub enum RpcMessage {
    /// A reply to a command we sent.
    Response(RpcResponse),
    /// An extension UI request (dialog or fire-and-forget).
    Ui(ExtensionUiRequest),
    /// Any other event, kept raw.
    Event(RpcEvent),
}

/// Parse one line of the pi RPC stream. `None` for lines that are not JSON at
/// all — they are still logged, just not interpreted.
#[must_use]
pub fn parse_line(line: &str) -> Option<RpcMessage> {
    let raw: Value = serde_json::from_str(line).ok()?;
    match raw.get("type").and_then(Value::as_str) {
        Some("response") => match serde_json::from_value::<RpcResponse>(raw.clone()) {
            Ok(mut response) => {
                response.raw = Some(raw);
                Some(RpcMessage::Response(response))
            }
            // A malformed response is still an event we can mirror.
            Err(_) => RpcEvent::from_value(raw).map(RpcMessage::Event),
        },
        Some("extension_ui_request") => {
            match serde_json::from_value::<ExtensionUiRequest>(raw.clone()) {
                Ok(mut request) => {
                    request.raw = Some(raw);
                    Some(RpcMessage::Ui(request))
                }
                Err(_) => RpcEvent::from_value(raw).map(RpcMessage::Event),
            }
        }
        _ => RpcEvent::from_value(raw).map(RpcMessage::Event),
    }
}

/// A command sent to pi's stdin. Commands the TS monitor sent without an `id`
/// keep carrying none, so the wire stays byte-compatible.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    /// Send a user prompt; with a streaming behavior it queues mid-stream
    /// (and is the only delivery that expands extension commands).
    Prompt {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        message: String,
        #[serde(rename = "streamingBehavior", skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    /// Queue a steering message delivered after the current tool calls.
    Steer { message: String },
    /// Queue a follow-up delivered when the agent finishes.
    FollowUp { message: String },
    /// Abort the current agent operation.
    Abort,
    /// Current session state (model, thinking level, …).
    GetState {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Extension commands, prompt templates and skills.
    GetCommands {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Text of the last assistant message.
    GetLastAssistantText {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Change the reasoning level.
    SetThinkingLevel {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        level: String,
    },
    /// Switch model. `modelId` is camelCase on the wire.
    SetModel {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    /// List every configured model.
    GetAvailableModels {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}

/// What to do with a `prompt` sent while the agent is already streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    /// Deliver after the current turn's tool calls, before the next LLM call.
    Steer,
    /// Deliver only when the agent finishes.
    FollowUp,
}

impl RpcCommand {
    /// Serialize as one JSONL line (no trailing newline).
    #[must_use]
    pub fn to_line(&self) -> String {
        // Serialization of our own commands cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// A reply to a command: `{"id","type":"response","command","success","error","data"}`.
/// Every field except `type` is optional — a malformed response still parses
/// rather than dropping the line.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RpcResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    /// The full parsed line, for mirroring failures into `events.jsonl`.
    #[serde(default)]
    pub raw: Option<Value>,
}

impl RpcResponse {
    /// The model object pi reports (`get_state`, `set_model` data), if any.
    #[must_use]
    pub fn model(&self) -> Option<ModelRef> {
        serde_json::from_value::<ModelRef>(self.data.as_ref()?.get("model")?.clone()).ok()
    }

    /// The thinking level pi reports (`get_state` data).
    #[must_use]
    pub fn thinking_level(&self) -> Option<String> {
        self.data
            .as_ref()?
            .get("thinkingLevel")?
            .as_str()
            .map(str::to_string)
    }

    /// The thinking levels the selected model actually has, in canonical
    /// order, from the `thinkingLevelMap` pi reports with the model
    /// (`get_state`, `set_model`). pi maps every level it knows, using null
    /// for the ones this model does not support — `deepseek-v4-flash` maps
    /// `max` to null, and setting it succeeds while changing nothing. Empty
    /// when pi did not report a map, which reads as "we do not know".
    #[must_use]
    pub fn available_thinking_levels(&self) -> Vec<String> {
        let Some(map) = self
            .data
            .as_ref()
            .and_then(|d| d.get("model"))
            .and_then(|m| m.get("thinkingLevelMap"))
            .and_then(Value::as_object)
        else {
            return Vec::new();
        };
        crate::fleet::run::THINKING_LEVELS
            .iter()
            .filter(|level| map.get(**level).is_some_and(|v| !v.is_null()))
            .map(|level| (*level).to_string())
            .collect()
    }

    /// The commands pi offers (`get_commands` data), unfiltered.
    #[must_use]
    pub fn commands(&self) -> Vec<CommandEntry> {
        serde_json::from_value::<CommandsData>(
            self.data
                .clone()
                .unwrap_or_else(|| Value::Object(Map::default())),
        )
        .map(|d| d.commands)
        .unwrap_or_default()
    }

    /// The models pi has configured (`get_available_models` data).
    #[must_use]
    pub fn available_models(&self) -> Vec<ModelRef> {
        serde_json::from_value::<AvailableModelsData>(
            self.data
                .clone()
                .unwrap_or_else(|| Value::Object(Map::default())),
        )
        .map(|d| d.models)
        .unwrap_or_default()
    }

    /// The text pi reports (`get_last_assistant_text` data).
    #[must_use]
    pub fn text(&self) -> Option<String> {
        self.data
            .as_ref()?
            .get("text")?
            .as_str()
            .map(str::to_string)
    }
}

/// The `model` object from `get_state`/`set_model`/`get_available_models`.
/// pi sends more fields (`api`, `baseUrl`, …); the rest are ignored.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Every level pi knows, the ones this model lacks mapped to null.
    #[serde(default, rename = "thinkingLevelMap")]
    pub thinking_level_map: Option<Map<String, Value>>,
    #[serde(default, rename = "contextWindow")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub cost: Option<crate::fleet::run::ModelCost>,
}

impl ModelRef {
    /// The levels this model has, in [`THINKING_LEVELS`] order: the map's
    /// non-null entries. Empty for a model with no map at all.
    ///
    /// [`THINKING_LEVELS`]: crate::fleet::run::THINKING_LEVELS
    #[must_use]
    pub fn thinking_levels(&self) -> Vec<String> {
        let Some(map) = &self.thinking_level_map else {
            return Vec::new();
        };
        crate::fleet::run::THINKING_LEVELS
            .iter()
            .filter(|level| map.get(**level).is_some_and(|v| !v.is_null()))
            .map(|level| (*level).to_string())
            .collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CommandsData {
    #[serde(default)]
    commands: Vec<CommandEntry>,
}

/// One entry of `get_commands`: an extension command, prompt template or skill.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommandEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AvailableModelsData {
    #[serde(default)]
    models: Vec<ModelRef>,
}

/// Any pi event that is not a response or an extension UI request, kept raw
/// so nothing is dropped: selected types are mirrored into `events.jsonl`
/// and every line lands in `pi.log` regardless.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcEvent {
    /// The event `type` (`agent_settled`, `tool_execution_start`, …).
    pub kind: String,
    /// The full parsed line.
    pub raw: Value,
}

impl RpcEvent {
    fn from_value(raw: Value) -> Option<Self> {
        let kind = raw.get("type")?.as_str()?.to_string();
        Some(Self { kind, raw })
    }

    /// Which phase a `message_update` is streaming, from its
    /// `assistantMessageEvent.type`.
    #[must_use]
    pub fn stream_phase(&self) -> Option<StreamPhase> {
        if self.kind != "message_update" {
            return None;
        }
        let phase = self
            .raw
            .get("assistantMessageEvent")?
            .get("type")?
            .as_str()?;
        match phase {
            "thinking_start" | "thinking_delta" | "thinking_end" => Some(StreamPhase::Thinking),
            "text_start" | "text_delta" | "text_end" => Some(StreamPhase::Text),
            _ => Some(StreamPhase::Other),
        }
    }

    /// The mirrored form of a text-phase `message_update`, shaped like the
    /// TypeScript monitor wrote it (`{"type","ev":{…}}`); `None` for phases
    /// that are not mirrored (thinking, tool calls).
    #[must_use]
    pub fn mirrored_message_update(&self) -> Option<Value> {
        if self.stream_phase() != Some(StreamPhase::Text) {
            return None;
        }
        let a = self.raw.get("assistantMessageEvent")?;
        let mut ev = serde_json::Map::new();
        ev.insert("type".into(), a.get("type")?.clone());
        for key in ["contentIndex", "delta", "content"] {
            if let Some(v) = a.get(key) {
                ev.insert(key.to_string(), v.clone());
            }
        }
        Some(serde_json::json!({ "type": "message_update", "ev": ev }))
    }

    /// The tool name of a `tool_execution_*` event.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        self.raw.get("toolName").and_then(Value::as_str)
    }
}

/// What a `message_update` is streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPhase {
    Thinking,
    Text,
    Other,
}

/// The event types mirrored from the RPC stream into `events.jsonl`.
/// `turn_start`, `message_start/end` and queue chatter are not mirrored; the
/// raw log keeps them.
#[must_use]
pub fn is_selected_event(kind: &str) -> bool {
    matches!(
        kind,
        "agent_start"
            | "agent_end"
            | "agent_settled"
            | "turn_end"
            | "tool_execution_start"
            | "tool_execution_end"
            | "extension_error"
            | "auto_retry_start"
            | "auto_retry_end"
            | "compaction_start"
            | "compaction_end"
    )
}

/// A pi extension UI request (`extension_ui_request`). pi sends more fields
/// per method (`placeholder`, `prefill`, …); unknown ones are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ExtensionUiRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    /// Dialog methods only: milliseconds before pi auto-resolves itself.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// The full parsed line, for mirroring fire-and-forget requests into
    /// `events.jsonl`.
    #[serde(default)]
    pub raw: Option<Value>,
}

impl ExtensionUiRequest {
    /// The four dialog methods that block the agent until answered.
    #[must_use]
    pub fn is_dialog(&self) -> bool {
        matches!(
            self.method.as_str(),
            "select" | "confirm" | "input" | "editor"
        )
    }

    /// Display text: the title, with the body below it when there is one.
    #[must_use]
    pub fn display_question(&self) -> String {
        let title = self.title.as_deref().unwrap_or("(no title)");
        match self.message.as_deref() {
            Some(message) if !message.is_empty() => format!("{title}\n{message}"),
            _ => title.to_string(),
        }
    }
}

/// A reply on pi's stdin to a dialog request. Only one of `value`,
/// `confirmed`, `cancelled` is set, matching pi's expected shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtensionUiResponse {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cancelled: Option<bool>,
}

impl ExtensionUiResponse {
    /// Answer a `select`/`input`/`editor` dialog with a value.
    #[must_use]
    pub fn value(id: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind: "extension_ui_response",
            id: id.into(),
            value: Some(value.into()),
            confirmed: None,
            cancelled: None,
        }
    }

    /// Answer a `confirm` dialog with yes/no.
    #[must_use]
    pub fn confirmed(id: impl Into<String>, confirmed: bool) -> Self {
        Self {
            kind: "extension_ui_response",
            id: id.into(),
            value: None,
            confirmed: Some(confirmed),
            cancelled: None,
        }
    }

    /// Dismiss any dialog; the extension sees `undefined` (or `false`).
    #[must_use]
    pub fn cancelled(id: impl Into<String>) -> Self {
        Self {
            kind: "extension_ui_response",
            id: id.into(),
            value: None,
            confirmed: None,
            cancelled: Some(true),
        }
    }

    /// Serialize as one JSONL line (no trailing newline).
    #[must_use]
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn commands_serialize_to_the_pi_wire_shapes() {
        let prompt = RpcCommand::Prompt {
            id: Some("fleet-init".into()),
            message: "do the thing".into(),
            streaming_behavior: Some(StreamingBehavior::Steer),
        };
        assert_eq!(
            prompt.to_line(),
            r#"{"type":"prompt","id":"fleet-init","message":"do the thing","streamingBehavior":"steer"}"#
        );
        // steer/follow_up/abort carry no id, like the TypeScript monitor.
        assert_eq!(
            RpcCommand::Steer {
                message: "hi".into()
            }
            .to_line(),
            r#"{"type":"steer","message":"hi"}"#
        );
        assert_eq!(
            RpcCommand::FollowUp {
                message: "go on".into()
            }
            .to_line(),
            r#"{"type":"follow_up","message":"go on"}"#
        );
        assert_eq!(RpcCommand::Abort.to_line(), r#"{"type":"abort"}"#);
        assert_eq!(
            RpcCommand::GetState { id: None }.to_line(),
            r#"{"type":"get_state"}"#
        );
        assert_eq!(
            RpcCommand::SetThinkingLevel {
                id: Some("fleet-thinking".into()),
                level: "max".into(),
            }
            .to_line(),
            r#"{"type":"set_thinking_level","id":"fleet-thinking","level":"max"}"#
        );
        // `set_model` takes `modelId` (camelCase) per the pi docs.
        assert_eq!(
            RpcCommand::SetModel {
                id: Some("fleet-model".into()),
                provider: Some("anthropic".into()),
                model_id: "claude-fable-5".into(),
            }
            .to_line(),
            r#"{"type":"set_model","id":"fleet-model","provider":"anthropic","modelId":"claude-fable-5"}"#
        );
        assert_eq!(
            RpcCommand::SetModel {
                id: None,
                provider: None,
                model_id: "m".into(),
            }
            .to_line(),
            r#"{"type":"set_model","modelId":"m"}"#
        );
        assert_eq!(
            RpcCommand::GetAvailableModels { id: None }.to_line(),
            r#"{"type":"get_available_models"}"#
        );
    }

    #[test]
    fn responses_parse_and_expose_their_data() {
        let state = r#"{"type":"response","id":"fleet-state","command":"get_state","success":true,
            "data":{"model":{"id":"m-1","name":"Model One","provider":"vendorco","contextWindow":200000},
            "thinkingLevel":"high","isStreaming":false}}"#;
        let r = match parse_line(state) {
            Some(RpcMessage::Response(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(r.command.as_deref(), Some("get_state"));
        assert_eq!(
            r.model(),
            Some(ModelRef {
                id: Some("m-1".into()),
                name: Some("Model One".into()),
                provider: Some("vendorco".into()),
                // routing weighs it, so it is kept now rather than ignored
                context_window: Some(200_000),
                ..ModelRef::default()
            })
        );
        assert_eq!(r.thinking_level().as_deref(), Some("high"));
    }

    #[test]
    fn a_catalogue_model_carries_its_levels_context_and_cost() {
        // shaped like a real get_available_models entry (pi 0.87)
        let line = r#"{"type":"response","command":"get_available_models","success":true,"data":{"models":[
            {"id":"claude-fable-5","name":"Claude Fable 5","provider":"anthropic","contextWindow":1000000,
             "cost":{"input":10,"output":50,"cacheRead":1,"cacheWrite":12.5},
             "thinkingLevelMap":{"off":null,"xhigh":"xhigh","max":"max"}},
            {"id":"claude-haiku-4-5","provider":"anthropic","thinkingLevelMap":null}]}}"#;
        let RpcMessage::Response(r) = parse_line(line).unwrap() else {
            panic!("a response")
        };
        let models = r.available_models();
        assert_eq!(models[0].thinking_levels(), vec!["xhigh", "max"]);
        assert_eq!(models[0].context_window, Some(1_000_000));
        let cost = models[0].cost.unwrap();
        assert!((cost.input - 10.0).abs() < 1e-9 && (cost.output - 50.0).abs() < 1e-9);
        assert!(
            models[1].thinking_levels().is_empty(),
            "a model with no map has no level to ask for"
        );
    }

    #[test]
    fn available_thinking_levels_come_from_pis_own_map() {
        // the real get_state payload from a deepseek-v4-flash worker: pi maps
        // every level it knows and nulls the ones this model lacks
        let line = r#"{"type":"response","command":"get_state","success":true,"data":{
            "model":{"id":"deepseek/deepseek-v4-flash-0731","provider":"openrouter",
            "thinkingLevelMap":{"off":"none","minimal":null,"low":null,"medium":null,
            "high":"high","xhigh":"xhigh","max":null}},"thinkingLevel":"xhigh"}}"#;
        let RpcMessage::Response(r) = parse_line(line).unwrap() else {
            panic!("a response")
        };
        assert_eq!(
            r.available_thinking_levels(),
            vec!["off", "high", "xhigh"],
            "canonical order, nulls dropped — `max` is not one of them"
        );
        assert_eq!(r.thinking_level().as_deref(), Some("xhigh"));

        // no map at all reads as "we do not know", never as "none"
        let line = r#"{"type":"response","command":"get_state","success":true,
            "data":{"model":{"id":"m"},"thinkingLevel":"high"}}"#;
        let RpcMessage::Response(r) = parse_line(line).unwrap() else {
            panic!("a response")
        };
        assert!(r.available_thinking_levels().is_empty());

        let commands = r#"{"type":"response","command":"get_commands","success":true,"data":{"commands":[
            {"name":"skill:x","description":"a skill","source":"skill"},
            {"description":"no name, skipped by the caller"},
            {"name":"session-name","source":"extension","path":"/tmp/x.ts"}]}}"#;
        let r = match parse_line(commands) {
            Some(RpcMessage::Response(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(r.commands().len(), 3);
        assert_eq!(r.commands()[0].name.as_deref(), Some("skill:x"));

        let models = r#"{"type":"response","command":"get_available_models","success":true,
            "data":{"models":[{"id":"m-1","provider":"vendorco"},{"id":"m-2","name":"Two","provider":"other"}]}}"#;
        let r = match parse_line(models) {
            Some(RpcMessage::Response(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(r.available_models().len(), 2);
        assert_eq!(r.available_models()[1].name.as_deref(), Some("Two"));

        let text = r#"{"type":"response","id":"fleet-last","command":"get_last_assistant_text","success":true,"data":{"text":"done"}}"#;
        let r = match parse_line(text) {
            Some(RpcMessage::Response(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(r.text().as_deref(), Some("done"));

        let failure = r#"{"type":"response","id":"fleet-init","command":"prompt","success":false,"error":"nope"}"#;
        let r = match parse_line(failure) {
            Some(RpcMessage::Response(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(r.success, Some(false));
        assert_eq!(r.error.as_deref(), Some("nope"));
    }

    #[test]
    fn message_update_phases_are_classified_and_mirrored() {
        let thinking = r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hmm"}}"#;
        match parse_line(thinking).unwrap() {
            RpcMessage::Event(ev) => {
                assert_eq!(ev.stream_phase(), Some(StreamPhase::Thinking));
                assert_eq!(ev.mirrored_message_update(), None);
            }
            other => panic!("{other:?}"),
        }
        let text = r#"{"type":"message_update","usage":{},"assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"Hi"}}"#;
        match parse_line(text).unwrap() {
            RpcMessage::Event(ev) => {
                assert_eq!(ev.stream_phase(), Some(StreamPhase::Text));
                assert_eq!(
                    ev.mirrored_message_update(),
                    Some(
                        json!({"type":"message_update","ev":{"type":"text_delta","contentIndex":1,"delta":"Hi"}})
                    )
                );
            }
            other => panic!("{other:?}"),
        }
        // Tool-call deltas are neither thinking nor mirrored text.
        let toolcall = r#"{"type":"message_update","assistantMessageEvent":{"type":"toolcall_start","id":"c1","toolName":"bash"}}"#;
        match parse_line(toolcall).unwrap() {
            RpcMessage::Event(ev) => {
                assert_eq!(ev.stream_phase(), Some(StreamPhase::Other));
                assert_eq!(ev.mirrored_message_update(), None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn selected_events_match_the_ts_mirror_set() {
        for kind in [
            "agent_start",
            "agent_end",
            "agent_settled",
            "turn_end",
            "tool_execution_start",
            "tool_execution_end",
            "extension_error",
            "auto_retry_start",
            "auto_retry_end",
            "compaction_start",
            "compaction_end",
        ] {
            assert!(is_selected_event(kind), "{kind}");
        }
        for kind in ["turn_start", "message_start", "message_end", "queue_update"] {
            assert!(!is_selected_event(kind), "{kind}");
        }
    }

    #[test]
    fn events_keep_the_raw_json_and_tool_names() {
        let tool = r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","result":{"content":[]}}"#;
        match parse_line(tool).unwrap() {
            RpcMessage::Event(ev) => {
                assert_eq!(ev.kind, "tool_execution_end");
                assert_eq!(ev.tool_name(), Some("bash"));
                assert_eq!(ev.raw["toolCallId"], "c1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_event_types_and_fields_are_tolerated() {
        let unknown = r#"{"type":"brand_new_event","futureField":{"deep":[1]}}"#;
        match parse_line(unknown).unwrap() {
            RpcMessage::Event(ev) => {
                assert_eq!(ev.kind, "brand_new_event");
                assert_eq!(ev.raw["futureField"]["deep"], json!([1]));
            }
            other => panic!("{other:?}"),
        }
        // A response with unknown extra fields still parses.
        let extra = r#"{"type":"response","command":"get_state","success":true,"future":"x"}"#;
        assert!(matches!(
            parse_line(extra),
            Some(RpcMessage::Response(RpcResponse { .. }))
        ));
        // Not JSON at all: nothing to interpret (the raw line is still logged).
        assert!(parse_line("garbage").is_none());
        assert!(parse_line("").is_none());
        // A response missing every field still parses into the shell.
        assert!(matches!(
            parse_line(r#"{"type":"response"}"#),
            Some(RpcMessage::Response(_))
        ));
    }

    #[test]
    fn extension_ui_requests_are_typed_with_dialog_classification() {
        let select = r#"{"type":"extension_ui_request","id":"u1","method":"select","title":"Pick",
            "options":["a","b"],"timeout":10000,"futureField":1}"#;
        match parse_line(select).unwrap() {
            RpcMessage::Ui(req) => {
                assert_eq!(req.id, "u1");
                assert!(req.is_dialog());
                assert_eq!(req.options, Some(vec!["a".into(), "b".into()]));
                assert_eq!(req.timeout, Some(10_000));
                assert_eq!(req.display_question(), "Pick");
            }
            other => panic!("{other:?}"),
        }
        let confirm = r#"{"type":"extension_ui_request","id":"u2","method":"confirm","title":"Sure?","message":"All gone."}"#;
        match parse_line(confirm).unwrap() {
            RpcMessage::Ui(req) => {
                assert!(req.is_dialog());
                assert_eq!(req.display_question(), "Sure?\nAll gone.");
            }
            other => panic!("{other:?}"),
        }
        // Fire-and-forget methods need no reply.
        let notify = r#"{"type":"extension_ui_request","id":"u3","method":"notify","message":"heads up","notifyType":"info"}"#;
        match parse_line(notify).unwrap() {
            RpcMessage::Ui(req) => {
                assert!(!req.is_dialog());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ui_responses_serialize_to_pis_expected_shapes() {
        assert_eq!(
            ExtensionUiResponse::value("u1", "Allow").to_line(),
            r#"{"type":"extension_ui_response","id":"u1","value":"Allow"}"#
        );
        assert_eq!(
            ExtensionUiResponse::confirmed("u2", true).to_line(),
            r#"{"type":"extension_ui_response","id":"u2","confirmed":true}"#
        );
        assert_eq!(
            ExtensionUiResponse::cancelled("u3").to_line(),
            r#"{"type":"extension_ui_response","id":"u3","cancelled":true}"#
        );
    }
}
