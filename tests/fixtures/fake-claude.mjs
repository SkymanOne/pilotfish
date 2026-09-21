#!/usr/bin/env node
// Scripted `claude -p --input-format stream-json --output-format stream-json` stand-in.
// Like the real thing it stays silent until a user message arrives, then emits system/init (after every one).
// Message prefixes drive behavior:
//   perm:<cmd>        → control_request can_use_tool (Bash) then "allowed:<cmd>" / "denied:<msg>"
//   ask:<q>|opt1|opt2 → AskUserQuestion request, then "answers:<json>"
//   slow:             → streams deltas for ~5s, honors an interrupt control request
//   fail:             → result with subtype error_during_execution
//   anything else     → deltas + assistant "echo: <text>" + result
// Env: FAKE_CLAUDE_ARGV_FILE (dump argv), FAKE_CLAUDE_STDIN_LOG (append every stdin line),
//      FAKE_CLAUDE_SESSION_ID, FAKE_CLAUDE_REQUIRE_INIT=1 (no permission prompts until an
//      `initialize` control request arrived; the tool then runs without asking).
import fs from "node:fs";
import { randomUUID } from "node:crypto";

const argv = process.argv.slice(2);
if (process.env.FAKE_CLAUDE_SETTINGS_FILE) {
  process.on("exit", () => {
    try { fs.writeFileSync(process.env.FAKE_CLAUDE_SETTINGS_FILE, JSON.stringify(settings)); } catch {}
  });
}
if (argv.includes("--version")) {
  process.stdout.write(`${process.env.FAKE_CLAUDE_VERSION || "2.1.251"} (Claude Code)\n`);
  process.exit(0);
}
if (process.env.FAKE_CLAUDE_ARGV_FILE) fs.writeFileSync(process.env.FAKE_CLAUDE_ARGV_FILE, JSON.stringify(argv));
const replay = argv.includes("--replay-user-messages");
const sessionId = process.env.FAKE_CLAUDE_SESSION_ID || randomUUID();
const send = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let initialized = false;
// How many `initialize` requests have arrived. The second one answers with an
// extra command, so a test can tell a fresh answer from a cached one.
let initCount = 0;
let settings = {};
let initSent = false;
let numTurns = 0;
let interrupted = false;
const pendingControl = new Map(); // request_id -> resolve(response)
const queue = [];
let busy = false;

// The real CLI emits system/init after every user message, not only the first.
function emitInit() {
  initSent = true;
  send({
    type: "system",
    subtype: "init",
    session_id: sessionId,
    cwd: process.cwd(),
    model: "fake-model",
    tools: ["Bash", "Read", "Glob", "Grep", "AskUserQuestion", "mcp__fleet__fleet_status"],
    mcp_servers: [{ name: "fleet", status: "connected" }],
    capabilities: ["interrupt_receipt_v1"],
    permissionMode: "default",
    claude_code_version: "0.0.0-fake",
    uuid: randomUUID(),
  });
}

function requestPermission(request) {
  const request_id = `perm_${randomUUID()}`;
  return new Promise((resolve) => {
    pendingControl.set(request_id, resolve);
    send({ type: "control_request", request_id, request });
  });
}

function assistantText(text) {
  const uuid = randomUUID();
  send({ type: "stream_event", event: { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }, parent_tool_use_id: null, session_id: sessionId });
  send({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "text_delta", text } }, parent_tool_use_id: null, session_id: sessionId });
  send({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text }], model: "fake-model" }, parent_tool_use_id: null, session_id: sessionId, uuid });
}

function result(extra = {}) {
  numTurns += 1;
  send({ type: "result", subtype: "success", is_error: false, result: extra.text ?? "", num_turns: numTurns, total_cost_usd: 0.001 * numTurns, duration_ms: 10, session_id: sessionId, ...extra.fields });
}

async function handleUser(text) {
  if (text.startsWith("perm:")) {
    const command = text.slice(5);
    if (process.env.FAKE_CLAUDE_REQUIRE_INIT === "1" && !initialized) {
      assistantText(`ran-without-prompt:${command}`);
      result({ text: `ran-without-prompt:${command}` });
      return;
    }
    const tool_use_id = `toolu_${randomUUID().slice(0, 8)}`;
    send({ type: "assistant", message: { role: "assistant", content: [{ type: "tool_use", id: tool_use_id, name: "Bash", input: { command } }] }, parent_tool_use_id: null, session_id: sessionId, uuid: randomUUID() });
    const response = await requestPermission({
      subtype: "can_use_tool",
      tool_name: "Bash",
      input: { command },
      tool_use_id,
      title: `Run ${command}`,
      display_name: "Bash",
      description: "Run a shell command",
      permission_suggestions: [{ type: "addRules", rules: [{ toolName: "Bash", ruleContent: `${command.split(" ")[0]} *` }], behavior: "allow", destination: "session" }],
    });
    const outcome = response?.behavior === "allow" ? `allowed:${response.updatedInput?.command ?? command}` : `denied:${response?.message ?? "?"}`;
    send({ type: "user", message: { role: "user", content: [{ type: "tool_result", tool_use_id, content: outcome, is_error: response?.behavior !== "allow" }] }, parent_tool_use_id: null, session_id: sessionId, uuid: randomUUID() });
    assistantText(outcome);
    result({ text: outcome });
    return;
  }
  if (text.startsWith("ask:")) {
    const [question, ...options] = text.slice(4).split("|");
    const tool_use_id = `toolu_${randomUUID().slice(0, 8)}`;
    const input = { questions: [{ question, header: "Choice", options: options.map((label) => ({ label, description: `pick ${label}` })), multiSelect: false }] };
    const response = await requestPermission({ subtype: "can_use_tool", tool_name: "AskUserQuestion", input, tool_use_id, title: "Question", display_name: "AskUserQuestion" });
    const outcome = response?.behavior === "allow" ? `answers:${JSON.stringify(response.updatedInput?.answers ?? null)}` : `denied:${response?.message ?? "?"}`;
    assistantText(outcome);
    result({ text: outcome });
    return;
  }
  if (text.startsWith("slow:")) {
    interrupted = false;
    send({ type: "stream_event", event: { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }, parent_tool_use_id: null, session_id: sessionId });
    for (let i = 0; i < 25 && !interrupted; i++) {
      send({ type: "stream_event", event: { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: `tick${i} ` } }, parent_tool_use_id: null, session_id: sessionId });
      await sleep(200);
    }
    const text2 = interrupted ? "interrupted" : "slow-done";
    send({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text: text2 }] }, parent_tool_use_id: null, session_id: sessionId, uuid: randomUUID() });
    result({ text: text2 });
    return;
  }
  if (text.startsWith("fail:")) {
    numTurns += 1;
    send({ type: "result", subtype: "error_during_execution", is_error: true, errors: ["boom"], num_turns: numTurns, total_cost_usd: 0, session_id: sessionId });
    return;
  }
  assistantText(`echo: ${text}`);
  result({ text: `echo: ${text}` });
}

async function pump() {
  if (busy) return;
  busy = true;
  while (queue.length > 0) {
    const text = queue.shift();
    await handleUser(text);
  }
  busy = false;
}

let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk.toString();
  let idx;
  while ((idx = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, idx);
    buffer = buffer.slice(idx + 1);
    if (!line.trim()) continue;
    if (process.env.FAKE_CLAUDE_STDIN_LOG) fs.appendFileSync(process.env.FAKE_CLAUDE_STDIN_LOG, line + "\n");
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (msg.type === "user") {
      emitInit();
      const content = msg.message?.content;
      const text = typeof content === "string" ? content : Array.isArray(content) ? content.map((b) => b.text ?? "").join("") : "";
      if (replay) send({ type: "user", message: { role: "user", content: text }, parent_tool_use_id: null, session_id: sessionId, uuid: randomUUID() });
      queue.push(text);
      void pump();
    } else if (msg.type === "control_response") {
      const rid = msg.response?.request_id;
      const resolve = pendingControl.get(rid);
      if (resolve) { pendingControl.delete(rid); resolve(msg.response?.response ?? null); }
    } else if (msg.type === "control_request") {
      const sub = msg.request?.subtype;
      if (sub === "initialize") { initialized = true; initCount += 1; }
      if (sub === "interrupt") interrupted = true;
      if (sub === "apply_flag_settings") settings = { ...settings, ...(msg.request.settings ?? {}) };
      if (sub === "apply_flag_settings" && process.env.FAKE_CLAUDE_NO_FLAG_SETTINGS === "1") {
        send({ type: "control_response", response: { subtype: "error", request_id: msg.request_id, error: "unknown subtype" } });
        continue;
      }
      const response =
        sub === "interrupt"
          ? { still_queued: [] }
          : sub === "initialize"
            ? {
                commands: [
                  { name: "model", description: "Set the model", argumentHint: "<model>" },
                  { name: "usage", description: "Show plan usage", aliases: ["cost"] },
                  { name: "research", description: "Research a topic", argumentHint: "<topic>" },
                  // stands in for a skill installed after the handshake
                  ...(initCount > 1 ? [{ name: "late-skill", description: "Installed later" }] : []),
                ],
                agents: [],
                models: [],
                output_style: "default",
                available_output_styles: [],
                account: {},
              }
            : {};
      send({ type: "control_response", response: { subtype: "success", request_id: msg.request_id, response } });
    }
  }
});
process.stdin.on("end", () => setTimeout(() => process.exit(0), 20));
