//! Routing a brief: which model runs it, how hard it should think, and
//! whether it needs a worktree of its own.
//!
//! The judgment comes from TypeSafe's System One (Jev), asked over HTTP as
//! one request with four independent questions. There is no Rust SDK, so
//! this speaks the documented API directly: `POST /v1/systemone` with
//! `{state, model, questions}`, answering `{answers: {id: …}}`.
//!
//! Code owns the workflow, which is what the model is for. The candidates
//! come from pi's own catalogue, narrowed to what the user allowed; each is
//! named `provider:id`, because most ids are served by several providers
//! and they do not behave alike (verified fact 4). The thinking level is
//! clamped to the levels the model that will actually run has, a worktree is
//! only dropped on a confident "read-only", and a model choice the judgment
//! is not sure of falls back to the configured default. Nothing here is
//! required: with routing off, or no API key, the caller gets `None` and
//! spawns exactly as it always did.

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

/// The most options one Choice question takes (TypeSafe's documented limit).
/// pi's catalogue can run to hundreds; past this, routing declines and says
/// how to narrow it, rather than sending a question that cannot be asked.
const MAX_CHOICES: usize = 255;

/// A worktree is only dropped when the brief is this surely read-only. Being
/// wrong the other way puts a worker's edits straight into the human's
/// checkout, with no branch to diff or merge, so a coin flip keeps it.
const READ_ONLY_BELOW: f64 = 0.1;

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
    /// pi's whole catalogue; [`candidates`] narrows it to what routing may
    /// choose between.
    pub catalogue: &'a [WorkerModel],
    /// The provider the spawn is pinned to — an explicit `--provider` or the
    /// configured `[worker] provider`. Only that provider's models are
    /// candidates, so a choice can never contradict it.
    pub provider: Option<&'a str>,
    /// The model that runs when routing does not pick one: the caller's
    /// pinned model or the configured default. A thinking level is clamped to
    /// *its* levels then, not to the unchosen model's.
    pub fallback_model: Option<&'a str>,
}

/// pi's catalogue narrowed to one provider, when one is pinned, and to an
/// allowlist of `provider:id` or bare-id entries, when one is set. The one
/// definition of "the models that matter here", shared by routing and by
/// `fleet_status`.
#[must_use]
pub fn narrow(
    catalogue: &[WorkerModel],
    provider: Option<&str>,
    allow: &[String],
) -> Vec<WorkerModel> {
    catalogue
        .iter()
        .filter(|m| provider.is_none_or(|p| m.provider == p))
        .filter(|m| allow.is_empty() || allow.iter().any(|want| *want == m.key() || *want == m.id))
        .cloned()
        .collect()
}

/// The models routing may choose between: the catalogue, narrowed to the
/// `[routing] models` allowlist when one is set and to the pinned provider
/// when there is one.
///
/// # Errors
///
/// A one-line reason when nothing is left to choose from, or too much to put
/// in one question.
pub fn candidates(brief: &Brief<'_>, config: &RoutingConfig) -> Result<Vec<WorkerModel>, String> {
    let list = narrow(brief.catalogue, brief.provider, &config.models);
    if list.is_empty() {
        return Err(if brief.catalogue.is_empty() {
            "no pi catalogue yet — it is written when a worker first starts".to_string()
        } else {
            "no model in pi's catalogue matches [routing] models and the pinned provider"
                .to_string()
        });
    }
    if list.len() > MAX_CHOICES {
        return Err(format!(
            "pi offers {} models here, more than one judgment can weigh ({MAX_CHOICES}) — \
list the ones routing may choose under [routing] models, or pin a [worker] provider",
            list.len()
        ));
    }
    Ok(list)
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
    // a decline the user can act on is said, not swallowed: routing is on,
    // and a spawn that quietly did not route would look like a broken one
    let candidates = match candidates(brief, config) {
        Ok(list) => list,
        Err(reason) => {
            return Some(Routing {
                note: format!("not routed: {reason}"),
                ..Routing::default()
            });
        }
    };
    let body = request_body(brief, &candidates, config);
    let Some(answers) = post(&endpoint(config), key, &body).await else {
        return Some(Routing {
            note: "not routed: jev did not answer; kept the configured defaults".to_string(),
            ..Routing::default()
        });
    };
    Some(decide(brief, &candidates, config, &answers))
}

/// The request body: the state to judge against, and four questions that do
/// not depend on one another, so they answer in one round trip.
fn request_body(brief: &Brief<'_>, candidates: &[WorkerModel], config: &RoutingConfig) -> Value {
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
    let criteria: serde_json::Map<String, Value> = candidates
        .iter()
        .map(|m| (m.key(), Value::String(describe(m))))
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

/// One model as the judgment sees it: what it is, who serves it, how much it
/// can hold and what it costs — the last being what "prefer the smallest
/// model that can finish it well" is weighed against.
fn describe(model: &WorkerModel) -> String {
    let mut out = model.name.clone().unwrap_or_else(|| model.id.clone());
    out.push_str(&format!(", served by {}", model.provider));
    if let Some(window) = model.context_window {
        out.push_str(&format!(", {}k context", window / 1000));
    }
    if let Some(cost) = model.cost {
        out.push_str(&format!(
            ", ${:.2} in / ${:.2} out per million tokens",
            cost.input, cost.output
        ));
    }
    if model.thinking_levels.is_empty() {
        out.push_str(", no reasoning levels");
    } else {
        out.push_str(&format!(", reasoning {}", model.thinking_levels.join("/")));
    }
    out
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
fn decide(
    brief: &Brief<'_>,
    candidates: &[WorkerModel],
    config: &RoutingConfig,
    answers: &Answers,
) -> Routing {
    let mut routing = Routing::default();
    let threshold = config.confidence_threshold();

    let model = answers.answers.get("model");
    let confidence = model.and_then(|a| a.confidence).unwrap_or(0.0);
    routing.confidence = confidence;
    let named = model
        .and_then(|a| a.choice.as_deref())
        .and_then(|key| candidates.iter().find(|m| m.key() == key));
    let chosen = match (named, confidence >= threshold) {
        (Some(model), true) => {
            routing.model = Some(model.id.clone());
            routing.provider = (!model.provider.is_empty()).then(|| model.provider.clone());
            routing.note = format!("jev chose {} ({confidence:.2})", model.key());
            Some(model)
        }
        (Some(model), false) => {
            routing.note = format!(
                "jev leaned to {} but only at {confidence:.2} (threshold {threshold:.2}); \
kept the configured model",
                model.key()
            );
            None
        }
        (None, _) => {
            routing.note = "jev named no candidate; kept the configured model".into();
            None
        }
    };

    // The effort Score is read on its own scale, then clamped to the levels
    // of the model that will actually run — the chosen one, or the fallback
    // when the choice was not taken. pi accepts a level its model lacks and
    // then silently ignores it (verified fact 5), so a level that model does
    // not have is not asked for at all.
    let runs = chosen.or_else(|| {
        let fallback = brief.fallback_model?;
        brief.catalogue.iter().find(|m| {
            (m.id == fallback || m.key() == fallback)
                && brief.provider.is_none_or(|p| m.provider == p)
        })
    });
    if let (Some(score), Some(runs)) = (answers.answers.get("effort").and_then(|a| a.score), runs) {
        routing.thinking = clamp_effort(score, &runs.thinking_levels);
    }

    // `true` only ever keeps what the spawn already has; `false` drops the
    // worktree, and so needs a judgment that is sure of it
    routing.worktree = answers
        .answers
        .get("needs_worktree")
        .and_then(|a| a.noul)
        .map(|p| p >= READ_ONLY_BELOW);
    routing.parallel_safe = answers
        .answers
        .get("parallel_safe")
        .and_then(|a| a.noul)
        .map(|p| p >= 0.5);
    routing
}

/// The level nearest the effort `score` among the ones a model has, or
/// `None` when it has none to set. Ties go to the higher level: too little
/// reasoning costs more than a little too much.
fn clamp_effort(score: f64, levels: &[String]) -> Option<String> {
    let order = |level: &str| {
        crate::fleet::run::THINKING_LEVELS
            .iter()
            .position(|l| *l == level)
    };
    let at = (score.round().max(0.0) as usize).min(EFFORT_SCALE.len() - 1);
    let want = order(EFFORT_SCALE[at])?;
    levels
        .iter()
        .filter_map(|level| order(level).map(|i| (i, level)))
        .min_by_key(|(i, _)| (i.abs_diff(want), std::cmp::Reverse(*i)))
        .map(|(_, level)| level.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str, levels: &[&str]) -> WorkerModel {
        WorkerModel {
            thinking_levels: levels.iter().map(ToString::to_string).collect(),
            ..WorkerModel::new(provider, id)
        }
    }

    /// Shaped like the real catalogue: the same id from two providers, one
    /// model with a narrow level map and one with none.
    fn catalogue() -> Vec<WorkerModel> {
        vec![
            WorkerModel {
                name: Some("Opus 5".into()),
                context_window: Some(1_000_000),
                cost: Some(crate::fleet::run::ModelCost {
                    input: 5.0,
                    output: 25.0,
                }),
                ..model(
                    "anthropic",
                    "claude-opus-5",
                    &["off", "high", "xhigh", "max"],
                )
            },
            model(
                "claude-bridge",
                "claude-opus-5",
                &["off", "high", "xhigh", "max"],
            ),
            model("openrouter", "deepseek-v4-flash", &["off", "high", "xhigh"]),
            model(
                "opencode-go",
                "deepseek-v4-flash",
                &["off", "high", "xhigh"],
            ),
            model("anthropic", "claude-haiku-4-5", &[]),
        ]
    }

    fn brief<'a>(models: &'a [WorkerModel], in_flight: &'a [RunState]) -> Brief<'a> {
        Brief {
            name: "add-auth",
            brief: "add token refresh",
            repo_root: Path::new("/repo"),
            in_flight,
            catalogue: models,
            provider: None,
            fallback_model: None,
        }
    }

    fn answers(json: Value) -> Answers {
        serde_json::from_value(json).unwrap()
    }

    fn decide_all(b: &Brief<'_>, json: Value) -> Routing {
        let config = RoutingConfig::default();
        let list = candidates(b, &config).unwrap();
        decide(b, &list, &config, &answers(json))
    }

    #[test]
    fn every_candidate_is_named_by_provider_and_id() {
        let models = catalogue();
        let mut running = RunState {
            name: "docs".into(),
            task_brief: "rewrite the readme\nsecond line".into(),
            ..RunState::default()
        };
        running.branch = Some("parl/docs-1234567".into());
        let in_flight = vec![running];
        let b = brief(&models, &in_flight);
        let list = candidates(&b, &RoutingConfig::default()).unwrap();
        let body = request_body(&b, &list, &RoutingConfig::default());

        let criteria = body["questions"]["model"]["criteria"].as_object().unwrap();
        assert_eq!(
            criteria.len(),
            5,
            "the same id from two providers stays two options: {criteria:?}"
        );
        let opus = criteria["anthropic:claude-opus-5"].as_str().unwrap();
        assert!(opus.contains("Opus 5"), "{opus}");
        assert!(opus.contains("1000k context"), "{opus}");
        assert!(
            opus.contains("$5.00 in / $25.00 out"),
            "cost is what it weighs: {opus}"
        );
        assert!(
            criteria["anthropic:claude-haiku-4-5"]
                .as_str()
                .unwrap()
                .contains("no reasoning levels")
        );
        assert_eq!(body["state"]["inFlight"][0]["brief"], "rewrite the readme");
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["questions"].as_object().unwrap().len(), 4);
    }

    #[test]
    fn candidates_follow_the_allowlist_and_the_pinned_provider() {
        let models = catalogue();
        let config = RoutingConfig {
            models: vec!["deepseek-v4-flash".into(), "anthropic:claude-opus-5".into()],
            ..RoutingConfig::default()
        };
        let keys = |list: Vec<WorkerModel>| list.iter().map(WorkerModel::key).collect::<Vec<_>>();
        assert_eq!(
            keys(candidates(&brief(&models, &[]), &config).unwrap()),
            vec![
                "anthropic:claude-opus-5",
                "openrouter:deepseek-v4-flash",
                "opencode-go:deepseek-v4-flash"
            ],
            "a bare id admits every provider, provider:id admits one"
        );
        let pinned = Brief {
            provider: Some("opencode-go"),
            ..brief(&models, &[])
        };
        assert_eq!(
            keys(candidates(&pinned, &config).unwrap()),
            vec!["opencode-go:deepseek-v4-flash"],
            "a pinned provider is never contradicted"
        );
    }

    #[test]
    fn too_many_candidates_or_none_is_a_reason_not_a_request() {
        let huge: Vec<WorkerModel> = (0..300)
            .map(|i| model("openrouter", &format!("m-{i}"), &[]))
            .collect();
        let err = candidates(&brief(&huge, &[]), &RoutingConfig::default()).unwrap_err();
        assert!(err.contains("300 models"), "{err}");
        assert!(
            err.contains("[routing] models"),
            "says how to narrow it: {err}"
        );
        let err = candidates(&brief(&[], &[]), &RoutingConfig::default()).unwrap_err();
        assert!(err.contains("no pi catalogue"), "{err}");
    }

    #[test]
    fn a_confident_choice_brings_its_own_provider() {
        let models = catalogue();
        let decision = decide_all(
            &brief(&models, &[]),
            json!({"answers": {
                "model": {"choice": "opencode-go:deepseek-v4-flash", "confidence": 0.9},
                "needs_worktree": {"noul": 0.95},
                "parallel_safe": {"noul": 0.1},
            }}),
        );
        assert_eq!(decision.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(
            decision.provider.as_deref(),
            Some("opencode-go"),
            "not whichever provider happens to come first"
        );
        assert_eq!(decision.worktree, Some(true));
        assert_eq!(decision.parallel_safe, Some(false));
    }

    #[test]
    fn an_unconfident_or_unknown_choice_keeps_the_configured_model() {
        let models = catalogue();
        let decision = decide_all(
            &brief(&models, &[]),
            json!({"answers": {"model": {"choice": "anthropic:claude-opus-5", "confidence": 0.2}}}),
        );
        assert_eq!(decision.model, None);
        assert!(decision.note.contains("threshold"), "{}", decision.note);
        let decision = decide_all(
            &brief(&models, &[]),
            json!({"answers": {"model": {"choice": "claude-opus-5", "confidence": 1.0}}}),
        );
        assert_eq!(decision.model, None, "a bare id is not a candidate's name");
    }

    #[test]
    fn effort_is_clamped_to_the_levels_of_the_model_that_runs() {
        let models = catalogue();
        let b = brief(&models, &[]);
        let effort = |choice: &str, score: f64| {
            decide_all(
                &b,
                json!({"answers": {
                    "model": {"choice": choice, "confidence": 0.9},
                    "effort": {"score": score},
                }}),
            )
            .thinking
        };
        // deepseek-v4-flash has off/high/xhigh only
        let flash = "openrouter:deepseek-v4-flash";
        assert_eq!(effort(flash, 0.0).as_deref(), Some("off"));
        assert_eq!(
            effort(flash, 1.0).as_deref(),
            Some("high"),
            "low is not there"
        );
        assert_eq!(effort(flash, 2.0).as_deref(), Some("high"), "nor medium");
        assert_eq!(effort(flash, 4.0).as_deref(), Some("xhigh"), "nor max");
        // a model with no levels gets none asked for
        assert_eq!(effort("anthropic:claude-haiku-4-5", 3.0), None);
    }

    #[test]
    fn an_untaken_choice_clamps_effort_to_the_fallback_model_instead() {
        let models = catalogue();
        let b = Brief {
            fallback_model: Some("deepseek-v4-flash"),
            provider: Some("opencode-go"),
            ..brief(&models, &[])
        };
        let list = candidates(&b, &RoutingConfig::default()).unwrap();
        let decision = decide(
            &b,
            &list,
            &RoutingConfig::default(),
            &answers(json!({"answers": {
                "model": {"choice": "opencode-go:deepseek-v4-flash", "confidence": 0.1},
                "effort": {"score": 4.0},
            }})),
        );
        assert_eq!(decision.model, None);
        assert_eq!(
            decision.thinking.as_deref(),
            Some("xhigh"),
            "the fallback's levels, not the scale's `max`"
        );
        // and with no model known to run, no level is guessed at
        let unknown = Brief {
            fallback_model: None,
            ..b
        };
        let decision = decide(
            &unknown,
            &list,
            &RoutingConfig::default(),
            &answers(json!({"answers": {"effort": {"score": 4.0}}})),
        );
        assert_eq!(decision.thinking, None);
    }

    #[test]
    fn a_worktree_is_only_dropped_on_a_confident_read_only() {
        let models = catalogue();
        let worktree = |p: f64| {
            decide_all(
                &brief(&models, &[]),
                json!({"answers": {"needs_worktree": {"noul": p}}}),
            )
            .worktree
        };
        assert_eq!(worktree(0.45), Some(true), "a coin flip keeps the worktree");
        assert_eq!(worktree(0.2), Some(true));
        assert_eq!(worktree(0.05), Some(false));
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
            "model": {"type": "choice", "choice": "opencode-go:deepseek-v4-flash", "confidence": 0.82},
            "effort": {"type": "score", "score": 3.0},
            "needs_worktree": {"type": "noul", "noul": 0.9},
            "parallel_safe": {"type": "noul", "noul": 0.8},
        }}))
        .await;
        let models = catalogue();
        let config = RoutingConfig {
            enabled: true,
            endpoint: Some(url),
            ..RoutingConfig::default()
        };
        let decision = route(&brief(&models, &[]), &config, Some("sk-test"))
            .await
            .expect("a decision");
        assert_eq!(decision.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(decision.provider.as_deref(), Some("opencode-go"));
        assert_eq!(decision.thinking.as_deref(), Some("high"));
        assert_eq!(decision.worktree, Some(true));

        let (head, body) = rx.await.unwrap();
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-test"),
            "the key rides as a bearer token: {head}"
        );
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["state"]["task"]["name"], "add-auth");
        assert!(sent["questions"]["model"]["criteria"]["anthropic:claude-opus-5"].is_string());
    }

    #[tokio::test]
    async fn a_judgment_that_cannot_be_had_says_so_rather_than_failing() {
        let models = catalogue();
        let config = RoutingConfig {
            enabled: true,
            // nothing listening: a spawn must not fail because a judgment could not be had
            endpoint: Some("http://127.0.0.1:1/v1/systemone".to_string()),
            ..RoutingConfig::default()
        };
        let decision = route(&brief(&models, &[]), &config, Some("sk-test"))
            .await
            .expect("routing was on, so the outcome is said");
        assert_eq!(decision.model, None);
        assert!(
            decision.note.contains("did not answer"),
            "{}",
            decision.note
        );
    }

    #[tokio::test]
    async fn routing_is_silent_when_off_or_unkeyed_and_says_why_it_declined() {
        let models = catalogue();
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
        assert!(route(&brief(&models, &[]), &on, None).await.is_none());
        let declined = route(&brief(&[], &[]), &on, Some("k")).await.unwrap();
        assert!(
            declined.note.starts_with("not routed:"),
            "{}",
            declined.note
        );
    }
}
