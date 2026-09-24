//! Orchestrator records: the monitor's durable state, the console→monitor
//! mailbox commands, the monitor→console transcript events, and the writer
//! that appends them to `orchestrator/events.jsonl` with token deltas
//! coalesced.
//!
//! Ported from the TypeScript `src/orchestrator/records.ts`. The mailbox
//! itself is the shared [`Envelope`] shell (AGENTS.md: every `inbox.jsonl`
//! line is an envelope), addressed `to: orchestrator`; this module defines
//! the payload types and their decode. Everything is serde-tolerant of
//! unknown and missing fields, like the run state is: a newer writer cannot
//! crash an older reader, and unknown types decode to `None` so readers skip
//! the line.
//!
//! [`Envelope`]: crate::fleet::envelope::Envelope

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::fleet::envelope::{Envelope, Party};
use crate::orch::protocol::{AgentCommand, CanUseToolRequest, McpServerStatus, PermissionRequest};
use crate::paths::SessionKey;
use crate::util::{append_json_line, now_iso};

/// The rendered orchestrator prompt handed to `--append-system-prompt-file`:
/// `<fleetDir>/orchestrators/<key>/prompt.md`.
#[must_use]
pub fn prompt_path(fleet_dir: &Path, key: &SessionKey) -> PathBuf {
    crate::paths::FleetPaths::new(fleet_dir).orchestrator_prompt(key)
}

/// What the orchestrator is doing right now: reasoning, writing, or in a tool.
///
/// Lives here rather than in the TUI — the monitor derives it from the wire
/// and the TUI renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityKind {
    Thinking,
    Responding,
    Tool,
}

/// One activity snapshot; `since` is epoch millis (for the elapsed counter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub kind: ActivityKind,
    /// The tool being run, when that is what it is doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub since: i64,
}

impl Activity {
    /// A fresh activity starting now.
    #[must_use]
    pub fn starting(kind: ActivityKind, label: Option<String>) -> Self {
        Self {
            kind,
            label,
            since: crate::util::now_ms(),
        }
    }
}

/// How a console answered a pending request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "behavior",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PermissionDecisionRecord {
    /// Allow, optionally with the rules claude suggested.
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<Vec<Value>>,
    },
    Deny {
        message: String,
    },
    /// Answer an AskUserQuestion (values keyed by question text).
    Answer {
        answers: Value,
    },
}

/// Console → monitor, as carried in an [`Envelope`] payload (the type tag
/// rides on the envelope; the payload holds the fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum OrchestratorCommand {
    User {
        text: String,
    },
    Permission {
        request_id: String,
        decision: PermissionDecisionRecord,
    },
    Interrupt,
    Effort {
        level: String,
    },
    PermissionMode {
        mode: String,
    },
    RemoteControl {
        name: Option<String>,
    },
    /// Change the orchestrator's model live (`set_model`); claude validates
    /// the name itself and its error text is surfaced verbatim.
    Model {
        name: String,
    },
    /// Cut `events.jsonl` to its recent tail. Sent to the monitor rather than
    /// done by the console, because the monitor is the file's only writer.
    TrimTranscript,
    /// Ask the agent what it currently offers and rewrite
    /// `capabilities.json`. The console sends this instead of trusting a
    /// snapshot taken at handshake time.
    RefreshCapabilities,
    Stop,
}

impl OrchestratorCommand {
    /// Typed view of an envelope payload, or `None` for an unknown type or a
    /// payload that does not fit — readers skip those lines.
    #[must_use]
    pub fn decode(kind: &str, payload: &Value) -> Option<Self> {
        let mut tagged = payload.clone();
        let Value::Object(map) = &mut tagged else {
            return None;
        };
        map.insert("type".into(), Value::String(kind.to_string()));
        serde_json::from_value(tagged).ok()
    }

    /// The envelope `type` and payload for this command.
    #[must_use]
    pub fn payload(&self) -> (String, Value) {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new()));
        let kind = match &mut value {
            Value::Object(map) => map
                .remove("type")
                .and_then(|t| t.as_str().map(str::to_string))
                .unwrap_or_default(),
            _ => String::new(),
        };
        (kind, value)
    }

    /// Wrap this command as an envelope from `from` to the orchestrator.
    /// Addresses the default session: the inbox file it lands in is the
    /// session's own, and the monitor accepts any orchestrator party.
    #[must_use]
    pub fn to_envelope(&self, from: Party) -> Envelope {
        let (kind, payload) = self.payload();
        Envelope::new(
            from,
            Party::Orchestrator(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION),
            &kind,
            payload,
        )
    }
}

/// Decode a mailbox envelope into a command. Only envelopes addressed to
/// the orchestrator decode; unknown types yield `None`. Any orchestrator
/// party counts — a session's own line and a legacy bare `"orchestrator"`
/// line land in the same physical inbox file.
#[must_use]
pub fn decode_command(envelope: &Envelope) -> Option<OrchestratorCommand> {
    if !matches!(envelope.to, Party::Orchestrator(_)) {
        return None;
    }
    OrchestratorCommand::decode(&envelope.kind, &envelope.payload)
}

/// The claude child being replaced (Remote Control needs a new flag), noted
/// so the monitor does not mistake a restart for an exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ExitedRecord {
    pub code: Option<i32>,
    pub signal: Option<String>,
    pub at: String,
}

/// What a console needs to know without replaying the whole transcript:
/// `orchestrator/state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct OrchestratorState {
    #[serde(default = "default_version")]
    pub version: u8,
    /// The monitor's pid; the claude child lives and dies with it.
    pub pid: Option<i32>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub claude_version: Option<String>,
    pub cost_usd: f64,
    pub num_turns: u32,
    pub turn_active: bool,
    pub activity: Option<Activity>,
    /// Reasoning level last asked for, since claude does not report one.
    pub effort: Option<String>,
    /// How permission prompts are handled: default, auto, acceptEdits, dontAsk, plan.
    pub permission_mode: String,
    /// Remote Control name this session was started with, or none when off.
    pub remote_control: Option<String>,
    pub started_at: String,
    pub last_activity: Option<String>,
    pub pending_requests: Vec<PermissionRequest>,
    /// Set once the child is gone; the console then offers to start a new one.
    pub exited: Option<ExitedRecord>,
    pub cwd: String,
}

/// The version stamped on freshly created state.
const STATE_VERSION: u8 = 1;

const fn default_version() -> u8 {
    STATE_VERSION
}

/// A fresh state for a monitor starting in `cwd`.
#[must_use]
pub fn new_orchestrator_state(cwd: &str) -> OrchestratorState {
    OrchestratorState {
        version: STATE_VERSION,
        permission_mode: "default".to_string(),
        started_at: now_iso(),
        cwd: cwd.to_string(),
        ..OrchestratorState::default()
    }
}

// ---------------------------------------------------------------------------
// Capabilities (capabilities.json)

/// The model names claude accepts. There is no control request that lists
/// models (AGENTS.md, verified fact 3), so unlike everything else in
/// [`Capabilities`] this one list cannot be asked for and has to be stated.
pub const ORCHESTRATOR_MODEL_ALIASES: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "opusplan"];

/// What the agent on the other end offers right now:
/// `orchestrators/<key>/capabilities.json`.
///
/// Deliberately not part of [`OrchestratorState`]. Capabilities change while
/// a session runs — install a skill and claude's command list grows — so
/// they are asked for ([`OrchestratorCommand::RefreshCapabilities`]) and
/// rewritten, never snapshotted at handshake time and left to rot. The
/// console reads this file, shows `fetched_at` as its age, and asks for a
/// refresh when it goes stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Capabilities {
    /// When the agent last answered, RFC3339.
    pub fetched_at: String,
    /// Every tool the agent has, from `system/init` — including the
    /// `mcp__*` ones, which is how the MCP servers get their tool lists.
    pub tools: Vec<String>,
    /// Slash commands and skills, from the `initialize` response.
    pub commands: Vec<AgentCommand>,
    pub mcp_servers: Vec<McpServerStatus>,
    /// Protocol capabilities claude reports (`interrupt_receipt_v1`, …).
    pub capabilities: Vec<String>,
    /// Model names to offer, [`ORCHESTRATOR_MODEL_ALIASES`] plus whatever
    /// the session is actually running.
    pub models: Vec<String>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            fetched_at: String::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            mcp_servers: Vec::new(),
            capabilities: Vec::new(),
            models: ORCHESTRATOR_MODEL_ALIASES
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

impl Capabilities {
    /// How long ago the agent answered, or `None` when it never has or the
    /// stamp does not parse.
    #[must_use]
    pub fn age(&self) -> Option<std::time::Duration> {
        crate::util::age_of(&self.fetched_at)
    }

    /// Whether the console should ask for a refresh before showing this.
    #[must_use]
    pub fn is_stale(&self, max_age: std::time::Duration) -> bool {
        self.age().is_none_or(|age| age > max_age)
    }
}

/// Read `capabilities.json`, or defaults when it is missing or unparsable —
/// an unreadable capability file degrades to "nothing known yet", never to
/// an error.
#[must_use]
pub fn read_capabilities(path: &Path) -> Capabilities {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Write `capabilities.json` atomically.
///
/// # Errors
///
/// Returns `std::io::Error` when serialization or the atomic rename fails.
pub fn write_capabilities(path: &Path, caps: &Capabilities) -> std::io::Result<()> {
    crate::util::atomic_write_json(path, caps)
}

// ---------------------------------------------------------------------------
// Monitor → console transcript events (events.jsonl lines)

/// One `events.jsonl` line: `{"type": "...", ...}`. Known types decode to
/// [`OrchestratorEvent`]; unknown ones pass through untouched, so a newer
/// monitor's records are still replayable by an older console.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EventRecord {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub body: Map<String, Value>,
}

/// Monitor → console. Claude's own messages ride through as they are, except
/// token deltas, which are coalesced into [`OrchestratorEvent::StreamText`]
/// so the file stays small and a reattaching console can still replay what
/// was said.
#[derive(Debug, Clone, PartialEq)]
pub enum OrchestratorEvent {
    StreamText {
        text: String,
    },
    /// Coalesced reasoning, written like [`Self::StreamText`] so the console
    /// can show thinking as it is written instead of at turn end.
    StreamThinking {
        text: String,
    },
    Activity {
        activity: Option<Activity>,
    },
    PermissionRequest {
        request_id: String,
        request: Value,
    },
    PermissionResolved {
        request_id: String,
        how: String,
    },
    Notice {
        text: String,
        error: Option<bool>,
    },
    Exit {
        code: Option<i32>,
        signal: Option<String>,
    },
    /// A claude message (or an unknown future type) verbatim.
    Passthrough(Value),
}

impl OrchestratorEvent {
    /// The on-disk record for this event.
    #[must_use]
    pub fn to_record(&self) -> EventRecord {
        let mut body = Map::new();
        let kind = match self {
            Self::StreamText { text } => {
                body.insert("text".into(), Value::String(text.clone()));
                "stream_text"
            }
            Self::StreamThinking { text } => {
                body.insert("text".into(), Value::String(text.clone()));
                "stream_thinking"
            }
            Self::Activity { activity } => {
                body.insert(
                    "activity".into(),
                    serde_json::to_value(activity).unwrap_or(Value::Null),
                );
                "activity"
            }
            Self::PermissionRequest {
                request_id,
                request,
            } => {
                body.insert("requestId".into(), Value::String(request_id.clone()));
                body.insert("request".into(), request.clone());
                "permission_request"
            }
            Self::PermissionResolved { request_id, how } => {
                body.insert("requestId".into(), Value::String(request_id.clone()));
                body.insert("how".into(), Value::String(how.clone()));
                "permission_resolved"
            }
            Self::Notice { text, error } => {
                body.insert("text".into(), Value::String(text.clone()));
                if let Some(flag) = error {
                    body.insert("error".into(), Value::Bool(*flag));
                }
                "notice"
            }
            Self::Exit { code, signal } => {
                body.insert(
                    "code".into(),
                    serde_json::to_value(code).unwrap_or(Value::Null),
                );
                body.insert(
                    "signal".into(),
                    serde_json::to_value(signal).unwrap_or(Value::Null),
                );
                "exit"
            }
            Self::Passthrough(value) => {
                let mut record =
                    serde_json::from_value::<EventRecord>(value.clone()).unwrap_or_default();
                record.body.extend(body);
                return record;
            }
        };
        EventRecord {
            kind: kind.to_string(),
            body,
        }
    }
}

impl EventRecord {
    /// Typed view of this record. Claude's own messages and unknown types
    /// decode to [`OrchestratorEvent::Passthrough`] — never rejected.
    #[must_use]
    pub fn decode(&self) -> OrchestratorEvent {
        let body = &self.body;
        match self.kind.as_str() {
            "stream_text" => OrchestratorEvent::StreamText {
                text: body
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "stream_thinking" => OrchestratorEvent::StreamThinking {
                text: body
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "activity" => OrchestratorEvent::Activity {
                activity: body
                    .get("activity")
                    .filter(|v| !v.is_null())
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
            },
            "permission_request" => OrchestratorEvent::PermissionRequest {
                request_id: string_field(body, "requestId"),
                request: body.get("request").cloned().unwrap_or(Value::Null),
            },
            "permission_resolved" => OrchestratorEvent::PermissionResolved {
                request_id: string_field(body, "requestId"),
                how: string_field(body, "how"),
            },
            "notice" => OrchestratorEvent::Notice {
                text: string_field(body, "text"),
                error: body.get("error").and_then(Value::as_bool),
            },
            "exit" => OrchestratorEvent::Exit {
                // codes are written as i32 by us; a wider foreign value is
                // treated as unknown rather than wrapped
                code: body
                    .get("code")
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok()),
                signal: body
                    .get("signal")
                    .filter(|v| !v.is_null())
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            _ => {
                let mut value = Map::new();
                value.insert("type".into(), Value::String(self.kind.clone()));
                value.extend(body.clone());
                OrchestratorEvent::Passthrough(Value::Object(value))
            }
        }
    }
}

fn string_field(body: &Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Append one record as a JSONL line (creating the file when missing).
///
/// Errors are the caller's problem: the transcript is best effort, and
/// `state.json` is what matters.
///
/// # Errors
///
/// Returns an I/O error when the file cannot be created or appended to.
pub fn append_record(events_path: &Path, record: &EventRecord) -> std::io::Result<()> {
    append_json_line(events_path, record)
}

/// The transcript writer: appends records and coalesces streamed text into
/// one `stream_text` record per flush, so the file stays small.
#[derive(Debug)]
pub struct Transcript {
    path: PathBuf,
    pending_text: String,
    pending_thinking: String,
}

impl Transcript {
    /// A writer appending to `path`.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self {
            path,
            pending_text: String::new(),
            pending_thinking: String::new(),
        }
    }

    /// Accumulate streamed tokens; flushed as one record per [`Self::flush_text`].
    pub fn stream_text(&mut self, delta: &str) {
        self.pending_text.push_str(delta);
    }

    /// Accumulate streamed reasoning, flushed alongside the text.
    pub fn stream_thinking(&mut self, delta: &str) {
        self.pending_thinking.push_str(delta);
    }

    /// Write any pending reasoning and text, in that order — a model thinks
    /// before it answers, and the records have to read that way too.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when a record cannot be appended.
    pub fn flush_text(&mut self) -> std::io::Result<bool> {
        let mut wrote = false;
        if !self.pending_thinking.is_empty() {
            let text = std::mem::take(&mut self.pending_thinking);
            append_record(
                &self.path,
                &OrchestratorEvent::StreamThinking { text }.to_record(),
            )?;
            wrote = true;
        }
        if !self.pending_text.is_empty() {
            let text = std::mem::take(&mut self.pending_text);
            append_record(
                &self.path,
                &OrchestratorEvent::StreamText { text }.to_record(),
            )?;
            wrote = true;
        }
        Ok(wrote)
    }

    /// Append one record, flushing pending text first so ordering stays sane.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when either record cannot be appended.
    pub fn write(&mut self, event: &OrchestratorEvent) -> std::io::Result<()> {
        self.flush_text()?;
        append_record(&self.path, &event.to_record())
    }
}

/// Once a transcript file holds more than `over` lines, cut it to its last
/// `keep`. The rewrite goes to a temporary file that is renamed over the
/// original, so a reader never sees a half-written file, and the new inode
/// is how a reader tailing by offset knows to start again.
///
/// Best effort: an unreadable or unwritable file is left alone, because a
/// transcript that cannot be trimmed is better than one that is lost.
/// Returns whether the file was trimmed.
pub fn trim_events_file(events_path: &Path, over: usize, keep: usize) -> bool {
    let Ok(raw) = std::fs::read_to_string(events_path) else {
        return false;
    };
    let lines: Vec<&str> = raw.split('\n').filter(|l| !l.is_empty()).collect();
    if lines.len() <= over {
        return false;
    }
    let keep = keep.min(lines.len());
    let trimmed = format!("{}\n", lines[lines.len() - keep..].join("\n"));
    let tmp = events_path.with_extension("jsonl.trim");
    if std::fs::write(&tmp, trimmed).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    std::fs::rename(&tmp, events_path).is_ok()
}

/// Pending requests ordered by arrival, newest last — what `state.json` holds.
#[must_use]
pub fn sorted_pending(pending: &HashMap<String, PermissionRequest>) -> Vec<PermissionRequest> {
    let mut list: Vec<PermissionRequest> = pending.values().cloned().collect();
    // RFC3339 with milliseconds sorts lexicographically; break ties on id.
    list.sort_by(|a, b| {
        a.received_at
            .cmp(&b.received_at)
            .then(a.request_id.cmp(&b.request_id))
    });
    list
}

/// The `can_use_tool` body as a passthrough `Value` for transcript records.
#[must_use]
pub fn request_value(request: &CanUseToolRequest) -> Value {
    serde_json::to_value(request).unwrap_or_else(|_| Value::Object(Map::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::envelope::Envelope;
    use crate::orch::protocol::PermissionRequest;
    use serde_json::json;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        value: &T,
    ) -> T {
        let line = serde_json::to_string(value).unwrap();
        let parsed: T = serde_json::from_str(&line).unwrap();
        assert_eq!(*value, parsed, "line: {line}");
        parsed
    }

    #[test]
    fn command_envelopes() {
        {
            let commands = vec![
                OrchestratorCommand::User {
                    text: "hello".into(),
                },
                OrchestratorCommand::Permission {
                    request_id: "req_1".into(),
                    decision: PermissionDecisionRecord::Allow {
                        updated_permissions: Some(vec![json!({"type":"addRules"})]),
                    },
                },
                OrchestratorCommand::Permission {
                    request_id: "req_2".into(),
                    decision: PermissionDecisionRecord::Deny {
                        message: "no".into(),
                    },
                },
                OrchestratorCommand::Permission {
                    request_id: "req_3".into(),
                    decision: PermissionDecisionRecord::Answer {
                        answers: json!({"Which style?": "terse"}),
                    },
                },
                OrchestratorCommand::Interrupt,
                OrchestratorCommand::Effort {
                    level: "high".into(),
                },
                OrchestratorCommand::PermissionMode {
                    mode: "acceptEdits".into(),
                },
                OrchestratorCommand::RemoteControl { name: None },
                OrchestratorCommand::RemoteControl {
                    name: Some("phone".into()),
                },
                OrchestratorCommand::Model {
                    name: "fable".into(),
                },
                OrchestratorCommand::Stop,
            ];
            for command in &commands {
                let envelope = command.to_envelope(Party::Console);
                assert_eq!(
                    envelope.to,
                    Party::Orchestrator(crate::fleet::envelope::DEFAULT_ORCHESTRATOR_SESSION)
                );
                assert!(envelope.id.starts_with("m_"));
                let line = serde_json::to_string(&envelope).unwrap();
                let parsed = Envelope::parse_line(&line).unwrap();
                let decoded = decode_command(&parsed).unwrap();
                assert_eq!(&decoded, command, "line: {line}");
            }
            // Wire shapes are pinned: camelCase fields, no type tag in the payload.
            let envelope = commands[9].to_envelope(Party::Console);
            assert_eq!(envelope.kind, "model");
            assert_eq!(envelope.payload, json!({"name":"fable"}));
            let permission = commands[1].to_envelope(Party::Console);
            assert_eq!(permission.kind, "permission");
            assert_eq!(
                permission.payload,
                json!({"requestId":"req_1","decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules"}]}})
            );
        }
        {
            let line = r#"{"id":"m_x","ts":"2026-08-30T12:00:00.000Z","from":"console","to":"orchestrator","type":"brand_new","payload":{"whatever":1}}"#;
            let envelope = Envelope::parse_line(line).unwrap();
            assert_eq!(decode_command(&envelope), None);
            // Wrong party: not for the orchestrator.
            let line = r#"{"id":"m_x","ts":"t","from":"console","to":"worker:r","type":"user","payload":{"text":"hi"}}"#;
            let envelope = Envelope::parse_line(line).unwrap();
            assert_eq!(decode_command(&envelope), None);
            // Known type, payload of the wrong shape: parses, decodes to None.
            let line = r#"{"id":"m_x","ts":"t","from":"console","to":"orchestrator","type":"user","payload":{"oops":1}}"#;
            let envelope = Envelope::parse_line(line).unwrap();
            assert_eq!(decode_command(&envelope), None);
        }
    }

    #[test]
    fn state_files() {
        {
            let mut state = new_orchestrator_state("/repo");
            state.pid = Some(4321);
            state.session_id = Some("sess".into());
            state.model = Some("claude-fable-5".into());
            state.cost_usd = 0.05;
            state.num_turns = 3;
            state.turn_active = true;
            state.activity = Some(Activity {
                kind: ActivityKind::Tool,
                label: Some("Bash".into()),
                since: 1_760_000_000_000,
            });
            state.effort = Some("high".into());
            state.pending_requests = vec![PermissionRequest {
                request_id: "req_1".into(),
                request: CanUseToolRequest {
                    tool_name: "Bash".into(),
                    input: json!({"command":"ls"}),
                    tool_use_id: "t1".into(),
                    title: Some("Run ls".into()),
                    ..CanUseToolRequest::default()
                },
                received_at: "2026-08-30T12:00:00.000Z".into(),
            }];
            state.exited = Some(ExitedRecord {
                code: None,
                signal: Some("SIGTERM".into()),
                at: now_iso(),
            });
            let parsed: OrchestratorState = round_trip(&state);
            assert_eq!(parsed.version, STATE_VERSION);
            assert_eq!(parsed.pending_requests.len(), 1);
            assert_eq!(parsed.pending_requests[0].request.tool_name, "Bash");

            // A newer writer's extra fields and an older writer's gaps both parse.
            let foreign: OrchestratorState =
                serde_json::from_str(r#"{"version":2,"futureField":1,"costUsd":0.5,"cwd":"/r"}"#)
                    .unwrap();
            assert_eq!(foreign.cost_usd, 0.5);
            assert_eq!(foreign.cwd, "/r");
            assert_eq!(foreign.pending_requests, Vec::new());
            // camelCase on disk: state round-trips through the wire names.
            let line = serde_json::to_string(&state).unwrap();
            assert!(line.contains(r#""sessionId":"sess""#), "{line}");
            assert!(line.contains(r#""turnActive":true"#), "{line}");
            assert!(line.contains(r#""requestId":"req_1""#), "{line}");
        }
        {
            let caps = Capabilities {
                fetched_at: now_iso(),
                tools: vec!["Read".into(), "mcp__fleet__fleet_spawn".into()],
                commands: vec![AgentCommand {
                    name: "model".into(),
                    description: Some("Set the model".into()),
                    argument_hint: Some("<model>".into()),
                    aliases: None,
                }],
                mcp_servers: vec![McpServerStatus {
                    name: "fleet".into(),
                    status: "connected".into(),
                }],
                capabilities: vec!["interrupt_receipt_v1".into()],
                models: vec!["sonnet".into()],
            };
            let parsed: Capabilities = round_trip(&caps);
            assert_eq!(parsed, caps);
            let line = serde_json::to_string(&caps).unwrap();
            assert!(line.contains(r#""fetchedAt""#), "{line}");
            assert!(line.contains(r#""mcpServers""#), "{line}");

            // Never asked: the aliases stand in, and the view reads as stale.
            let fresh = Capabilities::default();
            assert_eq!(fresh.models, ORCHESTRATOR_MODEL_ALIASES);
            assert!(fresh.is_stale(std::time::Duration::from_secs(30)));
            assert!(fresh.age().is_none());

            // A newer writer's extra field and an older writer's gaps both parse.
            let foreign: Capabilities =
                serde_json::from_str(r#"{"futureField":1,"tools":["Read"]}"#).unwrap();
            assert_eq!(foreign.tools, vec!["Read".to_string()]);
            assert!(foreign.commands.is_empty());
        }
        {
            let caps = Capabilities {
                fetched_at: now_iso(),
                ..Capabilities::default()
            };
            assert!(!caps.is_stale(std::time::Duration::from_secs(30)));
            assert!(caps.age().is_some());
        }
        {
            let make = |id: &str, at: &str| PermissionRequest {
                request_id: id.into(),
                request: CanUseToolRequest {
                    tool_name: "Bash".into(),
                    ..CanUseToolRequest::default()
                },
                received_at: at.into(),
            };
            let mut pending = HashMap::new();
            pending.insert("b".into(), make("b", "2026-08-30T12:00:01.000Z"));
            pending.insert("a".into(), make("a", "2026-08-30T12:00:00.000Z"));
            pending.insert("c".into(), make("c", "2026-08-30T12:00:00.000Z"));
            let list = sorted_pending(&pending);
            let ids: Vec<&str> = list.iter().map(|p| p.request_id.as_str()).collect();
            // Same arrival time: tie broken by request id.
            assert_eq!(ids, vec!["a", "c", "b"]);
        }
    }

    #[test]
    fn trim_keeps_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let line = |i: usize| format!("{{\"type\":\"notice\",\"text\":\"{i}\"}}");
        let body: String = (0..10).map(|i| format!("{}\n", line(i))).collect();
        std::fs::write(&path, &body).unwrap();

        assert!(
            !trim_events_file(&path, 10, 4),
            "exactly at the cap is fine"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        assert!(trim_events_file(&path, 8, 4));
        let kept = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = kept.lines().collect();
        assert_eq!(lines.len(), 4, "{kept}");
        assert_eq!(lines[0], line(6), "the tail is what survives");
        assert_eq!(lines[3], line(9));
        assert!(kept.ends_with('\n'), "still a well-framed JSONL file");

        // a file that is not there is not an error
        assert!(!trim_events_file(&tmp.path().join("missing.jsonl"), 1, 1));
        // and no temporary file is left behind
        assert!(!tmp.path().join("events.jsonl.trim").exists());
    }

    #[test]
    fn event_records() {
        {
            let events = vec![
                OrchestratorEvent::StreamText {
                    text: "hello ".into(),
                },
                OrchestratorEvent::Activity { activity: None },
                OrchestratorEvent::Activity {
                    activity: Some(Activity {
                        kind: ActivityKind::Responding,
                        label: None,
                        since: 7,
                    }),
                },
                OrchestratorEvent::PermissionRequest {
                    request_id: "req_1".into(),
                    request: json!({"subtype":"can_use_tool","tool_name":"Bash"}),
                },
                OrchestratorEvent::PermissionResolved {
                    request_id: "req_1".into(),
                    how: "allow".into(),
                },
                OrchestratorEvent::Notice {
                    text: "· hi".into(),
                    error: None,
                },
                OrchestratorEvent::Notice {
                    text: "! boom".into(),
                    error: Some(true),
                },
                OrchestratorEvent::Exit {
                    code: None,
                    signal: Some("SIGTERM".into()),
                },
                OrchestratorEvent::Exit {
                    code: Some(0),
                    signal: None,
                },
            ];
            for event in &events {
                let record = event.to_record();
                let line = serde_json::to_string(&record).unwrap();
                let parsed: EventRecord = serde_json::from_str(&line).unwrap();
                assert_eq!(&parsed.decode(), event, "line: {line}");
            }
            // Pinned wire shapes: camelCase request ids, error flag only when set.
            let line = serde_json::to_string(&events[3].to_record()).unwrap();
            assert!(
                line.contains(r#""type":"permission_request""#)
                    && line.contains(r#""requestId":"req_1""#),
                "{line}"
            );
            let line = serde_json::to_string(&events[5].to_record()).unwrap();
            assert_eq!(line, r#"{"type":"notice","text":"· hi"}"#);
        }
        {
            let claude_msg = json!({
                "type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},
                "parent_tool_use_id":null,"session_id":"s",
            });
            let event = OrchestratorEvent::Passthrough(claude_msg);
            let record = event.to_record();
            let line = serde_json::to_string(&record).unwrap();
            let parsed: EventRecord = serde_json::from_str(&line).unwrap();
            assert_eq!(parsed.decode(), event);
            assert!(line.contains(r#""parent_tool_use_id":null"#), "{line}");

            let record = EventRecord {
                kind: "brand_new".into(),
                body: Map::from_iter([("x".to_string(), json!(1))]),
            };
            match record.decode() {
                OrchestratorEvent::Passthrough(value) => {
                    assert_eq!(value, json!({"type":"brand_new","x":1}));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn coalesces_deltas() {
        let dir = std::env::temp_dir().join(format!(
            "pilotfish-transcript-{}-{}",
            std::process::id(),
            crate::util::new_id("t").replace('_', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let mut transcript = Transcript::new(path.clone());
        transcript.stream_text("hel");
        transcript.stream_text("lo");
        transcript
            .write(&OrchestratorEvent::Passthrough(json!({"type":"assistant"})))
            .unwrap();
        transcript.stream_text(" more");
        transcript.flush_text().unwrap();
        transcript.flush_text().unwrap(); // nothing pending: no record

        let lines: Vec<EventRecord> = crate::util::read_jsonl_tail(&path, 10);
        assert_eq!(lines.len(), 3, "{lines:?}");
        match lines[0].decode() {
            OrchestratorEvent::StreamText { text } => assert_eq!(text, "hello"),
            other => panic!("{other:?}"),
        }
        match lines[1].decode() {
            OrchestratorEvent::Passthrough(value) => assert_eq!(value["type"], "assistant"),
            other => panic!("{other:?}"),
        }
        match lines[2].decode() {
            OrchestratorEvent::StreamText { text } => assert_eq!(text, " more"),
            other => panic!("{other:?}"),
        }
    }
}
