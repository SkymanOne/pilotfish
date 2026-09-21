//! Routing a brief: which model runs it, how hard it should think, and
//! whether it needs a worktree of its own.
//!
//! The judgment comes from TypeSafe's System One (Jev), asked over HTTP as
//! one request with four independent questions. There is no Rust SDK, so
//! this speaks the documented API directly: `POST /v1/systemone` with
//! `{state, model, questions}`, answering `{answers: {id: …}}`.
//!
//! Code owns the workflow, which is what the model is for. The candidate
//! models come from pi's own catalogue, the thinking levels are clamped to
//! the ones the chosen model actually has, and an answer the model is not
//! confident about falls back to the configured default rather than being
//! acted on. Nothing here is required: with routing off, or no API key, the
//! caller gets `None` and spawns exactly as it always did.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::fleet::run::{RunState, WorkerModel};
use crate::paths::{RoutingConfig, env_var};

/// Where the API lives, unless `$PARL_TYPESAFE_URL` says otherwise.
const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// A judgment is on the spawn path, so it cannot hang it. The documented
/// latency is around 100 ms; past this the spawn goes ahead unrouted.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The ordered effort scale the `effort` Score is read against. Kept
/// separate from pi's [`THINKING_LEVELS`] so a model that lacks a level does
/// not distort the scale — the clamp happens after.
///
/// [`THINKING_LEVELS`]: crate::fleet::run::THINKING_LEVELS
const EFFORT_SCALE: [&str; 5] = ["off", "low", "medium", "high", "max"];

/// The questions, as their own constants: a multi-line string literal inside
/// `json!` is reindented by rustfmt, and the indent lands *in the prompt*.
const MODEL_QUESTION: &str = "Which coding model should carry out `task.brief`? \
Weigh how much reasoning, context and care the task needs against the cost of the \
model; prefer the smallest model that can finish it well.";
const EFFORT_QUESTION: &str =
    "How much reasoning does `task.brief` need before the work can be done?";
const EFFORT_CRITERIA: [&str; 5] = [
    "Mechanical: the change is fully specified and needs no judgment.",
    "Simple: one obvious approach, a handful of files.",
    "Moderate: some design judgment, or code that must be understood first.",
    "Hard: several interacting parts, or a non-obvious approach to find.",
    "Very hard: the approach itself is the problem to solve.",
];
const WORKTREE_QUESTION: &str = "Will carrying out `task.brief` change files in the \
repository? Answer no for reading, reviewing, summarising or investigating.";
const PARALLEL_QUESTION: &str = "Can `task.brief` run at the same time as everything in \
`inFlight` without them editing the same files?";

/// What a brief was routed to, recorded on the run so the decision is
/// auditable rather than mysterious.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Routing {
    /// The chosen pi model id, or none when the choice was not confident.
    pub model: Option<String>,
    pub provider: Option<String>,
    /// The chosen thinking level, already clamped to what the model has.
    pub thinking: Option<String>,
    /// Whether the brief was judged to need its own worktree.
    pub worktree: Option<bool>,
    /// Whether it was judged safe to run alongside what is already in flight.
    pub parallel_safe: Option<bool>,
    /// The model's confidence in the model choice, 0..1.
    pub confidence: f64,
    /// Why the decision came out the way it did, in one line.
    pub note: String,
}

/// Everything the judgment is made from: the brief and what is already
/// happening in the repository.
#[derive(Debug, Clone)]
pub struct Brief<'a> {
    pub name: &'a str,
    pub brief: &'a str,
    pub repo_root: &'a Path,
    /// What the orchestrator already has running, so concurrency can be
    /// judged rather than assumed.
    pub in_flight: &'a [RunState],
    /// The models pi has configured. An empty catalogue means there is
    /// nothing to choose between, and routing declines.
    pub candidates: &'a [WorkerModel],
}

/// The API key, from `$PARL_TYPESAFE_API_KEY` or `$TYPESAFE_API_KEY`.
/// Neither set means routing is off, however the config reads.
#[must_use]
pub fn api_key() -> Option<String> {
    api_key_from(
        std::env::var(env_var("TYPESAFE_API_KEY")).ok().as_deref(),
        std::env::var("TYPESAFE_API_KEY").ok().as_deref(),
    )
}

/// [`api_key`] with both values injected, so tests never read the ambient
/// environment.
#[must_use]
pub fn api_key_from(ours: Option<&str>, theirs: Option<&str>) -> Option<String> {
    ours.or(theirs)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToString::to_string)
}

/// The endpoint to call: `$PARL_TYPESAFE_URL`, else the config's, else the
/// documented one. The override is what the tests point at a local stub.
#[must_use]
fn endpoint(config: &RoutingConfig) -> String {
    std::env::var(env_var("TYPESAFE_URL"))
        .ok()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .or_else(|| config.endpoint.clone())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
}

/// Route one brief, or `None` when routing is off, unkeyed, or has nothing
/// to choose between.
///
/// Never an error: a judgment that cannot be obtained is a judgment not
/// made, and the caller falls back to the configured default. The reason is
/// carried on [`Routing::note`] for anything that *was* decided.
pub async fn route(
    brief: &Brief<'_>,
    config: &RoutingConfig,
    key: Option<&str>,
) -> Option<Routing> {
    if !config.enabled {
        return None;
    }
    let key = key?;
    if brief.candidates.is_empty() {
        return None;
    }
    let body = request_body(brief, config);
    let answers = post(&endpoint(config), key, &body).await?;
    Some(decide(brief, config, &answers))
}

/// The request body: the state to judge against, and four questions that do
/// not depend on one another, so they answer in one round trip.
fn request_body(brief: &Brief<'_>, config: &RoutingConfig) -> Value {
    let in_flight: Vec<Value> = brief
        .in_flight
        .iter()
        .map(|run| {
            json!({
                "name": run.name,
                "brief": crate::util::first_line(&run.task_brief),
                "branch": run.branch,
            })
        })
        .collect();
    let criteria: serde_json::Map<String, Value> = brief
        .candidates
        .iter()
        .map(|m| {
            let label = m.name.clone().unwrap_or_else(|| m.id.clone());
            (
                m.id.clone(),
                Value::String(format!("{label}, served by {}", m.provider)),
            )
        })
        .collect();
    json!({
        "model": config.model,
        "state": {
            "task": {"name": brief.name, "brief": brief.brief},
            "repository": brief.repo_root.to_string_lossy(),
            "inFlight": in_flight,
        },
        "questions": {
            "model": {
                "type": "choice",
                "instructions": MODEL_QUESTION,
                "criteria": criteria,
            },
            "effort": {
                "type": "score",
                "instructions": EFFORT_QUESTION,
                "criteria": EFFORT_CRITERIA,
            },
            "needs_worktree": {"type": "noul", "instructions": WORKTREE_QUESTION},
            "parallel_safe": {"type": "noul", "instructions": PARALLEL_QUESTION},
        },
    })
}

/// One POST. Any failure — no network, a refused key, a body that does not
/// parse — is `None`: the spawn is not the place to surface it.
async fn post(url: &str, key: &str, body: &Value) -> Option<Answers> {
    let client = reqwest::Client::builder().timeout(TIMEOUT).build().ok()?;
    let response = client
        .post(url)
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Answers>().await.ok()
}

/// The response, as much of it as this module reads.
#[derive(Debug, Clone, Default, Deserialize)]
struct Answers {
    #[serde(default)]
    answers: std::collections::HashMap<String, Answer>,
}

/// One answer. The three primitives share a shape loosely enough that one
/// tolerant struct reads all of them, which is also what keeps a newer
/// field from breaking an older reader.
#[derive(Debug, Clone, Default, Deserialize)]
struct Answer {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    noul: Option<f64>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    confidence: Option<f64>,
}

/// Turn the answers into a decision. This is where policy lives: the model
/// supplies judgments, code decides what to do with them.
fn decide(brief: &Brief<'_>, config: &RoutingConfig, answers: &Answers) -> Routing {
    let mut routing = Routing::default();
    let threshold = config.confidence_threshold();

    let model = answers.answers.get("model");
    let confidence = model.and_then(|a| a.confidence).unwrap_or(0.0);
    routing.confidence = confidence;
    let chosen = model
        .and_then(|a| a.choice.as_deref())
        .and_then(|id| brief.candidates.iter().find(|m| m.id == id));
    match (chosen, confidence >= threshold) {
        (Some(model), true) => {
            routing.model = Some(model.id.clone());
            routing.provider = (!model.provider.is_empty()).then(|| model.provider.clone());
            routing.note = format!("jev chose {} ({confidence:.2})", model.id);
        }
        (Some(model), false) => {
            routing.note = format!(
                "jev leaned to {} but only at {confidence:.2} (threshold {threshold:.2}); \
kept the configured default",
                model.id
            );
        }
        (None, _) => {
            routing.note = "jev named no model this fleet has; kept the configured default".into();
        }
    }

    // The effort Score is answered on its own scale, then clamped to the
    // levels the chosen model actually has: pi accepts a level its model
    // lacks and then silently ignores it (AGENTS.md, verified fact 5), so a
    // level it cannot honour is worse than none at all.
    if let Some(score) = answers.answers.get("effort").and_then(|a| a.score) {
        let at = (score.round().max(0.0) as usize).min(EFFORT_SCALE.len() - 1);
        routing.thinking = Some(EFFORT_SCALE[at].to_string());
    }

    routing.worktree = answers
        .answers
        .get("needs_worktree")
        .and_then(|a| a.noul)
        .map(|p| p >= 0.5);
    routing.parallel_safe = answers
        .answers
        .get("parallel_safe")
        .and_then(|a| a.noul)
        .map(|p| p >= 0.5);
    routing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<WorkerModel> {
        vec![
            WorkerModel {
                provider: "anthropic".into(),
                id: "claude-opus-5".into(),
                name: Some("Opus 5".into()),
            },
            WorkerModel {
                provider: "openrouter".into(),
                id: "deepseek-v4-flash".into(),
                name: None,
            },
        ]
    }

    fn brief<'a>(models: &'a [WorkerModel], in_flight: &'a [RunState]) -> Brief<'a> {
        Brief {
            name: "add-auth",
            brief: "add token refresh",
            repo_root: Path::new("/repo"),
            in_flight,
            candidates: models,
        }
    }

    fn answers(json: Value) -> Answers {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn the_request_offers_every_model_and_names_what_is_in_flight() {
        let models = candidates();
        let mut running = RunState {
            name: "docs".into(),
            task_brief: "rewrite the readme\nsecond line".into(),
            ..RunState::default()
        };
        running.branch = Some("parl/docs-1234567".into());
        let in_flight = vec![running];
        let body = request_body(&brief(&models, &in_flight), &RoutingConfig::default());

        let criteria = &body["questions"]["model"]["criteria"];
        assert!(
            criteria["claude-opus-5"]
                .as_str()
                .unwrap()
                .contains("Opus 5")
        );
        assert!(
            criteria["deepseek-v4-flash"]
                .as_str()
                .unwrap()
                .contains("openrouter"),
            "a model with no display name still names its provider: {criteria}"
        );
        assert_eq!(body["state"]["inFlight"][0]["name"], "docs");
        assert_eq!(
            body["state"]["inFlight"][0]["brief"], "rewrite the readme",
            "one line of another worker's brief is context enough"
        );
        assert_eq!(body["model"], "jev-latest");
        // the four questions are independent, so they ride in one request
        let questions = body["questions"].as_object().unwrap();
        assert_eq!(questions.len(), 4, "{questions:?}");
    }

    #[test]
    fn a_confident_choice_is_taken_with_its_provider() {
        let models = candidates();
        let decision = decide(
            &brief(&models, &[]),
            &RoutingConfig::default(),
            &answers(json!({"answers": {
                "model": {"type": "choice", "choice": "claude-opus-5", "confidence": 0.9},
                "effort": {"type": "score", "score": 3.0},
                "needs_worktree": {"type": "noul", "noul": 0.95},
                "parallel_safe": {"type": "noul", "noul": 0.1},
            }})),
        );
        assert_eq!(decision.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(decision.provider.as_deref(), Some("anthropic"));
        assert_eq!(decision.thinking.as_deref(), Some("high"));
        assert_eq!(decision.worktree, Some(true));
        assert_eq!(decision.parallel_safe, Some(false));
        assert!(decision.note.contains("claude-opus-5"), "{}", decision.note);
    }

    #[test]
    fn an_unconfident_choice_keeps_the_configured_default() {
        let models = candidates();
        let decision = decide(
            &brief(&models, &[]),
            &RoutingConfig::default(),
            &answers(json!({"answers": {
                "model": {"type": "choice", "choice": "claude-opus-5", "confidence": 0.2},
                "effort": {"type": "score", "score": 0.0},
            }})),
        );
        assert_eq!(decision.model, None, "the fallback is the configured model");
        assert_eq!(decision.thinking.as_deref(), Some("off"));
        assert!(decision.note.contains("threshold"), "{}", decision.note);
        // an unanswered noul decides nothing either way
        assert_eq!(decision.worktree, None);
    }

    #[test]
    fn a_model_this_fleet_does_not_have_is_refused() {
        let models = candidates();
        let decision = decide(
            &brief(&models, &[]),
            &RoutingConfig::default(),
            &answers(json!({"answers": {
                "model": {"type": "choice", "choice": "gpt-9", "confidence": 1.0},
            }})),
        );
        assert_eq!(decision.model, None);
        assert!(decision.note.contains("no model this fleet has"));
    }

    #[test]
    fn the_effort_score_maps_onto_the_scale_and_cannot_run_off_it() {
        let models = candidates();
        for (score, level) in [
            (-4.0, "off"),
            (0.0, "off"),
            (1.4, "low"),
            (2.0, "medium"),
            (4.0, "max"),
            (99.0, "max"),
        ] {
            let decision = decide(
                &brief(&models, &[]),
                &RoutingConfig::default(),
                &answers(json!({"answers": {"effort": {"type": "score", "score": score}}})),
            );
            assert_eq!(decision.thinking.as_deref(), Some(level), "score {score}");
        }
    }

    #[test]
    fn a_key_comes_from_either_variable_and_blank_is_no_key() {
        assert_eq!(
            api_key_from(Some("ours"), Some("theirs")).as_deref(),
            Some("ours")
        );
        assert_eq!(
            api_key_from(None, Some("theirs")).as_deref(),
            Some("theirs")
        );
        assert_eq!(api_key_from(Some("  "), None), None);
        assert_eq!(api_key_from(None, None), None);
    }

    /// A one-request HTTP stub: reads the request, hands the body back over
    /// a channel, and answers with `reply`. Enough to prove the wire
    /// contract without a mock-server dependency.
    async fn stub(reply: Value) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            // read until the body is in: the request is small and sent at once
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .or_else(|| {
                            head.lines()
                                .find_map(|l| l.strip_prefix("Content-Length: "))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        let _ = tx.send((head.to_string(), body.to_string()));
                        break;
                    }
                }
            }
            let body = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        (url, rx)
    }

    #[tokio::test]
    async fn a_round_trip_sends_the_key_and_reads_the_decision_back() {
        let (url, rx) = stub(json!({"answers": {
            "model": {"type": "choice", "choice": "deepseek-v4-flash", "confidence": 0.82},
            "effort": {"type": "score", "score": 1.0},
            "needs_worktree": {"type": "noul", "noul": 0.9},
            "parallel_safe": {"type": "noul", "noul": 0.8},
        }}))
        .await;
        let models = candidates();
        let config = RoutingConfig {
            enabled: true,
            endpoint: Some(url),
            ..RoutingConfig::default()
        };
        let decision = route(&brief(&models, &[]), &config, Some("sk-test"))
            .await
            .expect("a decision");
        assert_eq!(decision.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(decision.provider.as_deref(), Some("openrouter"));
        assert_eq!(decision.thinking.as_deref(), Some("low"));
        assert_eq!(decision.worktree, Some(true));

        let (head, body) = rx.await.unwrap();
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-test"),
            "the key rides as a bearer token: {head}"
        );
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["state"]["task"]["name"], "add-auth");
        assert!(sent["questions"]["model"]["criteria"]["claude-opus-5"].is_string());
    }

    #[tokio::test]
    async fn an_endpoint_that_is_not_there_is_no_decision_rather_than_an_error() {
        let models = candidates();
        let config = RoutingConfig {
            enabled: true,
            // nothing listening: a spawn must not fail because a judgment could not be had
            endpoint: Some("http://127.0.0.1:1/v1/systemone".to_string()),
            ..RoutingConfig::default()
        };
        assert!(
            route(&brief(&models, &[]), &config, Some("sk-test"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn routing_declines_when_it_is_off_unkeyed_or_has_no_catalogue() {
        let models = candidates();
        let on = RoutingConfig {
            enabled: true,
            ..RoutingConfig::default()
        };
        assert!(
            route(&brief(&models, &[]), &RoutingConfig::default(), Some("k"))
                .await
                .is_none(),
            "off by default"
        );
        assert!(
            route(&brief(&models, &[]), &on, None).await.is_none(),
            "no key, no judgment"
        );
        assert!(
            route(&brief(&[], &[]), &on, Some("k")).await.is_none(),
            "nothing to choose between"
        );
    }
}
