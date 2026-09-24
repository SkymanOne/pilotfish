#!/usr/bin/env node
// e2e-only pi stand-in: real pi writes JSONL session files into the
// --session-dir it is given; the scripted fake (fake-pi-pilotfish.mjs) does not.
// This wrapper materializes one session file the way pi would, then
// delegates to fake-pi-pilotfish.mjs with the same argv, env and stdio, exiting
// with its code — so `pilotfish spawn` end-to-end tests see the documented
// `runs/<id>/session/` layout without touching the shared fixture.
import { spawn as spawnChild } from "node:child_process";
import fsSync from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const argv = process.argv.slice(2);
const at = argv.indexOf("--session-dir");
if (at !== -1 && argv[at + 1]) {
  const dir = argv[at + 1];
  fsSync.mkdirSync(dir, { recursive: true });
  fsSync.writeFileSync(
    path.join(dir, "fake-session.jsonl"),
    JSON.stringify({ type: "session", id: "fake-session", version: 1 }) + "\n",
  );
}

const target = path.join(path.dirname(fileURLToPath(import.meta.url)), "fake-pi-pilotfish.mjs");
const child = spawnChild(process.execPath, [target, ...argv], { stdio: "inherit" });
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 1);
});
child.on("error", () => process.exit(1));
