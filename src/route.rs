//! Routing a brief: which model runs it, how hard it should think, and
//! whether it needs a worktree of its own.
//!
//! The judgment comes from TypeSafe's System One (Jev), asked over HTTP in
//! two cycles. There is no Rust SDK, so this speaks the documented API
//! directly: `POST /v1/systemone` with `{state, model, questions}`,
//! answering `{answers: {id: …}}`.
//!
//! 1. **The model**, with the two worktree questions that do not depend on
//!    it. Each candidate is shown with its price and how many times the
//!    cheapest option it costs, and the question asks for the best result
//!    for the money rather than the most capable model. A near-tie in Jev's
//!    probabilities goes to the cheaper model. Below the confidence limit
//!    the choice is not taken: [`judge`] hands back the candidates, ranked,
//!    for the caller to put to the human.
//! 2. **The thinking level**, asked once the model is known, as a choice
//!    between the levels *that* model has — pi accepts a level its model
//!    lacks and then ignores it (verified fact 5), so no other is offered.
//!
//! Code owns the workflow, which is what the model is for. The candidates
//! come from pi's own catalogue, narrowed to what the user shortlisted; each
//! is named `provider:id`, because most ids are served by several providers
//! and they do not behave alike (verified fact 4). Nothing here is required:
//! with routing off, or no API key, the caller gets `None` and spawns
//! exactly as it always did.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fleet::run::{RunState, WorkerModel};
use crate::paths::{FleetPaths, RoutingConfig, env_var};

/// Where the API lives, unless `$PARL_TYPESAFE_URL` says otherwise.
const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// A judgment is on the spawn path, so it cannot hang it. The documented
/// latency is around 100 ms; past this the spawn goes ahead unrouted.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The most options one Choice question takes (TypeSafe's documented limit),
/// and so the most models a shortlist may hold.
pub const MAX_CHOICES: usize = 255;

/// Two candidates this close in Jev's probabilities are a tie, and a tie goes
/// to the cheaper one: paying more has to buy a clearly better fit.
const NEAR_TIE: f64 = 0.05;

/// A worktree is only dropped when the brief is this surely read-only. Being
/// wrong the other way puts a worker's edits straight into the human's
/// checkout, with no branch to diff or merge, so a coin flip keeps it.
const READ_ONLY_BELOW: f64 = 0.1;

/// The questions, as their own constants: a multi-line string literal inside
/// `json!` is reindented by rustfmt, and the indent lands *in the prompt*.
const MODEL_QUESTION: &str = "Which model gives the best result for its cost on \
`task.brief`? Each option lists its price and how many times the cheapest option it \
costs. A dearer model is only worth it when the task clearly needs what it adds; for \
routine work, the cheaper model that will finish it well is the right answer.";
const THINKING_QUESTION: &str = "How much should `model` reason while it carries out \
`task.brief`? Reasoning costs tokens and time; choose the least that lets it do the \
task well.";
const WORKTREE_QUESTION: &str = "Will carrying out `task.brief` change files in the \
repository? Answer no for reading, reviewing, summarising or investigating.";
const PARALLEL_QUESTION: &str = "Can `task.brief` run at the same time as everything in \
`inFlight` without them editing the same files?";

/// What each thinking level is for, as the thinking question's criteria.
fn level_criterion(level: &str) -> &'static str {
    match level {
        "off" => "No extended reasoning: the change is fully specified and mechanical.",
        "minimal" => "A moment's thought: a small change with one obvious approach.",
        "low" => "Light reasoning: simple work across a handful of files.",
        "medium" => "Moderate reasoning: some design judgment, or code to understand first.",
        "high" => "Deep reasoning: several interacting parts, or a non-obvious approach.",
        "xhigh" => "Very deep reasoning: the approach itself has to be found.",
        "max" => "Everything the model has: the hardest problems, whatever they cost.",
        _ => "A reasoning level this model offers.",
    }
}

/// What a brief was routed to, recorded on the run so the decision is
/// auditable rather than mysterious.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Routing {
    /// The pi model id that was chosen — by Jev, or by the human when Jev
    /// was unsure — or none when the configured model stands.
    pub model: Option<String>,
    pub provider: Option<String>,
    /// The thinking level Jev chose among the running model's own.
    pub thinking: Option<String>,
    /// Whether the brief was judged to need its own worktree.
    pub worktree: Option<bool>,
    /// Whether it was judged safe to run alongside what is already in flight.
    pub parallel_safe: Option<bool>,
    /// Jev's confidence in its model choice, 0..1.
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
    /// pinned model or the configured default.
    pub fallback_model: Option<&'a str>,
    /// The caller named the model itself, so only the thinking level and the
    /// worktree are judged.
    pub model_pinned: bool,
}

/// One candidate as the human is shown it when Jev is unsure: its name,
/// how likely Jev thought it the right one, and what it costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    /// `provider:id`.
    pub key: String,
    /// Jev's probability for it, when it gave one.
    #[serde(default)]
    pub probability: Option<f64>,
    /// Name, price and relative cost, in one short line.
    #[serde(default)]
    pub detail: String,
}

/// The first cycle's outcome: what was decided, and — when Jev was not sure
/// enough of the model — the candidates to put to the human, the one it
/// leaned to first.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Judgment {
    pub routing: Routing,
    pub ask: Option<Vec<ModelOption>>,
}

/// pi's catalogue narrowed to one provider, when one is pinned, and to an
/// allowlist of `provider:id` or bare-id entries, when one is set. The one
/// definition of "the models that matter here", shared by routing, the
/// shortlist and `fleet_status`.
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
/// `[routing] models` shortlist when one is set and to the pinned provider
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
            "no model in pi's catalogue matches the shortlist and the pinned provider".to_string()
        });
    }
    if list.len() > MAX_CHOICES {
        return Err(format!(
            "pi offers {} models here, more than one judgment can weigh ({MAX_CHOICES}) — \
shortlist the ones routing may choose with /routing in the console, or pin a [worker] provider",
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

/// The first cycle: the model (unless the caller pinned one) and the two
/// worktree questions. `None` when routing is off or unkeyed.
///
/// Never an error: a judgment that cannot be obtained is a judgment not
/// made, and the caller falls back to the configured default. The reason is
/// carried on [`Routing::note`].
pub async fn judge(
    brief: &Brief<'_>,
    config: &RoutingConfig,
    key: Option<&str>,
) -> Option<Judgment> {
    if !config.enabled {
        return None;
    }
    let key = key?;
    // a decline the user can act on is said, not swallowed: routing is on,
    // and a spawn that quietly did not route would look like a broken one
    let candidates = if brief.model_pinned {
        Vec::new()
    } else {
        match candidates(brief, config) {
            Ok(list) => list,
            Err(reason) => return Some(declined(format!("not routed: {reason}"))),
        }
    };
    let body = model_body(brief, &candidates, config);
    let Some(answers) = post(&endpoint(config), key, &body).await else {
        return Some(declined(
            "not routed: jev did not answer; kept the configured defaults".to_string(),
        ));
    };
    Some(judge_answers(brief, &candidates, config, &answers))
}

fn declined(note: String) -> Judgment {
    Judgment {
        routing: Routing {
            note,
            ..Routing::default()
        },
        ask: None,
    }
}

/// The second cycle: the thinking level for the model that will run, among
/// the levels it has, with Jev's confidence. `None` when there is nothing to
/// choose between or no answer came.
pub async fn choose_thinking(
    brief: &Brief<'_>,
    model: &WorkerModel,
    config: &RoutingConfig,
    key: &str,
) -> Option<(String, f64)> {
    if model.thinking_levels.len() < 2 {
        return None;
    }
    let body = thinking_body(brief, model, config);
    let answers = post(&endpoint(config), key, &body).await?;
    thinking_answer(model, &answers)
}

/// The model that will actually run: the routed one, or the fallback, as the
/// catalogue knows it under the pinned provider.
#[must_use]
pub fn model_that_runs<'a>(brief: &Brief<'a>, routing: &Routing) -> Option<&'a WorkerModel> {
    let wanted = match (&routing.model, &routing.provider) {
        (Some(id), Some(provider)) => format!("{provider}:{id}"),
        (Some(id), None) => id.clone(),
        (None, _) => brief.fallback_model?.to_string(),
    };
    let provider = routing.provider.as_deref().or(brief.provider);
    brief
        .catalogue
        .iter()
        .find(|m| (m.key() == wanted || m.id == wanted) && provider.is_none_or(|p| m.provider == p))
}

/// The first cycle's body: the state to judge against, and the questions
/// that do not depend on one another, so they answer in one round trip.
fn model_body(brief: &Brief<'_>, candidates: &[WorkerModel], config: &RoutingConfig) -> Value {
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
    let mut questions = serde_json::Map::new();
    if !candidates.is_empty() {
        let cheapest = cheapest(candidates);
        let criteria: serde_json::Map<String, Value> = candidates
            .iter()
            .map(|m| (m.key(), Value::String(describe(m, cheapest))))
            .collect();
        questions.insert(
            "model".into(),
            json!({"type": "choice", "instructions": MODEL_QUESTION, "criteria": criteria}),
        );
    }
    questions.insert(
        "needs_worktree".into(),
        json!({"type": "noul", "instructions": WORKTREE_QUESTION}),
    );
    questions.insert(
        "parallel_safe".into(),
        json!({"type": "noul", "instructions": PARALLEL_QUESTION}),
    );
    json!({
        "model": config.model,
        "state": {
            "task": {"name": brief.name, "brief": brief.brief},
            "repository": brief.repo_root.to_string_lossy(),
            "inFlight": in_flight,
        },
        "questions": questions,
    })
}

/// The second cycle's body: the brief, the model that will run it, and one
/// choice between that model's levels.
fn thinking_body(brief: &Brief<'_>, model: &WorkerModel, config: &RoutingConfig) -> Value {
    let criteria: serde_json::Map<String, Value> = model
        .thinking_levels
        .iter()
        .map(|level| {
            (
                level.clone(),
                Value::String(level_criterion(level).to_string()),
            )
        })
        .collect();
    json!({
        "model": config.model,
        "state": {
            "task": {"name": brief.name, "brief": brief.brief},
            "model": describe(model, None),
        },
        "questions": {
            "thinking": {
                "type": "choice",
                "instructions": THINKING_QUESTION,
                "criteria": criteria,
            },
        },
    })
}

/// One price per model to compare by: input and output blended 3:1, the
/// usual weighting for agentic work, which reads far more than it writes.
fn blended(model: &WorkerModel) -> Option<f64> {
    model.cost.map(|c| (3.0 * c.input + c.output) / 4.0)
}

/// The lowest non-zero blended price among `models`: what "N times the
/// cheapest" is measured against. A free model has no ratio to anything.
fn cheapest(models: &[WorkerModel]) -> Option<f64> {
    models
        .iter()
        .filter_map(blended)
        .filter(|price| *price > 0.0)
        .min_by(f64::total_cmp)
}

/// How a price compares with the cheapest option, in words.
fn relative(model: &WorkerModel, cheapest: Option<f64>) -> Option<String> {
    let price = blended(model)?;
    if price <= 0.0 {
        return Some("free".to_string());
    }
    let ratio = price / cheapest?;
    Some(if ratio < 1.05 {
        "the cheapest option".to_string()
    } else {
        format!("{ratio:.1}× the cheapest option")
    })
}

/// One model as the judgment sees it: what it is, who serves it, how much it
/// can hold, and what it costs — absolutely and against the cheapest option,
/// which is what the value question is weighed against.
fn describe(model: &WorkerModel, cheapest: Option<f64>) -> String {
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
    if let Some(relative) = relative(model, cheapest) {
        out.push_str(&format!(" ({relative})"));
    }
    if model.thinking_levels.is_empty() {
        out.push_str(", no reasoning levels");
    } else {
        out.push_str(&format!(", reasoning {}", model.thinking_levels.join("/")));
    }
    out
}

/// The short line a human choosing between models reads beside each one.
fn summary(model: &WorkerModel, cheapest: Option<f64>) -> String {
    let mut parts = Vec::new();
    if let Some(name) = &model.name
        && *name != model.id
    {
        parts.push(name.clone());
    }
    if let Some(cost) = model.cost {
        parts.push(format!("${:.2} / ${:.2}", cost.input, cost.output));
    }
    if let Some(relative) = relative(model, cheapest) {
        parts.push(relative);
    }
    parts.join(" · ")
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
    answers: HashMap<String, Answer>,
}

/// One answer. The primitives share a shape loosely enough that one tolerant
/// struct reads all of them, which is also what keeps a newer field from
/// breaking an older reader.
#[derive(Debug, Clone, Default, Deserialize)]
struct Answer {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    probabilities: HashMap<String, f64>,
    #[serde(default)]
    noul: Option<f64>,
    #[serde(default)]
    confidence: Option<f64>,
}

/// Turn the first cycle's answers into a decision. This is where policy
/// lives: the model supplies judgments, code decides what to do with them.
fn judge_answers(
    brief: &Brief<'_>,
    candidates: &[WorkerModel],
    config: &RoutingConfig,
    answers: &Answers,
) -> Judgment {
    let mut judgment = Judgment::default();
    let routing = &mut judgment.routing;

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

    if brief.model_pinned || candidates.is_empty() {
        routing.note = "the model was named by the caller".to_string();
        return judgment;
    }
    let threshold = config.confidence_threshold();
    let answer = answers.answers.get("model").cloned().unwrap_or_default();
    let confidence = answer.confidence.unwrap_or(0.0);
    routing.confidence = confidence;
    let named = answer
        .choice
        .as_deref()
        .and_then(|key| candidates.iter().find(|m| m.key() == key));
    let pick = named.map(|named| value_pick(named, candidates, &answer.probabilities));
    match pick {
        Some(pick) if confidence >= threshold => {
            routing.model = Some(pick.id.clone());
            routing.provider = (!pick.provider.is_empty()).then(|| pick.provider.clone());
            routing.note = match named {
                Some(named) if named.key() != pick.key() => format!(
                    "jev chose {} ({confidence:.2}), cheaper than the near-tied {}",
                    pick.key(),
                    named.key()
                ),
                _ => format!("jev chose {} ({confidence:.2})", pick.key()),
            };
        }
        _ => {
            routing.note = match pick {
                Some(pick) => format!(
                    "jev leaned to {} at {confidence:.2}, under the {threshold:.2} limit",
                    pick.key()
                ),
                None => "jev named no candidate".to_string(),
            };
            judgment.ask = Some(ranked(pick, candidates, &answer.probabilities));
        }
    }
    judgment
}

/// Jev's pick, unless a cheaper candidate is a near-tie with it — then the
/// cheapest of those. A model whose price is unknown cannot undercut
/// anything, and cannot be undercut either.
fn value_pick<'a>(
    named: &'a WorkerModel,
    candidates: &'a [WorkerModel],
    probabilities: &HashMap<String, f64>,
) -> &'a WorkerModel {
    let (Some(top), Some(top_price)) = (probabilities.get(&named.key()), blended(named)) else {
        return named;
    };
    candidates
        .iter()
        .filter(|m| {
            probabilities
                .get(&m.key())
                .is_some_and(|p| *p >= top - NEAR_TIE)
        })
        .filter_map(|m| blended(m).map(|price| (m, price)))
        .filter(|(_, price)| *price < top_price)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(named, |(m, _)| m)
}

/// Every candidate for the human, Jev's leaning first, then by probability,
/// cheaper first among equals.
fn ranked(
    pick: Option<&WorkerModel>,
    candidates: &[WorkerModel],
    probabilities: &HashMap<String, f64>,
) -> Vec<ModelOption> {
    let cheapest = cheapest(candidates);
    let mut order: Vec<&WorkerModel> = candidates.iter().collect();
    let p = |m: &WorkerModel| probabilities.get(&m.key()).copied().unwrap_or(0.0);
    order.sort_by(|a, b| {
        let first = |m: &WorkerModel| pick.is_some_and(|pick| pick.key() == m.key());
        first(b).cmp(&first(a)).then(p(b).total_cmp(&p(a))).then(
            blended(a)
                .unwrap_or(f64::MAX)
                .total_cmp(&blended(b).unwrap_or(f64::MAX)),
        )
    });
    order
        .into_iter()
        .map(|m| ModelOption {
            key: m.key(),
            probability: probabilities.get(&m.key()).copied(),
            detail: summary(m, cheapest),
        })
        .collect()
}

/// The second cycle's answer: a level the model has, or nothing.
fn thinking_answer(model: &WorkerModel, answers: &Answers) -> Option<(String, f64)> {
    let answer = answers.answers.get("thinking")?;
    let level = answer.choice.as_deref()?;
    model
        .thinking_levels
        .iter()
        .any(|l| l == level)
        .then(|| (level.to_string(), answer.confidence.unwrap_or(0.0)))
}

// ---------------------------------------------------------------------------
// Asking the human

/// A model choice Jev was unsure of, put to the human in the console while
/// the spawn waits. Written by the spawn as `routing/<id>.json`; the console
/// answers with `routing/<id>.answer.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelQuestion {
    pub id: String,
    /// The worker being spawned.
    pub name: String,
    /// The first line of its brief.
    pub brief: String,
    pub confidence: f64,
    pub threshold: f64,
    /// Every candidate, Jev's leaning first.
    pub options: Vec<ModelOption>,
    /// What runs when nobody chooses: the configured model, or pi's own
    /// default when none is configured.
    #[serde(default)]
    pub fallback: Option<String>,
    pub asked_at: String,
    /// The spawn waiting on it. A question whose asker is gone is not asked.
    pub pid: u32,
    /// When the spawn stops waiting and keeps the fallback, in ms.
    pub deadline_ms: i64,
}

/// The human's answer: a candidate's `provider:id`, or none to keep the
/// fallback.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelAnswer {
    pub model: Option<String>,
}

fn question_path(paths: &FleetPaths, id: &str) -> std::path::PathBuf {
    paths.routing_dir().join(format!("{id}.json"))
}

fn answer_path(paths: &FleetPaths, id: &str) -> std::path::PathBuf {
    paths.routing_dir().join(format!("{id}.answer.json"))
}

/// Put a question up for the console.
///
/// # Errors
///
/// When the `routing/` directory or the file cannot be written.
pub fn post_question(paths: &FleetPaths, question: &ModelQuestion) -> std::io::Result<()> {
    std::fs::create_dir_all(paths.routing_dir())?;
    crate::util::atomic_write_json(&question_path(paths, &question.id), question)
}

/// The human's answer to question `id`, once there is one.
#[must_use]
pub fn read_answer(paths: &FleetPaths, id: &str) -> Option<ModelAnswer> {
    let raw = std::fs::read_to_string(answer_path(paths, id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Take question `id` down, with its answer.
pub fn remove_question(paths: &FleetPaths, id: &str) {
    let _ = std::fs::remove_file(question_path(paths, id));
    let _ = std::fs::remove_file(answer_path(paths, id));
}

/// Answer question `id` — what the console writes when the human chooses.
///
/// # Errors
///
/// When the answer file cannot be written.
pub fn answer_question(paths: &FleetPaths, id: &str, answer: &ModelAnswer) -> std::io::Result<()> {
    std::fs::create_dir_all(paths.routing_dir())?;
    crate::util::atomic_write_json(&answer_path(paths, id), answer)
}

/// The questions still waiting on the human, oldest first: unanswered, with
/// a live asker and time left. Anything else is litter from a spawn that
/// was interrupted, and is not asked.
#[must_use]
pub fn pending_questions(paths: &FleetPaths) -> Vec<ModelQuestion> {
    let Ok(entries) = std::fs::read_dir(paths.routing_dir()) else {
        return Vec::new();
    };
    let now = crate::util::now_ms();
    let mut questions: Vec<ModelQuestion> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && !path.to_string_lossy().ends_with(".answer.json")
        })
        .filter_map(|path| serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok())
        .filter(|q: &ModelQuestion| {
            q.deadline_ms > now
                && crate::fleet::run::is_alive(i32::try_from(q.pid).ok())
                && !answer_path(paths, &q.id).exists()
        })
        .collect();
    questions.sort_by(|a, b| a.asked_at.cmp(&b.asked_at));
    questions
}

/// Shared by the routing tests and the spawn tests that route through it.
#[cfg(test)]
pub(crate) mod test_support {
    use serde_json::Value;

    /// A small HTTP stub: answers one request per reply, in order, handing
    /// each request back over a channel. Enough to prove the wire contract
    /// without a mock-server dependency.
    pub(crate) async fn stub(
        replies: Vec<Value>,
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<(String, String)>,
    ) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for reply in replies {
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
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_string)
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
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        (url, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::stub;
    use super::*;

    fn model(provider: &str, id: &str, levels: &[&str]) -> WorkerModel {
        WorkerModel {
            thinking_levels: levels.iter().map(ToString::to_string).collect(),
            ..WorkerModel::new(provider, id)
        }
    }

    fn priced(mut m: WorkerModel, input: f64, output: f64) -> WorkerModel {
        m.cost = Some(crate::fleet::run::ModelCost { input, output });
        m
    }

    /// Shaped like the real catalogue: the same id from two providers, one
    /// model with a narrow level map and one with none.
    fn catalogue() -> Vec<WorkerModel> {
        vec![
            WorkerModel {
                name: Some("Opus 5".into()),
                context_window: Some(1_000_000),
                ..priced(
                    model(
                        "anthropic",
                        "claude-opus-5",
                        &["off", "high", "xhigh", "max"],
                    ),
                    5.0,
                    25.0,
                )
            },
            model(
                "claude-bridge",
                "claude-opus-5",
                &["off", "high", "xhigh", "max"],
            ),
            priced(
                model("openrouter", "deepseek-v4-flash", &["off", "high", "xhigh"]),
                0.5,
                2.0,
            ),
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
            model_pinned: false,
        }
    }

    fn answers(json: Value) -> Answers {
        serde_json::from_value(json).unwrap()
    }

    fn judge_all(b: &Brief<'_>, json: Value) -> Judgment {
        let config = RoutingConfig::default();
        let list = candidates(b, &config).unwrap();
        judge_answers(b, &list, &config, &answers(json))
    }

    #[test]
    fn every_candidate_is_named_by_provider_and_priced_against_the_cheapest() {
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
        let body = model_body(&b, &list, &RoutingConfig::default());

        let criteria = body["questions"]["model"]["criteria"].as_object().unwrap();
        assert_eq!(
            criteria.len(),
            5,
            "the same id from two providers stays two options: {criteria:?}"
        );
        let opus = criteria["anthropic:claude-opus-5"].as_str().unwrap();
        assert!(opus.contains("Opus 5"), "{opus}");
        assert!(opus.contains("1000k context"), "{opus}");
        assert!(opus.contains("$5.00 in / $25.00 out"), "{opus}");
        // blended 3:1 — (3·5 + 25)/4 = 10 against (3·0.5 + 2)/4 = 0.875
        assert!(
            opus.contains("11.4× the cheapest option"),
            "value is weighed against the cheapest: {opus}"
        );
        assert!(
            criteria["openrouter:deepseek-v4-flash"]
                .as_str()
                .unwrap()
                .contains("(the cheapest option)")
        );
        assert!(
            criteria["anthropic:claude-haiku-4-5"]
                .as_str()
                .unwrap()
                .contains("no reasoning levels")
        );
        let question = body["questions"]["model"]["instructions"].as_str().unwrap();
        assert!(question.contains("best result for its cost"), "{question}");
        assert_eq!(body["state"]["inFlight"][0]["brief"], "rewrite the readme");
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(
            body["questions"].as_object().unwrap().len(),
            3,
            "the thinking level waits for the model: {body}"
        );
    }

    #[test]
    fn a_pinned_model_is_not_asked_about() {
        let models = catalogue();
        let b = Brief {
            model_pinned: true,
            fallback_model: Some("claude-opus-5"),
            ..brief(&models, &[])
        };
        let body = model_body(&b, &[], &RoutingConfig::default());
        assert!(body["questions"].get("model").is_none(), "{body}");
        let judgment = judge_answers(
            &b,
            &[],
            &RoutingConfig::default(),
            &answers(json!({"answers": {"needs_worktree": {"noul": 0.9}}})),
        );
        assert_eq!(judgment.ask, None);
        assert_eq!(judgment.routing.model, None);
        assert_eq!(judgment.routing.worktree, Some(true));
    }

    #[test]
    fn candidates_follow_the_shortlist_and_the_pinned_provider() {
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
        assert!(err.contains("shortlist"), "says how to narrow it: {err}");
        let err = candidates(&brief(&[], &[]), &RoutingConfig::default()).unwrap_err();
        assert!(err.contains("no pi catalogue"), "{err}");
    }

    #[test]
    fn a_confident_choice_brings_its_own_provider() {
        let models = catalogue();
        let judgment = judge_all(
            &brief(&models, &[]),
            json!({"answers": {
                "model": {"choice": "opencode-go:deepseek-v4-flash", "confidence": 0.9},
                "needs_worktree": {"noul": 0.95},
                "parallel_safe": {"noul": 0.1},
            }}),
        );
        let decision = judgment.routing;
        assert_eq!(judgment.ask, None);
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
    fn a_near_tie_goes_to_the_cheaper_model_and_a_clear_lead_does_not() {
        let models = catalogue();
        let pick = |opus: f64, flash: f64| {
            judge_all(
                &brief(&models, &[]),
                json!({"answers": {"model": {
                    "choice": "anthropic:claude-opus-5",
                    "confidence": 0.9,
                    "probabilities": {
                        "anthropic:claude-opus-5": opus,
                        "openrouter:deepseek-v4-flash": flash,
                    },
                }}}),
            )
            .routing
        };
        let tie = pick(0.45, 0.42);
        assert_eq!(tie.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(tie.provider.as_deref(), Some("openrouter"));
        assert!(tie.note.contains("near-tied"), "{}", tie.note);
        let lead = pick(0.7, 0.2);
        assert_eq!(lead.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn an_unsure_choice_is_put_to_the_human_leaning_first() {
        let models = catalogue();
        let judgment = judge_all(
            &brief(&models, &[]),
            json!({"answers": {"model": {
                "choice": "anthropic:claude-opus-5",
                "confidence": 0.3,
                "probabilities": {
                    "anthropic:claude-opus-5": 0.4,
                    "claude-bridge:claude-opus-5": 0.1,
                    "opencode-go:deepseek-v4-flash": 0.3,
                },
            }}}),
        );
        assert_eq!(judgment.routing.model, None, "not taken below the limit");
        assert!(
            judgment.routing.note.contains("0.30"),
            "{}",
            judgment.routing.note
        );
        let options = judgment.ask.expect("the human is asked");
        let keys: Vec<&str> = options.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "anthropic:claude-opus-5",
                "opencode-go:deepseek-v4-flash",
                "claude-bridge:claude-opus-5",
                // no probability: cheaper first
                "openrouter:deepseek-v4-flash",
                "anthropic:claude-haiku-4-5",
            ]
        );
        assert_eq!(options[0].probability, Some(0.4));
        assert!(
            options[0].detail.contains("Opus 5"),
            "{}",
            options[0].detail
        );
        assert!(
            options[0].detail.contains("$5.00 / $25.00"),
            "{}",
            options[0].detail
        );

        // a choice that names nothing on offer is asked about too
        let judgment = judge_all(
            &brief(&models, &[]),
            json!({"answers": {"model": {"choice": "claude-opus-5", "confidence": 1.0}}}),
        );
        assert_eq!(
            judgment.routing.model, None,
            "a bare id is not a candidate's name"
        );
        assert_eq!(judgment.ask.map(|o| o.len()), Some(5));
    }

    #[test]
    fn the_confidence_limit_decides_when_to_ask() {
        let models = catalogue();
        let b = brief(&models, &[]);
        let list = candidates(&b, &RoutingConfig::default()).unwrap();
        let reply = answers(json!({"answers": {"model": {
            "choice": "anthropic:claude-opus-5", "confidence": 0.7,
        }}}));
        let with = |limit: f64| RoutingConfig {
            confidence_threshold: Some(limit),
            ..RoutingConfig::default()
        };
        assert_eq!(judge_answers(&b, &list, &with(0.6), &reply).ask, None);
        assert!(judge_answers(&b, &list, &with(0.8), &reply).ask.is_some());
        assert!(
            judge_answers(&b, &list, &with(1.0), &reply).ask.is_some(),
            "a limit of 1 always asks"
        );
    }

    #[test]
    fn the_thinking_choice_offers_only_the_levels_the_model_has() {
        let models = catalogue();
        let b = brief(&models, &[]);
        let flash = &models[2];
        let body = thinking_body(&b, flash, &RoutingConfig::default());
        let criteria = body["questions"]["thinking"]["criteria"]
            .as_object()
            .unwrap();
        assert_eq!(
            criteria.keys().collect::<Vec<_>>(),
            vec!["high", "off", "xhigh"],
            "{criteria:?}"
        );
        assert!(
            body["state"]["model"]
                .as_str()
                .unwrap()
                .contains("deepseek-v4-flash")
        );
        let pick = |level: &str| {
            thinking_answer(
                flash,
                &answers(json!({"answers": {"thinking": {"choice": level, "confidence": 0.8}}})),
            )
        };
        assert_eq!(pick("high"), Some(("high".to_string(), 0.8)));
        assert_eq!(pick("max"), None, "a level the model lacks is never set");
    }

    #[test]
    fn the_model_that_runs_is_the_routed_one_or_the_fallback() {
        let models = catalogue();
        let b = Brief {
            fallback_model: Some("claude-haiku-4-5"),
            ..brief(&models, &[])
        };
        let routed = Routing {
            model: Some("deepseek-v4-flash".into()),
            provider: Some("opencode-go".into()),
            ..Routing::default()
        };
        assert_eq!(
            model_that_runs(&b, &routed)
                .map(WorkerModel::key)
                .as_deref(),
            Some("opencode-go:deepseek-v4-flash")
        );
        assert_eq!(
            model_that_runs(&b, &Routing::default())
                .map(WorkerModel::key)
                .as_deref(),
            Some("anthropic:claude-haiku-4-5")
        );
    }

    #[test]
    fn a_worktree_is_only_dropped_on_a_confident_read_only() {
        let models = catalogue();
        let worktree = |p: f64| {
            judge_all(
                &brief(&models, &[]),
                json!({"answers": {"needs_worktree": {"noul": p}}}),
            )
            .routing
            .worktree
        };
        assert_eq!(worktree(0.45), Some(true), "a coin flip keeps the worktree");
        assert_eq!(worktree(0.2), Some(true));
        assert_eq!(worktree(0.05), Some(false));
    }

    #[test]
    fn a_question_is_pending_while_its_asker_lives_and_nobody_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = FleetPaths::new(tmp.path());
        let question = |id: &str, pid: u32, deadline_ms: i64| ModelQuestion {
            id: id.into(),
            name: "add-auth".into(),
            brief: "add token refresh".into(),
            confidence: 0.3,
            threshold: 0.6,
            options: Vec::new(),
            fallback: None,
            asked_at: crate::util::now_iso(),
            pid,
            deadline_ms,
        };
        let me = std::process::id();
        let later = crate::util::now_ms() + 60_000;
        assert!(pending_questions(&paths).is_empty(), "no directory yet");
        post_question(&paths, &question("live", me, later)).unwrap();
        post_question(&paths, &question("expired", me, 1)).unwrap();
        post_question(&paths, &question("orphan", 0, later)).unwrap();
        let ids = |qs: Vec<ModelQuestion>| qs.into_iter().map(|q| q.id).collect::<Vec<_>>();
        assert_eq!(ids(pending_questions(&paths)), vec!["live"]);

        let answer = ModelAnswer {
            model: Some("anthropic:claude-opus-5".into()),
        };
        answer_question(&paths, "live", &answer).unwrap();
        assert!(
            pending_questions(&paths).is_empty(),
            "answered is not pending"
        );
        assert_eq!(read_answer(&paths, "live"), Some(answer));
        remove_question(&paths, "live");
        assert_eq!(read_answer(&paths, "live"), None);
    }

    #[tokio::test]
    async fn two_cycles_choose_the_model_then_its_thinking_level() {
        let (url, mut rx) = stub(vec![
            json!({"answers": {
                "model": {"type": "choice", "choice": "opencode-go:deepseek-v4-flash", "confidence": 0.82},
                "needs_worktree": {"type": "noul", "noul": 0.9},
                "parallel_safe": {"type": "noul", "noul": 0.8},
            }}),
            json!({"answers": {
                "thinking": {"type": "choice", "choice": "xhigh", "confidence": 0.7},
            }}),
        ])
        .await;
        let models = catalogue();
        let config = RoutingConfig {
            enabled: true,
            endpoint: Some(url),
            ..RoutingConfig::default()
        };
        let b = brief(&models, &[]);
        let judgment = judge(&b, &config, Some("sk-test"))
            .await
            .expect("a decision");
        assert_eq!(judgment.ask, None);
        let decision = judgment.routing;
        assert_eq!(decision.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(decision.provider.as_deref(), Some("opencode-go"));
        assert_eq!(decision.worktree, Some(true));
        let runs = model_that_runs(&b, &decision).unwrap();
        let thinking = choose_thinking(&b, runs, &config, "sk-test").await;
        assert_eq!(thinking, Some(("xhigh".to_string(), 0.7)));

        let (head, body) = rx.recv().await.unwrap();
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-test"),
            "the key rides as a bearer token: {head}"
        );
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["state"]["task"]["name"], "add-auth");
        assert!(sent["questions"]["model"]["criteria"]["anthropic:claude-opus-5"].is_string());
        let (_, body) = rx.recv().await.unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            sent["questions"]["thinking"]["criteria"]
                .as_object()
                .unwrap()
                .len(),
            3,
            "the chosen model's own three levels: {sent}"
        );
    }

    #[tokio::test]
    async fn a_model_with_one_level_or_none_is_not_asked_about_thinking() {
        let models = catalogue();
        let config = RoutingConfig {
            enabled: true,
            // nothing listening: asking at all would fail loudly here
            endpoint: Some("http://127.0.0.1:1/v1/systemone".to_string()),
            ..RoutingConfig::default()
        };
        let haiku = &models[4];
        assert_eq!(
            choose_thinking(&brief(&models, &[]), haiku, &config, "k").await,
            None
        );
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
        let judgment = judge(&brief(&models, &[]), &config, Some("sk-test"))
            .await
            .expect("routing was on, so the outcome is said");
        assert_eq!(judgment.routing.model, None);
        assert_eq!(judgment.ask, None, "nothing to ask when nothing was judged");
        assert!(
            judgment.routing.note.contains("did not answer"),
            "{}",
            judgment.routing.note
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
            judge(&brief(&models, &[]), &RoutingConfig::default(), Some("k"))
                .await
                .is_none(),
            "off by default"
        );
        assert!(judge(&brief(&models, &[]), &on, None).await.is_none());
        let declined = judge(&brief(&[], &[]), &on, Some("k")).await.unwrap();
        assert!(
            declined.routing.note.starts_with("not routed:"),
            "{}",
            declined.routing.note
        );
    }
}
