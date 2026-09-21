#!/usr/bin/env node
// Scripted `pi --mode rpc` replacement for hermetic parl monitor tests —
// adapted from the TypeScript tree's tests/fixtures/fake-pi.mjs for the PARL
// layout (.parl, runs/<id>/report.md, inbox.jsonl envelopes).
// Env: FAKE_PI_DELAY_MS   settle delay after the work turn (default 300)
//      FAKE_PI_WRITE_HELLO=1  write hello.txt in cwd
//      FAKE_PI_ARGV_FILE  if set, dump process.argv.slice(2) there as JSON
//      FAKE_PI_ASK=1      call fleet_ask mid-turn: post a question to outbox.jsonl and
//                         wait for an `answer` envelope in inbox.jsonl (FAKE_PI_ASK_TIMEOUT_MS, default 15000)
//      FAKE_PI_PROGRESS=1 post a progress line to outbox.jsonl before the tool call
//      FAKE_PI_DIALOG=1   open an extension dialog mid-turn and wait for the
//                         extension_ui_response on stdin (FAKE_PI_DIALOG_TIMEOUT ms, optional)
//      FAKE_PI_NOTIFY=1   send fire-and-forget extension_ui_requests, need no reply
//      FAKE_PI_ACCEPTS_MODEL=1  set_model succeeds and switches the reported model
//      FAKE_PI_MODELS     ids offered by get_available_models (default "glm-5.3,glm-5.3-flash")
//      FAKE_PI_EXIT_DELAY_MS: linger after stdin closes, like real pi's shutdown() teardown.
import fsSync from "node:fs";
import path from "node:path";

// the real pi lists its models this way, and spawn checks a model before using it
if (process.argv.includes("--list-models")) {
  const models = (process.env.FAKE_PI_MODELS ?? "glm-5.3,glm-5.3-flash,claude-sonnet-5").split(",");
  process.stdout.write("provider           model                context\n");
  for (const m of models) process.stdout.write(`fake               ${m}                1M\n`);
  process.exit(0);
}

if (process.env.FAKE_PI_ARGV_FILE) {
  fsSync.writeFileSync(process.env.FAKE_PI_ARGV_FILE, JSON.stringify(process.argv.slice(2)));
}

const send = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");
const steers = [];
let taskStarted = false;
let commandsAsked = 0;
let answerText = null;
let dialogResult = null;
let model = {
  id: process.env.FAKE_PI_MODEL_ID || "fake/model-1",
  provider: process.env.FAKE_PI_PROVIDER || "fakeprovider",
};
let thinkingLevel = process.env.FAKE_PI_THINKING || "medium";
// Every level pi knows, and the ones this "model" actually has. Real pi maps
// the rest to null and then answers `success: true` to a set it will not
// honour, which is what FAKE_PI_THINKING_LEVELS reproduces.
const ALL_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const SUPPORTED = (process.env.FAKE_PI_THINKING_LEVELS || ALL_LEVELS.join(","))
  .split(",")
  .map((l) => l.trim())
  .filter(Boolean);
const thinkingLevelMap = Object.fromEntries(
  ALL_LEVELS.map((l) => [l, SUPPORTED.includes(l) ? l : null]),
);
const fleetDir = process.env.PARL_DIR;
const runId = process.env.PARL_RUN;
const delay = Number(process.env.FAKE_PI_DELAY_MS || 300);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// --- mailbox (same envelope shape as src/fleet/envelope.rs) ---
const runDir = () => path.join(fleetDir, "runs", runId);
function appendOutbox(line) {
  const full = {
    id: line.id ?? `m_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`,
    ts: new Date().toISOString(),
    from: `worker:${runId}`,
    to: "fleet",
    type: line.type,
    payload: line.payload,
  };
  fsSync.mkdirSync(runDir(), { recursive: true });
  fsSync.appendFileSync(path.join(runDir(), "outbox.jsonl"), JSON.stringify(full) + "\n");
  return full;
}
function readAnswer(questionId, offset) {
  const p = path.join(runDir(), "inbox.jsonl");
  let size = 0;
  try { size = fsSync.statSync(p).size; } catch { return { answer: null, offset }; }
  if (size <= offset) return { answer: null, offset };
  const buf = Buffer.alloc(size - offset);
  const fd = fsSync.openSync(p, "r");
  fsSync.readSync(fd, buf, 0, buf.length, offset);
  fsSync.closeSync(fd);
  const lastNl = buf.lastIndexOf(0x0a);
  if (lastNl === -1) return { answer: null, offset };
  for (const line of buf.subarray(0, lastNl + 1).toString("utf8").split("\n")) {
    if (!line) continue;
    try {
      const env = JSON.parse(line);
      if (env.type === "answer" && env.payload && env.payload.questionId === questionId) {
        return { answer: { message: env.payload.message, source: env.from ?? "unknown" }, offset: offset + lastNl + 1 };
      }
    } catch { /* skip */ }
  }
  return { answer: null, offset: offset + lastNl + 1 };
}

function writeReport() {
  if (!fleetDir || !runId) return;
  fsSync.mkdirSync(runDir(), { recursive: true });
  const steeringSection = steers.length > 0 ? steers.map((s) => `- ${s.message}`).join("\n") : "none";
  const decisions = [
    answerText ? `Answer received: ${answerText}` : null,
    dialogResult ? `dialog: ${JSON.stringify(dialogResult.reply)} after ${dialogResult.elapsed}ms` : null,
  ].filter(Boolean).join("; ") || 'Greeting text chosen as "hi".';
  fsSync.writeFileSync(
    path.join(runDir(), "report.md"),
    `# Fleet Report: ${runId}\n\n## Status\ndone\n\n## Summary\nCreated hello.txt with greeting content as briefed.\n\n## What I did\n1. Created hello.txt\n2. Verified content\n\n## Files changed\nhello.txt: new file with greeting\n\n## Verification\ncat hello.txt -> hi\n\n## Decisions & assumptions\n${decisions}\n\n## Steering received\n${steeringSection}\n\n## Open questions for orchestrator\n(none)\n\n## Suggested next step\nMerge the worker branch.\n`,
  );
}

function doWork() {
  if (process.env.FAKE_PI_WRITE_HELLO === "1") {
    fsSync.writeFileSync(path.join(process.cwd(), "hello.txt"), "hi\n");
  }
}

async function openDialog() {
  const request = {
    type: "extension_ui_request",
    id: "dlg_fake_1",
    method: process.env.FAKE_PI_DIALOG_METHOD || "select",
    title: "Pick one",
    options: ["a", "b"],
  };
  const timeout = Number(process.env.FAKE_PI_DIALOG_TIMEOUT || 0);
  if (timeout > 0) request.timeout = timeout;
  send({ type: "tool_execution_start", toolCallId: "c3", toolName: "ui_tool", args: {} });
  send(request);
  const started = Date.now();
  // pi's agent side auto-resolves with undefined when its own timeout lapses;
  // the monitor must cancel before that.
  const reply = await Promise.race([
    new Promise((resolve) => dialogWaiters.set(request.id, resolve)),
    sleep(timeout > 0 ? timeout + 500 : 60_000).then(() => "(pi timeout)"),
  ]);
  dialogResult = { reply, elapsed: Date.now() - started };
  send({ type: "tool_execution_end", toolCallId: "c3", toolName: "ui_tool", result: { content: [{ type: "text", text: JSON.stringify(reply) }] } });
}

async function askQuestion() {
  const questionId = `q_fake_${Date.now().toString(36)}`;
  const args = { question: "bcrypt or argon2?", options: ["bcrypt", "argon2"] };
  send({ type: "tool_execution_start", toolCallId: "c2", toolName: "fleet_ask", args });
  let offset = 0;
  try { offset = fsSync.statSync(path.join(runDir(), "inbox.jsonl")).size; } catch { offset = 0; }
  appendOutbox({ id: questionId, type: "question", payload: { question: args.question, options: args.options, context: null } });
  const deadline = Date.now() + Number(process.env.FAKE_PI_ASK_TIMEOUT_MS || 15000);
  let answer = null;
  while (!aborted && Date.now() < deadline) {
    const r = readAnswer(questionId, offset);
    offset = r.offset;
    if (r.answer) { answer = r.answer; break; }
    await sleep(100);
  }
  const how = answer ? "answered" : aborted ? "aborted" : "timeout";
  appendOutbox({ type: "question_resolved", payload: { questionId, how } });
  const text = answer ? `Answer from ${answer.source}: ${answer.message}` : `No answer (${how})`;
  send({ type: "tool_execution_end", toolCallId: "c2", toolName: "fleet_ask", result: { content: [{ type: "text", text }] } });
  if (answer) answerText = answer.message;
}

async function runTask() {
  send({ type: "agent_start" });
  send({ type: "turn_start" });
  // FAKE_PI_THINK_MS: reason for a while first, like a model with thinking on
  const thinkMs = Number(process.env.FAKE_PI_THINK_MS || 0);
  if (thinkMs > 0) {
    send({ type: "message_update", assistantMessageEvent: { type: "thinking_start", contentIndex: 0 } });
    send({ type: "message_update", assistantMessageEvent: { type: "thinking_delta", contentIndex: 0, delta: "hmm" } });
    await sleep(thinkMs);
    send({ type: "message_update", assistantMessageEvent: { type: "thinking_end", contentIndex: 0, content: "hmm" } });
  }
  send({ type: "message_update", assistantMessageEvent: { type: "text_start", contentIndex: 0 } });
  send({ type: "message_update", assistantMessageEvent: { type: "text_delta", contentIndex: 0, delta: "Working: " } });
  if (process.env.FAKE_PI_PROGRESS === "1" && fleetDir && runId) {
    appendOutbox({ type: "progress", payload: { message: "starting the work" } });
  }
  send({ type: "tool_execution_start", toolCallId: "c1", toolName: "bash", args: { command: "echo hi" } });
  doWork();
  send({ type: "tool_execution_end", toolCallId: "c1", toolName: "bash", result: { content: [{ type: "text", text: "hi\n" }] } });
  if (process.env.FAKE_PI_NOTIFY === "1") {
    send({ type: "extension_ui_request", id: "n1", method: "notify", message: "heads up", notifyType: "info" });
    send({ type: "extension_ui_request", id: "n2", method: "setTitle", title: "pi - working" });
  }
  if (process.env.FAKE_PI_DIALOG === "1" && !aborted) await openDialog();
  if (process.env.FAKE_PI_ASK === "1" && fleetDir && runId) await askQuestion();
  send({ type: "message_update", assistantMessageEvent: { type: "text_delta", contentIndex: 0, delta: "wrote hello.txt" } });
  send({ type: "message_update", assistantMessageEvent: { type: "text_end", contentIndex: 0, content: "Working: wrote hello.txt" } });
  send({ type: "turn_end", message: { role: "assistant" } });
  writeReport();
  setTimeout(() => {
    if (aborted) return;
    settle();
  }, delay);
}

let aborted = false;
let settled = false;
function settle() {
  if (settled) return;
  settled = true;
  writeReport();
  send({ type: "agent_end", willRetry: false });
  send({ type: "agent_settled" });
}

const dialogWaiters = new Map();
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk.toString();
  let idx;
  while ((idx = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.type === "extension_ui_response") {
      const waiter = dialogWaiters.get(msg.id);
      if (waiter) {
        dialogWaiters.delete(msg.id);
        waiter(msg);
      }
    } else if (msg.type === "prompt" && !taskStarted) {
      taskStarted = true;
      send({ id: msg.id, type: "response", command: "prompt", success: true });
      void runTask();
    } else if (msg.type === "prompt") {
      // a queued prompt (the monitor forwards `command` envelopes this way)
      steers.push({ message: msg.message });
      send({ id: msg.id, type: "response", command: "prompt", success: true });
      send({ type: "message_update", assistantMessageEvent: { type: "text_delta", contentIndex: 1, delta: `[prompt ack: ${msg.message}]` } });
    } else if (msg.type === "steer") {
      steers.push({ message: msg.message });
      send({ type: "response", command: "steer", success: true });
      send({ type: "message_update", assistantMessageEvent: { type: "text_delta", contentIndex: 1, delta: `[steer ack: ${msg.message}]` } });
    } else if (msg.type === "follow_up") {
      send({ type: "response", command: "follow_up", success: true });
      send({ type: "message_update", assistantMessageEvent: { type: "text_delta", contentIndex: 1, delta: "[followup ack]" } });
    } else if (msg.type === "abort") {
      aborted = true;
      send({ type: "response", command: "abort", success: true });
      settle();
    } else if (msg.type === "get_state") {
      send({
        id: msg.id,
        type: "response",
        command: "get_state",
        success: true,
        data: {
          model: {
            id: model.id,
            name: "Fake Model",
            provider: model.provider,
            thinkingLevelMap,
          },
          thinkingLevel,
          isStreaming: false,
          sessionId: "fake-session",
        },
      });
    } else if (msg.type === "set_thinking_level") {
      if (ALL_LEVELS.includes(msg.level)) {
        // a level this model lacks is accepted and then ignored, exactly as
        // pi does for `max` on deepseek-v4-flash
        if (SUPPORTED.includes(msg.level)) thinkingLevel = msg.level;
        send({ id: msg.id, type: "response", command: "set_thinking_level", success: true });
      } else {
        send({ id: msg.id, type: "response", command: "set_thinking_level", success: false, error: `unknown level: ${msg.level}` });
      }
    } else if (msg.type === "set_model") {
      if (process.env.FAKE_PI_ACCEPTS_MODEL === "1") {
        model = { id: msg.modelId, provider: msg.provider ?? model.provider };
        send({ id: msg.id, type: "response", command: "set_model", success: true, data: { ...model, name: "Fake Model" } });
      } else {
        send({ id: msg.id, type: "response", command: "set_model", success: false, error: `Model "${msg.modelId}" is not a recognized model id.` });
      }
    } else if (msg.type === "get_available_models") {
      const ids = (process.env.FAKE_PI_MODELS ?? "glm-5.3,glm-5.3-flash").split(",");
      send({
        id: msg.id,
        type: "response",
        command: "get_available_models",
        success: true,
        data: { models: ids.map((id) => ({ id, name: id, provider: process.env.FAKE_PI_PROVIDER || "fakeprovider" })) },
      });
    } else if (msg.type === "get_commands") {
      // The second and later answers carry one more command, so a test can
      // tell a fresh answer from the one cached at boot.
      commandsAsked += 1;
      send({
        id: msg.id,
        type: "response",
        command: "get_commands",
        success: true,
        data: {
          commands: [
            { name: "skill:fleet-worker-report", description: "How to write the fleet report", source: "skill" },
            { name: "compact-notes", description: "Summarize the session", source: "prompt" },
            { name: "session-name", description: "Set the session name", source: "extension" },
            ...(commandsAsked > 1 ? [{ name: "late-skill", description: "Installed later", source: "skill" }] : []),
          ],
        },
      });
    } else if (msg.type === "get_last_assistant_text") {
      send({ id: msg.id, type: "response", command: "get_last_assistant_text", success: true, data: { text: "Working: wrote hello.txt" } });
    }
  }
});
process.stdin.on("end", () => setTimeout(() => process.exit(0), Number(process.env.FAKE_PI_EXIT_DELAY_MS || 0)));
