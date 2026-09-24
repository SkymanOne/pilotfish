//! The mailbox envelope: every line of `inbox.jsonl` and `outbox.jsonl`.
//!
//! Contract (pinned, byte-for-byte; the pi-side TypeScript extension matches
//! it):
//!
//! ```json
//! {"id":"m_ab12cd","ts":"2026-08-30T12:00:00.000Z","from":"orchestrator","to":"worker:<uuid>","type":"steer","payload":{"message":"..."}}
//! ```
//!
//! `from`/`to` are [`Party`] values. `id` is `m_` plus a short random suffix
//! everywhere (inbox included), `ts` is RFC3339 UTC with milliseconds.
//!
//! **inbox** (orchestrator/console -> worker monitor), `to` is
//! `worker:<uuid>` (a legacy `worker:<runId>` parses too), `from` is
//! `orchestrator` / `orchestrator:<uuid>` or `console`:
//! `steer`/`follow_up`/`command`/`thinking` carry `{"message"}`, `abort`
//! carries `{}`, `answer` carries `{"message","questionId"}`, `model`
//! carries `{"message","provider"}` (`provider` null to resolve from the
//! models pi has configured).
//!
//! **outbox** (worker -> monitor), `from` is `worker:<uuid>` (legacy
//! `worker:<runId>` parses too), `to` is `fleet`: `question`,
//! `progress`, `question_resolved`.
//!
//! The old flat `control.jsonl` shape is gone: no back-compat reader, no
//! migration. Deserialisation tolerates unknown `type` values and unknown
//! payload fields — such lines parse into an [`Envelope`] but [`Envelope::decode`]
//! returns `None`, and mailbox readers skip them, so a newer writer cannot
//! crash an older reader.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::util::{append_json_line, new_id, now_iso};

/// The session a bare `"orchestrator"` party refers to: the pre-identity
/// fleet, and the provenance CLI- and MCP-driven steering carries until
/// sessions wire themselves through. Its canonical on-wire spelling is the
/// bare `"orchestrator"`, so files written before session identities keep
/// round-tripping byte-for-byte.
pub const DEFAULT_ORCHESTRATOR_SESSION: Uuid = Uuid::nil();

/// Namespace for deriving the stable party uuid of a legacy run id (a
/// `worker:<payload>` whose payload predates run uuids). Derived uuid, not
/// random: the same run id must parse to the same party forever.
const LEGACY_RUN_NAMESPACE: Uuid = Uuid::from_u128(0x7061_726c_2d6c_6567_6163_792d_7275_6e73);

/// The stable [`Party::Worker`] identity of a legacy run id (`"abc-1"` in
/// `worker:abc-1`, or a run whose state file predates run uuids).
#[must_use]
pub fn legacy_worker_uuid(run_id: &str) -> Uuid {
    Uuid::new_v5(&LEGACY_RUN_NAMESPACE, run_id.as_bytes())
}

/// Who a mailbox line is from or to.
///
/// Parsing accepts, permanently: `"orchestrator"` (the default session),
/// `"orchestrator:<uuid>"`, `"worker:<uuid>"`, and `"worker:<anything-not-a-uuid>"`
/// (a legacy run id, encoded as a derived uuid). `Console` and `Fleet` are
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Party {
    Orchestrator(Uuid),
    Console,
    Fleet,
    Worker(Uuid),
}

impl Party {
    /// The `worker:<uuid>` party of a run.
    #[must_use]
    pub fn worker(uuid: Uuid) -> Self {
        Self::Worker(uuid)
    }
}

impl std::fmt::Display for Party {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The default session's canonical spelling is the bare
            // `"orchestrator"` (its own round-trips), so legacy provenance
            // and event labels written before identities stay byte-identical.
            Self::Orchestrator(uuid) if *uuid == DEFAULT_ORCHESTRATOR_SESSION => {
                f.write_str("orchestrator")
            }
            Self::Orchestrator(uuid) => write!(f, "orchestrator:{uuid}"),
            Self::Console => f.write_str("console"),
            Self::Fleet => f.write_str("fleet"),
            Self::Worker(uuid) => write!(f, "worker:{uuid}"),
        }
    }
}

impl std::str::FromStr for Party {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "orchestrator" => Ok(Self::Orchestrator(DEFAULT_ORCHESTRATOR_SESSION)),
            "console" => Ok(Self::Console),
            "fleet" => Ok(Self::Fleet),
            other => {
                if let Some(rest) = other.strip_prefix("orchestrator:") {
                    return Uuid::parse_str(rest)
                        .map(Self::Orchestrator)
                        .map_err(|_| format!("not a party: {other}"));
                }
                if let Some(rest) = other.strip_prefix("worker:") {
                    return if rest.is_empty() {
                        Err(format!("not a party: {other}"))
                    } else {
                        Ok(Self::Worker(
                            Uuid::parse_str(rest).unwrap_or_else(|_| legacy_worker_uuid(rest)),
                        ))
                    };
                }
                Err(format!("not a party: {other}"))
            }
        }
    }
}

impl Serialize for Party {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Party {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// One mailbox line, on the wire in both `inbox.jsonl` and `outbox.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub ts: String,
    pub from: Party,
    pub to: Party,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

impl Envelope {
    /// A fresh envelope: id and ts are filled in (`m_` prefix).
    #[must_use]
    pub fn new(from: Party, to: Party, kind: &str, payload: Value) -> Self {
        Self {
            id: new_id("m"),
            ts: now_iso(),
            from,
            to,
            kind: kind.to_string(),
            payload,
        }
    }

    /// Parse one JSONL line into the envelope shell. Line-level shape errors
    /// (missing id/ts/from/to, malformed party) yield `None` so callers skip
    /// the line; unknown `type` values still parse and are filtered by
    /// [`Envelope::decode`].
    #[must_use]
    pub fn parse_line(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }

    /// Typed view of the payload for known types. `None` for unknown types or
    /// a payload that does not fit the type — readers skip those lines.
    #[must_use]
    pub fn decode(&self) -> Option<Decoded<'_>> {
        fn message(payload: &Value) -> Option<&str> {
            payload.get("message").and_then(Value::as_str)
        }
        match self.kind.as_str() {
            "steer" => message(&self.payload).map(Decoded::Steer),
            "follow_up" => message(&self.payload).map(Decoded::FollowUp),
            "command" => message(&self.payload).map(Decoded::Command),
            "thinking" => message(&self.payload).map(Decoded::Thinking),
            "abort" => Some(Decoded::Abort),
            "refresh_capabilities" => Some(Decoded::RefreshCapabilities),
            "answer" => Some(Decoded::Answer {
                message: self.payload.get("message").and_then(Value::as_str),
                question_id: self.payload.get("questionId").and_then(Value::as_str),
            }),
            "model" => Some(Decoded::Model {
                model_id: message(&self.payload)?,
                provider: self.payload.get("provider").and_then(Value::as_str),
            }),
            "question" => Some(Decoded::Question(QuestionPayload {
                question: self
                    .payload
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                options: self
                    .payload
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    }),
                context: self
                    .payload
                    .get("context")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })),
            "progress" => message(&self.payload).map(|m| Decoded::Progress(m.to_string())),
            "question_resolved" => Some(Decoded::QuestionResolved {
                question_id: self
                    .payload
                    .get("questionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                how: match self.payload.get("how").and_then(Value::as_str) {
                    Some("answered") => Resolution::Answered,
                    Some("timeout") => Resolution::Timeout,
                    Some("aborted") => Resolution::Aborted,
                    _ => return None,
                },
            }),
            _ => None,
        }
    }
}

/// A decoded, known payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded<'a> {
    Steer(&'a str),
    FollowUp(&'a str),
    Command(&'a str),
    Thinking(&'a str),
    Abort,
    /// Ask pi what it offers now and rewrite the fleet's pi catalogue.
    RefreshCapabilities,
    Answer {
        message: Option<&'a str>,
        question_id: Option<&'a str>,
    },
    /// Switch the worker's model; `model_id` is the pi model id.
    Model {
        model_id: &'a str,
        provider: Option<&'a str>,
    },
    Question(QuestionPayload),
    Progress(String),
    QuestionResolved {
        question_id: String,
        how: Resolution,
    },
}

/// A `question` payload: what the worker is asking and what it offered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionPayload {
    pub question: String,
    pub options: Option<Vec<String>>,
    pub context: Option<String>,
}

/// How a pending question ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Answered,
    Timeout,
    Aborted,
}

/// Builder helpers for the seven inbox types, with the payload shapes the
/// contract pins.
impl Envelope {
    /// `steer` — delivered after the worker's current tool call.
    pub fn steer(from: Party, to: Party, message: impl Into<String>) -> Self {
        Self::control(from, to, "steer", Some(message.into()), None)
    }

    /// `follow_up` — queued for after the worker finishes its current work.
    pub fn follow_up(from: Party, to: Party, message: impl Into<String>) -> Self {
        Self::control(from, to, "follow_up", Some(message.into()), None)
    }

    /// `command` — a slash command for pi to expand (skills, prompt
    /// templates, extension commands).
    pub fn command(from: Party, to: Party, message: impl Into<String>) -> Self {
        Self::control(from, to, "command", Some(message.into()), None)
    }

    /// `thinking` — change the worker's reasoning level (`"max"` etc).
    pub fn thinking(from: Party, to: Party, message: impl Into<String>) -> Self {
        Self::control(from, to, "thinking", Some(message.into()), None)
    }

    /// `abort` — payload is exactly `{}`.
    pub fn abort(from: Party, to: Party) -> Self {
        Self::control(from, to, "abort", None, None)
    }

    /// `refresh_capabilities` — payload is exactly `{}`. The monitor asks pi
    /// for its models and commands again and rewrites the fleet catalogue.
    pub fn refresh_capabilities(from: Party, to: Party) -> Self {
        Self::control(from, to, "refresh_capabilities", None, None)
    }

    /// `answer` — resolve the question the worker is blocked on.
    pub fn answer(
        from: Party,
        to: Party,
        message: impl Into<String>,
        question_id: Option<String>,
    ) -> Self {
        Self::control(from, to, "answer", Some(message.into()), question_id)
    }

    /// `model` — switch the running worker's model. A `provider` of `None`
    /// serializes as null: the monitor resolves it from pi's model list.
    pub fn model(
        from: Party,
        to: Party,
        model_id: impl Into<String>,
        provider: Option<String>,
    ) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("message".into(), Value::String(model_id.into()));
        payload.insert(
            "provider".into(),
            provider.map_or(Value::Null, Value::String),
        );
        Self::new(from, to, "model", Value::Object(payload))
    }

    fn control(
        from: Party,
        to: Party,
        kind: &str,
        message: Option<String>,
        question_id: Option<String>,
    ) -> Self {
        let mut payload = serde_json::Map::new();
        if let Some(m) = message {
            payload.insert("message".into(), Value::String(m));
        }
        if let Some(q) = question_id {
            payload.insert("questionId".into(), Value::String(q));
        }
        Self::new(from, to, kind, Value::Object(payload))
    }
}

/// Builder helpers for the three outbox types.
impl Envelope {
    /// `question` — the worker blocks on `fleet_ask` until an answer lands.
    pub fn question(from: Party, payload: QuestionPayload) -> Self {
        Self::new(
            from,
            Party::Fleet,
            "question",
            serde_json::to_value(payload)
                .unwrap_or_else(|_| Value::Object(serde_json::Map::default())),
        )
    }

    /// `progress` — a one-line `fleet_progress` milestone.
    pub fn progress(from: Party, message: impl Into<String>) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("message".into(), Value::String(message.into()));
        Self::new(from, Party::Fleet, "progress", Value::Object(payload))
    }

    /// `question_resolved` — the worker stopped waiting (answered, timed out
    /// or aborted); the asker should stop waiting too.
    pub fn question_resolved(from: Party, question_id: impl Into<String>, how: Resolution) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("questionId".into(), Value::String(question_id.into()));
        payload.insert(
            "how".into(),
            Value::String(
                serde_json::to_value(how)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ),
        );
        Self::new(
            from,
            Party::Fleet,
            "question_resolved",
            Value::Object(payload),
        )
    }
}

/// Append an envelope as one JSONL line (creating the file when missing).
///
/// # Errors
///
/// Returns `std::io::Error` when serialization or appending fails.
pub fn append_envelope(path: &std::path::Path, envelope: &Envelope) -> std::io::Result<()> {
    append_json_line(path, envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn worker_uuid() -> Uuid {
        Uuid::parse_str("9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c").unwrap()
    }

    fn sess_uuid() -> Uuid {
        Uuid::parse_str("6e1c9a86-3b7d-4f5a-9e2c-1b8d4a7f0c3e").unwrap()
    }

    fn round_trip(envelope: &Envelope) -> Envelope {
        let line = serde_json::to_string(envelope).unwrap();
        let parsed = Envelope::parse_line(&line).unwrap();
        assert_eq!(*envelope, parsed, "line: {line}");
        parsed
    }

    #[test]
    fn pinned_wire_shape() {
        let mut env = Envelope::new(
            Party::Orchestrator(DEFAULT_ORCHESTRATOR_SESSION),
            Party::worker(worker_uuid()),
            "steer",
            json!({"message": "hi"}),
        );
        env.id = "m_ab12cd".into();
        env.ts = "2026-08-30T12:00:00.000Z".into();
        let line = serde_json::to_string(&env).unwrap();
        assert_eq!(
            line,
            r#"{"id":"m_ab12cd","ts":"2026-08-30T12:00:00.000Z","from":"orchestrator","to":"worker:9ff7d0c4-4f2a-4b1e-8a3c-2d5e6f7a8b9c","type":"steer","payload":{"message":"hi"}}"#
        );
    }

    #[test]
    fn every_variant_round_trips() {
        {
            let from = Party::Orchestrator(DEFAULT_ORCHESTRATOR_SESSION);
            let to = Party::worker(worker_uuid());
            let cases: Vec<Envelope> = vec![
                Envelope::steer(from.clone(), to.clone(), "use tabs"),
                Envelope::follow_up(from.clone(), to.clone(), "after this, run fmt"),
                Envelope::command(from.clone(), to.clone(), "/skill:some-skill extra"),
                Envelope::thinking(from.clone(), to.clone(), "max"),
                Envelope::abort(from.clone(), to.clone()),
                Envelope::answer(Party::Console, to.clone(), "argon2", Some("m_q1".into())),
                Envelope::answer(Party::Console, to.clone(), "go with option a", None),
                Envelope::model(from.clone(), to, "claude-fable-5", Some("anthropic".into())),
                Envelope::model(from, Party::worker(worker_uuid()), "glm-5.3", None),
            ];
            for env in &cases {
                let parsed = round_trip(env);
                assert!(parsed.decode().is_some(), "{} did not decode", parsed.kind);
            }
            let steer = round_trip(&cases[0]);
            assert_eq!(steer.decode(), Some(Decoded::Steer("use tabs")));
            let abort = round_trip(&cases[4]);
            assert_eq!(abort.decode(), Some(Decoded::Abort));
            // `abort` on the wire carries an empty payload object.
            let abort_line = serde_json::to_string(&cases[4]).unwrap();
            assert!(
                abort_line.contains(r#""type":"abort","payload":{}"#),
                "{abort_line}"
            );
            let answer = round_trip(&cases[5]);
            assert_eq!(
                answer.decode(),
                Some(Decoded::Answer {
                    message: Some("argon2"),
                    question_id: Some("m_q1")
                })
            );
            let answer_no_id = round_trip(&cases[6]);
            assert_eq!(
                answer_no_id.decode(),
                Some(Decoded::Answer {
                    message: Some("go with option a"),
                    question_id: None
                })
            );
            let model = round_trip(&cases[7]);
            assert_eq!(
                model.decode(),
                Some(Decoded::Model {
                    model_id: "claude-fable-5",
                    provider: Some("anthropic")
                })
            );
            // A null provider serializes as null and decodes to None.
            let model_line = serde_json::to_string(&cases[8]).unwrap();
            assert!(model_line.contains(r#""provider":null"#), "{model_line}");
            let model_null = round_trip(&cases[8]);
            assert_eq!(
                model_null.decode(),
                Some(Decoded::Model {
                    model_id: "glm-5.3",
                    provider: None
                })
            );
        }
        {
            let from = Party::worker(worker_uuid());
            let question = QuestionPayload {
                question: "which fixture style?".to_string(),
                options: Some(vec!["a".into(), "b".into()]),
                context: Some("tests/helpers.ts".into()),
            };
            let env = Envelope::question(from.clone(), question.clone());
            assert_eq!(env.to, Party::Fleet);
            let parsed = round_trip(&env);
            assert_eq!(parsed.decode(), Some(Decoded::Question(question)));

            let env = Envelope::question(
                from.clone(),
                QuestionPayload {
                    question: "proceed?".into(),
                    options: None,
                    context: None,
                },
            );
            let parsed = round_trip(&env);
            match parsed.decode() {
                Some(Decoded::Question(q)) => {
                    assert_eq!(q.options, None);
                    assert_eq!(q.context, None);
                }
                other => panic!("{other:?}"),
            }

            let progress = Envelope::progress(from.clone(), "running tests");
            let parsed = round_trip(&progress);
            assert_eq!(
                parsed.decode(),
                Some(Decoded::Progress("running tests".into()))
            );

            for (how, expect) in [
                (Resolution::Answered, "answered"),
                (Resolution::Timeout, "timeout"),
                (Resolution::Aborted, "aborted"),
            ] {
                let env = Envelope::question_resolved(from.clone(), "m_q1", how);
                let line = serde_json::to_string(&env).unwrap();
                assert!(line.contains(&format!(r#""how":"{expect}""#)), "{line}");
                let parsed = round_trip(&env);
                assert_eq!(
                    parsed.decode(),
                    Some(Decoded::QuestionResolved {
                        question_id: "m_q1".into(),
                        how
                    })
                );
            }
        }
    }

    #[test]
    fn tolerant_reader() {
        {
            let line = r#"{"id":"m_x","ts":"2026-08-30T12:00:00.000Z","from":"orchestrator","to":"worker:r-1","type":"brand_new_kind","payload":{"whatever":1}}"#;
            let env = Envelope::parse_line(line).expect("a well-shaped envelope of unknown type");
            assert_eq!(env.kind, "brand_new_kind");
            assert_eq!(env.decode(), None);
        }
        {
            let line = r#"{"id":"m_x","ts":"t","from":"console","to":"worker:r-1","type":"steer","payload":{"message":"hi","futureField":[1,2]}}"#;
            let env = Envelope::parse_line(line).unwrap();
            assert_eq!(env.decode(), Some(Decoded::Steer("hi")));
        }
        {
            assert!(Envelope::parse_line("not json").is_none());
            assert!(Envelope::parse_line(r#"{"id":"m_1"}"#).is_none());
            assert!(Envelope::parse_line(
                r#"{"id":"m_1","ts":"t","from":"stranger","to":"worker:r","type":"steer","payload":{}}"#
            )
            .is_none());
            assert!(Envelope::parse_line(
                r#"{"id":"m_1","ts":"t","from":"worker:","to":"worker:r","type":"steer","payload":{}}"#
            )
            .is_none());
            // Known type with a payload of the wrong shape: parses, decodes to None.
            let env = Envelope::parse_line(
                r#"{"id":"m_1","ts":"t","from":"console","to":"worker:r","type":"steer","payload":{"oops":1}}"#,
            )
            .unwrap();
            assert_eq!(env.decode(), None);
            // `model` without a message is skipped like any other malformed payload.
            let env = Envelope::parse_line(
                r#"{"id":"m_1","ts":"t","from":"console","to":"worker:r","type":"model","payload":{"provider":"p"}}"#,
            )
            .unwrap();
            assert_eq!(env.decode(), None);
        }
    }

    #[test]
    fn party_forms() {
        {
            for s in ["orchestrator", "console", "fleet"] {
                let party: Party = s.parse().unwrap();
                assert_eq!(party.to_string(), s);
            }
            // The default session's canonical spelling is the bare form.
            assert_eq!(
                Party::Orchestrator(DEFAULT_ORCHESTRATOR_SESSION).to_string(),
                "orchestrator"
            );
            assert!("bogus".parse::<Party>().is_err());
            assert_eq!(Party::worker(worker_uuid()), Party::Worker(worker_uuid()));
        }
        {
            // bare orchestrator — the default session
            assert_eq!(
                "orchestrator".parse::<Party>().unwrap(),
                Party::Orchestrator(DEFAULT_ORCHESTRATOR_SESSION)
            );
            // orchestrator:<uuid> — that session
            assert_eq!(
                format!("orchestrator:{}", sess_uuid())
                    .parse::<Party>()
                    .unwrap(),
                Party::Orchestrator(sess_uuid())
            );
            // worker:<uuid> — that worker
            assert_eq!(
                format!("worker:{}", worker_uuid())
                    .parse::<Party>()
                    .unwrap(),
                Party::Worker(worker_uuid())
            );
            // worker:<anything not a uuid> — a legacy run id, mapped to a
            // stable derived uuid (same id, same party; different ids differ)
            let legacy: Party = "worker:auth-20260828141530".parse().unwrap();
            assert_eq!(
                legacy,
                Party::Worker(legacy_worker_uuid("auth-20260828141530"))
            );
            assert_eq!(
                "worker:auth-20260828141530".parse::<Party>().unwrap(),
                legacy
            );
            assert_eq!(
                legacy.to_string(),
                format!("worker:{}", legacy_worker_uuid("auth-20260828141530"))
            );
            assert_ne!(
                legacy_worker_uuid("auth-20260828141530"),
                legacy_worker_uuid("other-20990101000000")
            );
            // An unparseable orchestrator payload is not a party (the line is
            // skipped by readers, exactly like the other malformed forms).
            assert!("orchestrator:nope".parse::<Party>().is_err());
            assert!("worker:".parse::<Party>().is_err());
        }
        {
            for uuid in [worker_uuid(), sess_uuid()] {
                let orchestrator = format!("orchestrator:{uuid}");
                assert_eq!(
                    orchestrator.parse::<Party>().unwrap().to_string(),
                    orchestrator
                );
                let worker = format!("worker:{uuid}");
                assert_eq!(worker.parse::<Party>().unwrap().to_string(), worker);
            }
        }
    }
}
