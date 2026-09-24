/**
 * pilotfish worker protocol (pi extension).
 *
 * When pi runs as a fleet worker (PILOTFISH_RUN + PILOTFISH_DIR set by `pilotfish`'s
 * monitor) this extension:
 *  - appends the report protocol to the system prompt (`before_agent_start`), so
 *    report-writing does not depend on the model discovering the skill;
 *  - registers `fleet_ask` (block until the orchestrator answers through the run's
 *    inbox) and `fleet_progress` (one-line milestones), which write the run's
 *    outbox `runs/<id>/outbox.jsonl`.
 *
 * Mailbox helpers live in this file on purpose: pi loads the extension straight
 * from source, and a single file needs no relative-import resolution.
 * Idempotent: safe if loaded both via `pi install` and `--extension`.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import fs from "node:fs";
import path from "node:path";
import { Type } from "typebox";

export const FLEET_PROTOCOL_MARKER = "## Fleet worker protocol";

export const REPORT_TEMPLATE = `# Fleet Report: <run name>

## Status
done | blocked | failed

## Summary
(3-8 sentences: what was accomplished and the outcome)

## What I did
(numbered steps actually taken)

## Files changed
(path: one-line reason, from your actual edits)

## Verification
(command run → result, for each check performed)

## Decisions & assumptions
(any choice made without explicit instruction)

## Steering received
(mid-run course corrections you were given and how you handled them; "none" if none)

## Open questions for orchestrator
(things you could not resolve; empty if none, REQUIRED if Status: blocked)

## Suggested next step
(one concrete next action for the orchestrator)`;

export interface FleetEnv {
  PILOTFISH_RUN?: string;
  PILOTFISH_DIR?: string;
  PILOTFISH_ASK_POLL_MS?: string;
  PILOTFISH_ASK_TIMEOUT_MS?: string;
}

export function buildFleetProtocol(env: FleetEnv, cwd: string): string | null {
  const runId = env.PILOTFISH_RUN;
  const fleetDir = env.PILOTFISH_DIR;
  if (!runId || !fleetDir) return null;
  const reportPath = `${fleetDir}/runs/${runId}/report.md`;
  return [
    FLEET_PROTOCOL_MARKER,
    "",
    `You are a fleet worker. Run id: \`${runId}\`. Working directory: \`${cwd}\`. The orchestrator reads your results from files and from the \`fleet_ask\` / \`fleet_progress\` tools, not from this conversation.`,
    "",
    "Rules:",
    `1. Before you finish (before your final assistant turn), write your final report to \`${reportPath}\` using EXACTLY the template below — keep every heading, in order.`,
    "2. For long tasks, call `fleet_progress` with a one-line note at each milestone. The orchestrator sees it live.",
    "3. Stay scoped to your task brief. Do not touch files outside your working directory. Never run `git merge`, never modify the parent checkout, never push.",
    '4. If you receive steering messages mid-run (course corrections from the orchestrator or from the user\'s console), incorporate them immediately. Your final report MUST reflect the adjusted direction: list every steering message under "Steering received" and keep Status/Verification consistent with the work as finally done.',
    '5. When you are blocked on a decision or a missing input, call `fleet_ask` (question, optional options and context) instead of guessing. It waits for the orchestrator\'s answer and returns it. If it reports that no answer arrived in time, proceed on your best judgment and record the choice under "Decisions & assumptions".',
    '6. If you cannot proceed even after asking, set `Status: blocked` and fill "Open questions for orchestrator".',
    "",
    "Report template:",
    "",
    "```markdown",
    REPORT_TEMPLATE,
    "```",
  ].join("\n");
}

// ---------------------------------------------------------------------------
// Mailbox helpers (mirror of src/fleet/envelope.rs; keep the shapes aligned)

export type QuestionResolution = "answered" | "timeout" | "aborted";

export interface OutboxLine {
  id: string;
  ts: string;
  from: string;
  to: string;
  type: "question" | "progress" | "question_resolved";
  payload: Record<string, unknown>;
}

export function newId(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
}

export function nowIso(): string {
  return new Date().toISOString();
}

export function runDir(fleetDir: string, runId: string): string {
  return path.join(fleetDir, "runs", runId);
}

export function outboxPath(fleetDir: string, runId: string): string {
  return path.join(runDir(fleetDir, runId), "outbox.jsonl");
}

export function inboxPath(fleetDir: string, runId: string): string {
  return path.join(runDir(fleetDir, runId), "inbox.jsonl");
}

export function appendOutbox(
  fleetDir: string,
  runId: string,
  line: Omit<OutboxLine, "id" | "ts" | "from" | "to"> & { id?: string },
): OutboxLine {
  const full: OutboxLine = {
    id: line.id ?? newId("m"),
    ts: nowIso(),
    from: `worker:${runId}`,
    to: "fleet",
    type: line.type,
    payload: line.payload,
  };
  const p = outboxPath(fleetDir, runId);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.appendFileSync(p, JSON.stringify(full) + "\n");
  return full;
}

export function fileSize(p: string): number {
  try {
    return fs.statSync(p).size;
  } catch {
    return 0;
  }
}

/** Complete lines appended after byte `offset`; a partial trailing line is left for next time. */
export function readNewLines(
  p: string,
  offset: number,
): { lines: string[]; offset: number } {
  const size = fileSize(p);
  if (size <= offset) return { lines: [], offset };
  const buf = Buffer.alloc(size - offset);
  const fd = fs.openSync(p, "r");
  try {
    fs.readSync(fd, buf, 0, buf.length, offset);
  } finally {
    fs.closeSync(fd);
  }
  const lastNl = buf.lastIndexOf(0x0a);
  if (lastNl === -1) return { lines: [], offset };
  const lines = buf
    .subarray(0, lastNl + 1)
    .toString("utf8")
    .split("\n")
    .map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l))
    .filter((l) => l.length > 0);
  return { lines, offset: offset + lastNl + 1 };
}

export interface Answer {
  message: string;
  source: string;
}

/**
 * The `answer` envelope for `questionId` among `lines`, if any. The inbox
 * carries envelopes: `{"id","ts","from","to","type":"answer",
 * "payload":{"message","questionId"}}`.
 */
export function findAnswer(lines: string[], questionId: string): Answer | null {
  for (const line of lines) {
    let msg: {
      type?: unknown;
      from?: unknown;
      payload?: { questionId?: unknown; message?: unknown };
    };
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    const payload = msg?.payload;
    if (
      msg?.type === "answer" &&
      payload &&
      payload.questionId === questionId &&
      typeof payload.message === "string"
    ) {
      return {
        message: payload.message,
        source: typeof msg.from === "string" ? msg.from : "unknown",
      };
    }
  }
  return null;
}

const sleep = (ms: number): Promise<void> =>
  new Promise((r) => setTimeout(r, ms));

export interface AskParams {
  question: string;
  options?: string[];
  context?: string;
}

export interface AskResult {
  questionId: string;
  how: QuestionResolution;
  text: string;
  answer: Answer | null;
}

export const DEFAULT_ASK_POLL_MS = 500;
export const DEFAULT_ASK_TIMEOUT_MS = 10 * 60_000;

/** Post a question to the outbox and wait for a matching `answer` in the inbox. */
export async function askOrchestrator(
  env: FleetEnv,
  params: AskParams,
  opts: { signal?: AbortSignal; onWaiting?: (questionId: string) => void } = {},
): Promise<AskResult> {
  const fleetDir = env.PILOTFISH_DIR!;
  const runId = env.PILOTFISH_RUN!;
  const pollMs =
    Number(env.PILOTFISH_ASK_POLL_MS) > 0
      ? Number(env.PILOTFISH_ASK_POLL_MS)
      : DEFAULT_ASK_POLL_MS;
  const timeoutMs =
    Number(env.PILOTFISH_ASK_TIMEOUT_MS) > 0
      ? Number(env.PILOTFISH_ASK_TIMEOUT_MS)
      : DEFAULT_ASK_TIMEOUT_MS;
  const inbox = inboxPath(fleetDir, runId);
  // Answers can only arrive after the question is posted, so start reading at the current end.
  let offset = fileSize(inbox);
  const questionId = newId("q");
  appendOutbox(fleetDir, runId, {
    id: questionId,
    type: "question",
    payload: {
      question: params.question,
      options:
        Array.isArray(params.options) && params.options.length > 0
          ? params.options
          : null,
      context: params.context ?? null,
    },
  });
  opts.onWaiting?.(questionId);
  const deadline = Date.now() + timeoutMs;
  let how: QuestionResolution = "timeout";
  let answer: Answer | null = null;
  for (;;) {
    const read = readNewLines(inbox, offset);
    offset = read.offset;
    answer = findAnswer(read.lines, questionId);
    if (answer) {
      how = "answered";
      break;
    }
    if (opts.signal?.aborted) {
      how = "aborted";
      break;
    }
    if (Date.now() >= deadline) break;
    await sleep(Math.min(pollMs, Math.max(1, deadline - Date.now())));
  }
  appendOutbox(fleetDir, runId, {
    type: "question_resolved",
    payload: { questionId, how },
  });
  const minutes = Math.round(timeoutMs / 60_000);
  const text =
    how === "answered"
      ? `Answer from ${answer!.source}: ${answer!.message}`
      : how === "aborted"
        ? "The question was aborted before an answer arrived."
        : `No answer arrived within ${minutes} minute${minutes === 1 ? "" : "s"}. Proceed with your best judgment and record the decision under "Decisions & assumptions" in your report.`;
  return { questionId, how, text, answer };
}

/** Append a milestone to the outbox. */
export function noteProgress(env: FleetEnv, message: string): OutboxLine {
  const fleetDir = env.PILOTFISH_DIR!;
  const runId = env.PILOTFISH_RUN!;
  return appendOutbox(fleetDir, runId, {
    type: "progress",
    payload: { message },
  });
}

// ---------------------------------------------------------------------------
// Tool definitions

const ASK_PARAMS = Type.Object({
  question: Type.String({
    description:
      "The decision or input you need, phrased so someone can answer it in one line",
  }),
  options: Type.Optional(
    Type.Array(Type.String(), {
      description:
        "Concrete choices, if the question has a small set of answers",
    }),
  ),
  context: Type.Optional(
    Type.String({
      description:
        "One or two sentences of context the orchestrator needs to decide",
    }),
  ),
});

const PROGRESS_PARAMS = Type.Object({
  message: Type.String({ description: "One line: the milestone just reached" }),
});

export function fleetTools(env: FleetEnv) {
  return [
    {
      name: "fleet_ask",
      label: "Ask orchestrator",
      description:
        "Ask the fleet orchestrator a question and wait for the answer. Use it when you are blocked on a decision or a missing input instead of guessing. Returns the answer, or tells you to proceed on your own judgment when none arrives in time.",
      promptSnippet:
        "Ask the orchestrator when blocked on a decision or missing input",
      parameters: ASK_PARAMS,
      async execute(
        _toolCallId: string,
        params: AskParams,
        signal: AbortSignal | undefined,
        onUpdate:
          | ((partial: {
              content: { type: "text"; text: string }[];
              details: unknown;
            }) => void)
          | undefined,
      ) {
        const result = await askOrchestrator(env, params, {
          signal,
          onWaiting: (questionId) =>
            onUpdate?.({
              content: [
                { type: "text", text: "waiting for the orchestrator…" },
              ],
              details: { questionId },
            }),
        });
        return {
          content: [{ type: "text" as const, text: result.text }],
          details: {
            questionId: result.questionId,
            how: result.how,
            answer: result.answer,
          },
        };
      },
    },
    {
      name: "fleet_progress",
      label: "Report progress",
      description: "Record a one-line milestone for the fleet orchestrator.",
      promptSnippet: "Record a one-line milestone for the orchestrator",
      parameters: PROGRESS_PARAMS,
      async execute(_toolCallId: string, params: { message: string }) {
        const line = noteProgress(env, params.message);
        return {
          content: [{ type: "text" as const, text: "noted" }],
          details: { id: line.id },
        };
      },
    },
  ];
}

export default function fleetWorker(pi: ExtensionAPI): void {
  pi.on("before_agent_start", async (event, ctx) => {
    const block = buildFleetProtocol(process.env, ctx.cwd);
    if (!block) return;
    if (event.systemPrompt.includes(FLEET_PROTOCOL_MARKER)) return;
    return { systemPrompt: `${event.systemPrompt}\n\n${block}` };
  });
  if (process.env.PILOTFISH_RUN && process.env.PILOTFISH_DIR) {
    for (const tool of fleetTools(process.env)) pi.registerTool(tool as any);
  }
}
