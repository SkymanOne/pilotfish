# Notes for agents

Working notes for whoever builds on this repository, human or agent. Keep it factual and short, and update it when you own a step.

## What this is

`pilotfish` is a terminal app that runs a fleet of headless [pi](https://github.com/earendil-works/pi-mono) coding agents with Claude Code as the orchestrator. You talk to the orchestrator, it spawns pi workers into their own git worktrees, and they report back through files under the state directory. Every agent is owned by a detached monitor that writes what happens to files; the console only reads and writes those files.

The Rust rewrite of the original TypeScript implementation (with `ratatui`) is the only implementation; the TypeScript tree was deleted at cutover on 2026-08-30. Repository, crate, binary and state directory share the name. It was `parl` until 2026-09-24, when the crate name turned out to be taken: everything moved to `pilotfish` (the GitHub repository included, which redirects from the old URL). Nothing migrates — as with `.pi-fleet`, an old `.parl/` or `~/.parl/` is simply ignored, `PARL_*` variables are no longer read, and a TypeSafe key stored under the keychain service `parl` has to be set again. The MCP server name stays `fleet`, so its tools stay `mcp__fleet__*`.

The user-facing docs are [README.md](README.md) and [docs/](docs/) (getting started, the console, the CLI). Keep product behaviour documented there and the contracts here; neither should repeat the other.

## Architecture

Single crate `pilotfish`, lib + bin. The library is the contract, and the binary only parses and dispatches.

| Module | Owns |
| --- | --- |
| `src/main.rs` | clap parsing and dispatch to every subcommand. The only file that touches every module. |
| `src/cli.rs` | the clap `Parser`/`Subcommand` definitions and the `ExitCode` enum (0 ok, 1 refusal/error, 2 no report, 3 wait timeout, 4 run ended stopped/error/dead, 5 merge conflict). The ops signatures `main.rs` calls are the contract between the CLI surface and the operation layer. |
| `src/util.rs` | ids, RFC3339 timestamps with milliseconds, atomic JSON writes (tmp file + fsync + rename), JSONL framing (`split_json_lines`), `read_new_lines` offsets, `sanitize_name`, `short_uuid`. Run ids are `<name>-<short-uuid>`, branches `pilotfish/<name>-<short7>`; the legacy `<name>-<14-digit>` forms still shorten and resolve. |
| `src/paths.rs` | the `.pilotfish` layout as `FleetPaths`, the `STATE_DIR_NAME`/`ENV_PREFIX`/`BIN_NAME` constants (every env var name derives from `ENV_PREFIX`), `ensure()` (creates the layout and gitignores it), the user dir `~/.pilotfish` (`$PILOTFISH_HOME`-overridable) and `UserConfig` (`~/.pilotfish/config.toml`). |
| `src/git.rs` | thin wrapper over the git CLI: `git_raw` real-exit-code execution, repo root discovery, worktree add/remove, branch delete, diff against a base commit, merge with conflict detection and `--abort`, dirty/merged checks. |
| `src/fleet/run.rs` | `RunState` (stored as `runs/<id>/run.json`, camelCase on disk, serde tolerant; the `uuid` field is the run's identity), `RunStatus`, derived status/view (30 s starting grace, `kill(pid,0)` liveness with EPERM-alive), `find_run` (exact id → uuid → alias → legacy `<name>-<14-digit>` form; several live runs sharing an alias is an error naming the candidates), `list_runs`/`list_runs_for_owner`, steering log (capped at 20), `THINKING_LEVELS`. |
| `src/fleet/envelope.rs` | the mailbox envelope (`{"id","ts","from","to","type","payload"}`) shared by every `inbox.jsonl` and `outbox.jsonl` line, plus party parsing and typed builders/decoders. The contract is pinned byte-for-byte, see the module doc. |
| `src/fleet/event.rs` | `FleetEvent` and `<fleet-event>` rendering. `sanitize_field`/`attr` are the security boundary that stops worker text forging or closing a block. |
| `src/fleet/report.rs` | reading `runs/<id>/report.md` with the steering appendix, falling back to last assistant text. |
| `src/worker/` | the detached worker monitor (`monitor.rs`), pi RPC message types (`rpc.rs`), `pi --list-models` and model checking (`models.rs`). The monitor also materialises the embedded pi extension and skill into `.pilotfish/pi/` at boot. |
| `src/orch/` | the claude side: stream-json wire types (`protocol.rs`), argv builder (`args.rs`), child process (`process.rs`), detached monitor (`monitor.rs`), transcript records (`records.rs`), console-side client (`client.rs`), embedded prompt (`prompt.rs`), the `.mcp.json` (`mcp_config.rs`), health and the orphan reaper (`health.rs`), the run-state watcher that turns state into fleet events (`watcher.rs`), and the session store for `fleet.json` (`session.rs`). |
| `src/route.rs` | choosing a worker's model, thinking level and worktree from its brief with TypeSafe's System One (Jev), in two cycles, and the `routing/` question files a spawn uses to ask the human when Jev is unsure. Off by default; a judgment that cannot be had is no judgment, never an error. `narrow` is the one definition of which catalogue models matter, shared with `fleet_status` and the shortlist editor. |
| `src/secrets.rs` | the TypeSafe key: environment first, then the OS credential store (`keyring`). `Secret` never prints; there is deliberately no file fallback. |
| `src/ops/` | the shared operation layer both the CLI and the MCP tools call: `spawn.rs`, `query.rs` (status/output/logs/report/wait/attach), `steer.rs` (send/followup/answer/stop), `integrate.rs` (diff/merge/cleanup). The CLI-shaped signatures live beside `_core` variants that take a `Party` source, so the console and MCP can attribute actions honestly. |
| `src/mcp/` | the stdio MCP server (`server.rs`), one tool per op, built on `rmcp`. Server name stays `fleet` so tools stay `mcp__fleet__*`. |
| `src/tui/` | the console: state and update loop (`app/mod.rs`, with the overlay handlers in `app/overlays.rs` and the composer's commands in `app/commands.rs`), view model (`model.rs`), keys (`keys.rs`), palette (`palette.rs`), completions (`completions.rs`), transcript (`transcript.rs`), markdown rendering (`markdown.rs`), theme (`theme.rs`), crossterm runtime (`runtime.rs`), and `view/` draw functions (session, dashboard, composer, overlay, statusline). |
| `pi/`, `prompts/` | TypeScript on purpose, embedded with `include_str!` and materialised into `.pilotfish/pi/` at worker boot: `pi/extensions/fleet-worker.ts`, `pi/skills/fleet-worker-report/SKILL.md`, `prompts/orchestrator.md`. |

## The orchestrator contract

The orchestrator is a `claude -p` child of its monitor, given the fleet tools over stdio MCP (`pilotfish mcp`, server name `fleet`, so the tools are `mcp__fleet__*`) and nothing else that writes: `Edit`, `Write` and `NotebookEdit` are disabled for it, reads and read-only git are allowlisted, everything else raises a permission prompt the human answers in the console.

Tools, all thin wrappers over the same `ops` functions the CLI subcommands call: `fleet_spawn`, `fleet_status`, `fleet_wait`, `fleet_output`, `fleet_logs`, `fleet_send`, `fleet_followup`, `fleet_answer`, `fleet_stop`, `fleet_report`, `fleet_diff`, `fleet_merge`, `fleet_cleanup`. Every result ends with an `exit: N` line carrying the CLI exit code, so the agent branches on the same numbers a script would. `fleet_merge` aborts on conflict (exit 5) and names the base commit: conflicts go back to the worker as a rebase brief, because the orchestrator cannot edit files. `fleet_wait` defaults to 120 s with `timeoutSec` validated 1..=600, where the CLI's default is 600 s. With routing on, `fleet_spawn` can block for up to ten minutes while the human chooses a model Jev was unsure of (see Routing a brief); the prompt tells the agent that wait is expected.

Its brief lives in `prompts/orchestrator.md`, embedded with `include_str!`, rendered with the fleet's placeholders and written to each session's `orchestrators/<key>/prompt.md`. The override order is `$PILOTFISH_PROMPT` (set-but-missing is an error), `<repo>/.pilotfish/orchestrator.md`, `~/.pilotfish/orchestrator.md`, then the embedded copy. The legacy `~/.config/parl/orchestrator.md` is no longer read; when only it exists, resolution warns on stderr and names both paths. Nothing is ever copied into a project. Change the prompt and the tool semantics together: the prompt is where the tool contract is actually stated to the agent.

## The worker contract

A worker never sees the human's conversation. It gets a brief, and it answers through files under `.pilotfish/`:

- Its report (`runs/<runId>/report.md`) with fixed sections: Status, Summary, What I did, Files changed, Verification, Decisions & assumptions, Steering received, Open questions, Suggested next step. The shape is enforced by the embedded skill `pi/skills/fleet-worker-report/SKILL.md`.
- `fleet_ask`, a tool from the embedded extension (`pi/extensions/fleet-worker.ts`) that posts a question and blocks until someone answers. After ten minutes (`PILOTFISH_ASK_TIMEOUT_MS`, poll `PILOTFISH_ASK_POLL_MS`) the worker proceeds on its own judgment and records that under Decisions.
- `fleet_progress`, one-line milestones. Forwarded to the orchestrator only under `--progress-events`.

The watcher turns run state and events into `<fleet-event>` blocks injected into the orchestrator's conversation:

```text
<fleet-event kind="settled" run="add-auth-1f2e3d4" name="add-auth" id="ev_..." ts="...">
status: settled
report: /repo/.pilotfish/runs/add-auth-1f2e3d4/report.md (present)
</fleet-event>
```

Kinds: `settled`, `stopped`, `error`, `dead`, `question`, `question_resolved`, `answered_by_console`, `console_steer`, `progress`, `snapshot`; events caused by the human are labelled as such so the orchestrator reconciles with the intervention instead of undoing it. `sanitize_field`/`attr` in `src/fleet/event.rs` are the security boundary: worker text can never forge or close one of these blocks.

## Stack

Single crate `pilotfish`, lib + bin, edition 2024, version 0.2.0. `Cargo.lock` is committed, since this is a binary. Everything async is tokio (process, fs, io, sync, time, signal).

| Area | Crates | Worth knowing |
| --- | --- | --- |
| CLI | `clap` (derive, env) | `cli.rs` parses, `main.rs` dispatches, `ops` does the work |
| TUI | `ratatui` 0.29.0, `crossterm` 0.28.1, `tui-textarea` 0.7, `unicode-width` 0.2 | pinned exactly: `tui-textarea 0.7` requires `ratatui ^0.29`, so the three move together or not at all |
| MCP | `rmcp` 3.4.1 (`server`, `transport-io`) | its model types are `#[non_exhaustive]`, so build them with `Default` + field assignment. `ServerInfo` is deprecated in favour of `ServerConfig` (same type). Tool schemas are hand-built JSON in `src/mcp/server.rs` |
| Data | `serde`, `serde_json`, `time`, `uuid` | on-disk JSON is camelCase and tolerant of unknown/missing fields; `uuid` is the identity type for runs and sessions |
| Errors | `thiserror` in the library, `anyhow` at the binary edges | |
| Console | `nucleo-matcher` (palette ranking), `pulldown-cmark` (transcript markdown) | wrapping is hand-rolled in `markdown.rs` and `view/overlay.rs` |
| Process | `nix` (`signal`, `process`) | `unsafe_code = "forbid"` rules out `libc::kill`, so pid liveness is `nix::sys::signal::kill`, where EPERM counts as alive |
| Routing | `reqwest` (`json`, `rustls-tls`, no default features) | the only HTTP in the tree: `src/route.rs` calls TypeSafe's System One, since it has no Rust SDK |
| Secrets | `keyring` 4 (default `v1`: macOS Keychain, Windows Credential Manager, Secret Service over pure-Rust zbus) | every call blocks and can raise a system dialog, so callers use `spawn_blocking` |
| Config edits | `toml_edit` | `set_routing` edits `[routing]` keys in `~/.pilotfish/config.toml` in place, keeping the user's comments and order; `toml` still does the reading |
| Misc | `regex`, `dirs` (home lookup for the `~/.pilotfish` user dir), `rand`, `toml` (user config), `futures` | `StreamExt` over the crossterm event stream |

git is the git CLI, not `git2`: everything goes through `git_raw`, which trusts the real exit code, because merge conflicts print to stdout and sniffing stderr gets it wrong. The two agents are subprocesses, not libraries: `claude -p` over stream-json, `pi --mode rpc` over its own RPC; neither SDK is linked in.

## Verified protocol facts

**1. Changing a model mid-session works on both sides (verified 2026-08-30 against the real binaries).** The claude control request `{"type":"control_request","request_id":"r","request":{"subtype":"set_model","model":"fable"}}` returns `{"subtype":"success"}` and switches the running session with no child restart and no conversation turn. It validates — an unknown id errors with claude's own text, which the console shows verbatim — and accepts `opus`, `sonnet`, `haiku`, `fable`, `opusplan` and full ids like `claude-opus-5`. `apply_flag_settings {settings:{model}}` also succeeds but does not validate, so prefer `set_model`; there is no control request that lists models. On pi the RPC is `{"type":"set_model","provider":"anthropic","modelId":"…"}`, and `get_available_models` returns the full list, so worker model completions are real. Consequence: `/model` works for the selected session either way, mirroring `/thinking`.

**2. pi extensions can open blocking dialogs.** pi emits `extension_ui_request` (`select`/`confirm`/`input`/`editor`) on stdout and blocks until the client replies on stdin with `extension_ui_response` (`value`, or `confirmed`/`cancelled`); a `timeout` in milliseconds auto-resolves with `undefined`. `notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text` need no reply; wire shapes are in pi's `docs/rpc.md`. The worker monitor treats a dialog request like a `fleet_ask`: recorded on the run (`pendingDialog`) so the console shows the session as blocked and can answer it, and if nobody answers, the monitor sends `cancelled: true` shortly before pi's own timeout so the worker never hangs.

**3. The claude stream-json / control protocol, spiked against the real binary (claude 2.1.251, 2026-08-29).** The spike script (`scripts/spike-claude-protocol.ts`) was deleted at cutover, so these findings are recorded only here.

- `can_use_tool` arrives without any `initialize` handshake (`echo` never prompts, being in the built-in read-only set — probe with a writing command). The handshake is sent at startup anyway: its response carries the command/skill list, and a bare `{subtype:"initialize"}` is acked but not needed.
- `--allowedTools "mcp__fleet__*"` suppresses permission prompts for the fleet tools.
- `updatedPermissions` built from the request's `permission_suggestions` is honored: after an allow-always, the same command does not prompt again.
- `--append-system-prompt-file` is a hidden flag and works. `system/init` arrives only after the FIRST user message and is re-emitted after every user message; nothing at all is written before the first message. Also observed: `system/status {status:"requesting"}`, thinking deltas, `system/task_started|task_notification|background_tasks_changed`.
- A user message injected mid-turn is delivered inside the running turn as a system-reminder right after the next tool result. Whether the model acts on it is up to the model (haiku once folded it in, once ignored it as a possible prompt injection). This is why the orchestrator prompt states that `<fleet-event>` messages arriving mid-turn are legitimate and must be acted on.
- Permission modes: the flag accepts acceptEdits, auto, bypassPermissions, manual, dontAsk, plan and `default` (a hidden alias for `manual`; `bogus` is rejected), and `set_permission_mode` succeeds for all of them — so the modes the console offers work both at launch and mid-session.

**4. `deepseek-v4-flash` is provider-dependent (measured 2026-08-30).** On `openrouter` pi receives well-formed structured tool calls but the model never invokes a write tool — runs end with no edits, no commit, no report, so it is unusable for agentic work. On `opencode-go` it edits and commits normally; `opencode-go` needs an explicit account opt-in, without which every spawn dies instantly with `403 {"type":"RegionError", ...}`.

**5. pi accepts a thinking level its model does not have and then ignores it (measured 2026-09-02 against a real deepseek-v4-flash worker).** `{"type":"set_thinking_level","level":"max"}` came back `{"command":"set_thinking_level","success":true}`, and the `get_state` that followed still read `"thinkingLevel":"xhigh"`. The reason is in the model payload `get_state` already returns: `thinkingLevelMap` maps every level pi knows and nulls the ones the model lacks — deepseek-v4-flash nulls `minimal`, `low`, `medium` and `max`, so it has only `off`, `high`, `xhigh`. So `success` means "understood", not "applied", and the only honest source for the running level is the `thinkingLevel` a refresh comes back with. `available_thinking_levels` on the run is that map, filtered to [`THINKING_LEVELS`] order; empty means pi has not said yet, which reads as every level rather than none. `get_available_thinking_levels` returns the same list as its own round trip, but the map rides along with the `get_state` the monitor already does at boot.

**6. claude's `result` counts per query, and `initialize` can be sent again (measured 2026-09-23 against claude 2.1.280).** Three one-word turns in one stream-json session each came back `num_turns: 1`, while `total_cost_usd` grew (0.177 → 0.186 → 0.196): the turn count restarts with every user message, the cost is the session's. So anything that means "turns in this session" counts results itself — the monitor's `state.num_turns`, the transcript's, and auto-compaction's window. A second `initialize` control request mid-session returned `success` with the full `commands` list, so `RefreshCapabilities` works as written. `system/init`, re-sent after every user message, also carries `slash_commands`; it still arrives only after the first message, but with the user's hooks configured `system/hook_started`/`hook_response` lines now arrive before any, so "nothing before the first message" (fact 3) no longer holds literally. And pi 0.87's `get_available_models` returns ~700 models, nearly every id under two or more providers (`anthropic` and `claude-bridge`, `openrouter` and `vercel-ai-gateway`), each with its own `thinkingLevelMap`, `contextWindow` and `cost` — which is why a model is only ever named `provider:id`, and why routing narrows the catalogue before asking.

## The `.pilotfish` layout

```text
.pilotfish/
  fleet.json            the v2 session store, `{"version":2,"sessions":{<uuid>: row}}`; each row: alias,
                        last_heartbeat, pid + pid_started_at, claude session id, model, watcher cursors,
                        launch record; unknown top-level keys (console prefs under "console") round-trip
  fleet.json.lock       lock sidecar for store mutations — the store is written by atomic rename, so
                        flocking the store file itself would lock a fresh inode every write
  console.lock          single-instance lock for the TUI; a spawn also reads it to learn whether anyone is
                        there to ask (fresh heartbeat = a console is open)
  routing/              model choices a waiting spawn put to the human: `<id>.json` (candidates, asker pid,
                        deadline) and `<id>.answer.json` from the console; the spawn removes both (lazy)
  pi-cache.json         the fleet-level pi catalogue (models + commands) with `fetchedAt`; a property of
                        the pi installation, so it lives here once instead of in every run.json
  orchestrators/        one directory per session; the per-session dirs are created lazily
  orchestrators/<alias|-default>-<short-uuid>/
    state.json          monitor pid, session id, model, cost, turns, activity, pending permission
    capabilities.json   what the agent offers now: tools, commands, mcp servers, models, with `fetchedAt`
    events.jsonl        the orchestrator transcript
    inbox.jsonl         console -> monitor: messages, permission answers, interrupts, thinking and model
                        changes, capability refreshes, stop
    claude.log          raw protocol both directions, plus the monitor's own diagnostics
    prompt.md           the rendered prompt the orchestrator was started with
  runs/<name>-<short-uuid>/
    run.json            status, worktree, branch, base commit, last tool/activity, steering log, pending question or dialog
    events.jsonl        selected pi RPC events plus fleet events (steering_delivered, worker_question, worker_progress, answer_delivered, ...)
    inbox.jsonl         steer/follow_up/command/thinking/abort/answer/model/refresh_capabilities  (lazy)
    outbox.jsonl        question/progress/question_resolved                            (created lazily)
    report.md · pi.log · session/   final report; raw pi RPC stream + monitor diagnostics; pi session files (lazy)
  pi/
    extensions/fleet-worker.ts        materialised from the binary at worker boot
    skills/fleet-worker-report/SKILL.md
```

`inbox.jsonl`, `outbox.jsonl` and `session/` appear only on first use, so a run that is never steered has no `inbox.jsonl` on disk. Status is derived, never stored: a run whose monitor is gone reads as `dead`, a running worker waiting on `fleet_ask` or a pi dialog reads as `blocked`. Gone for good: top-level `reports/`, `orchestrator.json`, per-run `monitor.log`, `tui.lock` — no migration, and an old `.pi-fleet` is simply ignored.

## Capabilities

What an agent offers is **asked for, never snapshotted**. A session's command list grows when a skill is installed, so a copy taken at handshake time is wrong by the time it matters.

- The orchestrator's tools, commands, MCP servers and model names live in `orchestrators/<key>/capabilities.json`, not in `state.json`. The monitor fills it from `system/init` (which is where the `tools` list comes from) and from the `initialize` control response, and rewrites it whenever either arrives.
- pi's catalogue lives in the fleet-level `pi-cache.json`, because models and commands describe the pi *installation*, not one run — a per-run copy is what once made a single `run.json` reach 128 KB. `available_thinking_levels` stays on `run.json`, since the level map is per-model and therefore per-run.
- Both files carry `fetchedAt`. The console refreshes anything older than 30 s when the palette opens or an unknown `/command` is typed: `OrchestratorCommand::RefreshCapabilities` re-issues the `initialize` request, and the `refresh_capabilities` envelope makes a live worker monitor re-ask pi (`get_state`, `get_commands`, `get_available_models`). With no worker running there is nobody to ask, and the last answer stands.
- The one thing that cannot be asked for is claude's model list — no control request lists it (verified fact 3) — so `ORCHESTRATOR_MODEL_ALIASES` in `src/orch/records.rs` seeds `capabilities.models` and the session's own resolved model is appended.
- A worker monitor updates `pi-cache.json` one field at a time (read, change the field that arrived, write), never from a whole cache in its memory: monitors boot side by side and learn commands and models in separate replies, and a whole-file write after the first once wiped the models another spawn was routing from. An empty models answer replaces nothing.
- `fleet_status` reports the models that matter — the catalogue narrowed by `[worker] provider` and `[routing] models`, capped at `MODELS_SHOWN` — rather than all of pi's hundreds on every call.

## The console's shape

One conversation, and overlays over it. There is no second view and no modal split.

- **The composer always has focus**, so every printable key is text and no letter is stolen from a message. The only non-text keys are `ctrl` chords: `ctrl-f` fleet, `ctrl-k` palette, `ctrl-r` search (again steps), `ctrl-o` unfold, `ctrl-y` mouse, page keys to scroll. `esc` walks outwards — popup, answer, line — and interrupts the turn when there is nothing left to clear.
- **The fleet is `Overlay::Fleet`**, drawn by `view/dashboard.rs` over the conversation. `build_rows` in `src/tui/model.rs` is the one source of rows, as it always was; what went is the rail beside the transcript and the `railMode` preference it needed.
- **An overlay that is a list reads single letters as commands** (`map_overlay_key`), and one that is a text field reads them as text (`map_key`). `Console::handle_key` picks per overlay, including the permission overlay, which switches while a deny reason or a custom answer is being written — a reason starting with "just" must not lose its letters to list navigation. `n` and `y` are never remapped, because that is how a confirm prompt hears no.
- **A permission prompt raises itself**, once per request id: it blocks the orchestrator, so waiting to be found is wrong, and re-raising one that was dismissed would trap the console. It never rises under someone's fingers: not while the composer holds text, not until the keyboard has been quiet `TYPING_QUIET_MS`, and keys landing within `RAISE_GRACE_MS` of it appearing are dropped — otherwise the `a` of a word being typed is allow-always.
- **`esc` never aborts a worker.** Abort escalates on repeat to SIGKILL, and a reflexive close key must not destroy work; `/stop` and `s` in the fleet are the deliberate ways. The chain is popup → answer → line → search highlight → interrupt the orchestrator's turn.
- **Bracketed paste is on**, so a paste arrives as one `Event::Paste` routed by `Console::paste`: the composer keeps newlines (a pasted brief is one message, not one per line), one-line fields drop them, a key field keeps it masked.
- **A model question raises itself too** (`Overlay::ModelChoice`), under the same rules as a permission prompt and after any pending one: the spawn that asked it is waiting, and so is the orchestrator's `fleet_spawn`. The runtime reads `routing::pending_questions` on the feed tick; a question that vanishes (answered, expired, asker gone) closes its prompt. `esc` leaves it pending, the status line counts it, and `a` on the orchestrator row reopens it.
- **`/routing` (`/jev`)** opens `Overlay::Routing`: the switch, where the key in force comes from, what routing would choose between or why it would decline, the shortlist size and the ask limit (`-`/`+`, steps of 0.05). A key being entered is a `Secret` drawn one dot per character; saving goes to the OS store off the UI thread. Status loads the same way, since reading the store can raise a dialog. `m` opens the shortlist editor (`ShortlistEditor`), a text field over `pi-cache.json`: typing filters, `enter` ticks, `esc` saves `[routing] models`. Bare-id entries expand to the `provider:id` rows they match; entries that match nothing are kept verbatim.
- **Folding.** Reasoning and tool output older than `RECENT_BLOCKS` fold to one summary row each; `/verbose` (`ctrl-o`) unfolds. The model's prose, the human's prompts, fleet events and errors are never folded.
- **`app/` is one module in three files**, not three modules: `overlays.rs` and `commands.rs` are children of `app`, so `Console`'s state stays private to the console and only what the parent calls back into is `pub(super)`. A sibling module would have needed the fields public.

## Streaming and trimming

A token reaches the screen through three intervals, and all three are named constants: the monitor coalesces deltas every `STREAM_FLUSH_MS` (50 ms), the console reads the transcript tail every `TAIL_MS` (120 ms) and draws on `TICK_MS` (250 ms). Reloading every `run.json` and the diff stats is separate and slower (`FEED_MS`, 400 ms) — a frame is skipped only when a tail poll found nothing.

- **A reply is drawn once.** Whole lines commit as `Text` blocks the moment they arrive, so markdown renders while the reply streams; only the trailing incomplete line is held in `Transcript::partial`. The assistant message repeats the whole reply, and `unstreamed_tail` pushes only the part that is not already on screen. Reasoning streams the same way (`stream_thinking`/`StreamThinking`), which is also what keeps it drawn *before* the answer it produced.
- **Nothing agent-written reaches the screen unscrubbed.** `Transcript::push` runs `util::visible_line` on every block, streamed lines go through it too, and `partial()` scrubs the in-flight line on read. `visible_line` removes escape sequences whole rather than spacing out the `ESC` and leaving `[31m` behind as text.
- **Block indices are rebased, not left to rot.** `Transcript::dropped()` counts blocks that fell off the top past `MAX_BLOCKS`; `Console::rebase_scroll` moves a pinned scroll and the search hits by the same amount, so a long session does not silently shift what the reader is looking at. Paging still moves in blocks rather than rendered rows.
- **Sessions trim themselves, four ways.** Past `MAX_EVENT_LINES` the monitor cuts `events.jsonl` to half of that — the gap is what keeps it rare, since every trim makes the console replay — by writing a temp file and renaming it over, under its own lock. The monitor is the file's only writer: `/trim` asks it (`OrchestratorCommand::TrimTranscript`) rather than rewriting the file from the console. The console knows to replay by the file's inode changing, or its size dropping below the cursor. The monitor also sends claude's own `/compact` once a session has run `[session] auto_compact_turns` turns since the last one (default 60, `0` off); it counts results itself (verified fact 6), and the `/compact` turn's own result opens the next window rather than counting toward it — counting it once made a threshold of one compact forever. `/clear` forgets a transcript in the console only. `/compact` is deliberately *not* a console command; it passes through verbatim, and an agent command is only refused as unknown once claude has actually listed its commands.

## Sessions, user config, and limits

- **`~/.pilotfish/config.toml`** is the user-level config: `[orchestrator] model`, `[worker] model`/`provider`, `[session] auto_compact_turns`, `[routing] enabled`/`model`/`confidence_threshold`/`endpoint`/`models`, `[limits] max_workers_per_session`. Resolution is most-specific-wins: explicit flag/argument → project `fleet.json` launch record → user config → built-in default. `$PILOTFISH_HOME` overrides the directory wholesale, mirroring `$PILOTFISH_DIR` for a fleet. A malformed file is a hard error naming the path, never a silent fallback; a missing or empty file reads as defaults.
- **The worker cap is enforced, not advice.** Once a session's live runs reach `max_workers_per_session` (default 3; 0 means "no spawning allowed"), `spawn` refuses with exit 1, naming the cap and the runs holding slots. The prompt's `{{MAX_WORKERS}}` resolves through the same config value, so advice and enforcement cannot drift.
- **Session isolation.** Each monitor is pinned with `--session <uuid>`, and the watcher filters through `list_runs_for_owner`, so a worker settling in one session never appears in another's transcript. `tests/orch_multi_session.rs` proves it with two live sessions on one fleet.
- **Per-session shutdown.** Removing `orchestrators/<key>/` stops exactly that monitor within `MISSING_DIR_POLLS` polls; deleting `.pilotfish` stops them all.
- **Monitor health.** `last_heartbeat` is stamped on a 5 s cadence (`HEARTBEAT_WRITE_MS`); `monitor_health` derives Running / Wedged / Stopped from heartbeat freshness (`HEARTBEAT_GRACE_MS` 15 s) plus pid liveness — a live pid with a stale heartbeat is a wedged monitor.

## Routing a brief (Jev)

`src/route.rs` asks TypeSafe's System One which model should carry a brief, how hard it should think, and whether it needs a worktree. **Off by default**, and inert without a key however the config reads.

- **Candidates first.** `narrow` cuts pi's catalogue to the pinned provider (`--provider` or `[worker] provider`) and the `[routing] models` shortlist (`provider:id`, or a bare id for every provider). Every option is named `provider:id` — most ids are served by several providers that do not behave alike (verified facts 4 and 6). More than `MAX_CHOICES` (255, TypeSafe's limit) and routing declines with a note saying how to narrow it; the shortlist editor refuses a 256th entry for the same reason.
- **Cycle one: the model, for value.** One request: a Choice over the candidates plus two Nouls (`needs_worktree`, `parallel_safe`), which do not depend on the model. Each candidate is described with its context window, price, reasoning levels, and how many times the cheapest candidate it costs (input and output blended 3:1); the question asks for the best result *for its cost*, because asked for the best model Jev picked the dearest every time. Code then applies a near-tie rule: a candidate within `NEAR_TIE` (0.05) of Jev's pick in probability that is cheaper wins. A model with no known price neither undercuts nor is undercut.
- **Below the limit, the human chooses.** When the model answer's `confidence` is under `[routing] confidence_threshold` (0.6), or names nothing on offer, `judge` returns the candidates ranked (Jev's leaning, then probability, then price) instead of a model. The spawn writes `routing/<id>.json` and polls for `<id>.answer.json` every 200 ms until the deadline (`PILOTFISH_ASK_TIMEOUT_MS`, default ten minutes, as `fleet_ask`); then the configured model stands. With no live console (`console.lock` heartbeat stale or missing) nobody is waited for. A drop guard removes the question however the wait ends — a cancelled MCP call included — and the console ignores questions whose asker pid is dead or whose deadline passed. Claude's MCP tool timeout defaults to 1e8 ms (read from the 2.1.281 binary), so a ten-minute `fleet_spawn` needs no config.
- **Cycle two: the thinking level**, asked once the model is settled — by Jev, the human, or the fallback — as a Choice between *that* model's levels, each described by what it is for. A model with fewer than two levels is not asked about. This replaced an effort Score clamped after the fact: a Choice over the real levels cannot land on one the model lacks (verified fact 5). A pinned `model` still gets a thinking level chosen; a pinned `model` and `thinking` together are never routed.
- **Code owns the rest of the policy.** A worktree is only dropped when `needs_worktree` is under `READ_ONLY_BELOW` (0.1): being wrong that way puts a worker's edits in the human's checkout.
- **Declines are said, not swallowed.** Routing on but no key, no store, no catalogue, too many candidates, or no answer: the spawn goes ahead on the configured defaults and prints `routing: not routed: …`. A judgment that cannot be had is never an error.
- **The seam** is `route_request` in `src/ops/spawn.rs`, run *before* the per-session cap is counted, so a slow judgment — or a human thinking — cannot widen the gap between counting live workers and creating the new one. The key is only looked up once routing is on — reading the store can raise a dialog; `route_with_key` is the part below the lookup, which is what the tests drive. What was decided is written to `run.json` as `routing`, its `note` saying who chose.
- **The key** lives in `src/secrets.rs`: `$PILOTFISH_TYPESAFE_API_KEY`, then `$TYPESAFE_API_KEY`, then the OS credential store (service `pilotfish`, account `typesafe-api-key`), set from the console's `/routing` panel. No file fallback, ever: a box without a store uses the environment variable. `Secret`'s `Debug` prints only a masked tail, so effects and states carrying one stay safe to log. The one test that touches the real store is `#[ignore]`d and round-trips a throwaway entry; run it by hand per platform.
- **Config:** `[routing] enabled`, `model` (`jev-latest`), `confidence_threshold` (validated 0..1 at load — `80` is an error, not a silent default), `endpoint`, `models`. `paths::set_routing` edits any of them in place with `toml_edit`. `$PILOTFISH_TYPESAFE_URL` or `endpoint` points it elsewhere, which is how the tests reach a local stub (`route::test_support::stub`, one reply per request). `--route` / `--no-route` and `fleet_spawn`'s `route` override `enabled` per spawn.

## Known issues

- **`fleet_spawn`'s structured output field is `fleetDir`**, where the TypeScript emitted `piFleetDir`. Intentional, since it follows `SpawnData`'s serialisation, but noted in case anything reads it.
- **Still open: `src/worker/models.rs:17/:65`** — `list_models` transiently returns nothing and caches the empty result.
- **Resolved flakes, kept for archaeology** (see `git log` for the fixes): the zombie reap (`cf08a69`), the transient git-subprocess family (now one shared bounded-retry `git::test_support::git_sync` helper), the `run.json` flush races in `tests/worker_monitor.rs`, and the ambient-`PILOTFISH_DIR` test-isolation leak (`cb9cf81`, resolution is now injectable). If the old `src/ops/mod.rs:149` NotFound symptom recurs post-fix, it is environmental.

## Traps

Each of these cost a debugging session once already.

- Rust does not reap detached children on drop the way Node's `unref()` did, so a spawned monitor lingers as a zombie whose pid still looks alive. Spawn through `tokio::process::Command` (it has the safe `process_group(0)`, while `pre_exec` is `unsafe` and forbidden) and reap in a background task, as `spawn_monitor` and `launch_monitor` do.
- `ok()` in `ops` zeroes `err`, so anything that carries stderr text alongside a successful exit (diff/merge dirty warnings, attach's static-tail note, cleanup's kept-branch warning) has to build its `CommandResult` by hand. Two warnings were silently dropped before a test caught it.
- The orchestrator's pending-permission map is the source of truth, and the list in `state.json` is derived from it. Remove a request from the list and the next flush resurrects it.
- `ProcEvent::Error` has to end the orchestrator monitor: a bad binary produces an error and no close, and the old TypeScript monitor hung forever on it.
- The non-interactive refusal tests stdin/stdout with `crossterm::tty::IsTty`, not `terminal::size()`, whose `tput` fallback answers even with no controlling terminal. The old check sailed past the friendly refusal and died in raw mode.
- Envelope readers stay tolerant on purpose: an unknown `type` or an unknown payload field parses, decodes to `None`, and the line is skipped, so a newer writer can never crash an older reader. Keep it that way.
- Tool output is drawn with control characters and reaches the screen verbatim: `git rebase` writes `Rebasing (1/6)\rRebasing (2/6)\r…\rSuccessfully rebased and updated …` as one line. A cell holding a bare CR sends the terminal's cursor to column 0 mid-row, so the rest of the row repaints over what was drawn and the whole frame tears (an `ESC` would do worse). Two defences, both needed: `util::visible_line` resolves CRs the way a terminal would and spaces out every other control character, and every block goes through `Transcript::push`, which calls it; `view::scrub_controls` then sweeps the finished frame buffer, so a widget that skips the first defence still cannot tear the screen.
- The orphan reaper once matched a bare `"claude"` substring. With N sessions, a stale pid recycled onto another session's claude child matched, and the reaper SIGTERMed a healthy session. It now matches `--session <uuid>` and refuses any pid whose process started after the recorded `pid_started_at`.
- `session::save` destroyed every key it did not model. The console keeps prefs under a `"console"` key in the same `fleet.json`, so the monitor's 5 s heartbeat erased the console's keys continuously. `FleetSessions` now carries `#[serde(flatten)] extra` so unknown top-level keys round-trip. This passed a full green suite because nothing asserted that one writer preserves another writer's keys.
- A test that returns right after `shutdown()` drops a temp dir its monitor is still writing into. The removal can fail half-way (`ENOTEMPTY` — the monitor created a file mid-walk), the directory survives, and a monitor whose directory survives never learns to stop: two leaked this way and ran a compaction loop for two days. Tests wait for the monitor pid to go (`stop_and_wait` in `tests/orch_monitor.rs`), and removals under a live monitor retry.
- `fleet.json` is written by atomic rename, so an flock on the store file locks a fresh inode every write. Mutations go through `session::with_store_mutation`, which locks a stable `fleet.json.lock` sidecar.

## Conventions

- Borrow rather than clone, and return `Result` rather than panic. `thiserror` for typed library errors, `anyhow` at the binary edges, `?` over match chains. Doc comments on public items explain *what*, `//` comments only explain *why*, and both stay sparse.
- Lints are the contract: `unsafe_code` is forbidden, and clippy `all`, `perf`, `unwrap_used`, `todo` and `dbg_macro` are denied. Unit tests may unwrap through the crate-level `cfg_attr(test, allow(clippy::unwrap_used))`. Integration tests under `tests/` need `#![allow(clippy::unwrap_used)]` at the top of each file.
- The CLI surface is the contract: `main.rs` dispatches, `cli.rs` parses, and the ops signatures they call are the seam. Change all three together or none.
- Verification before you finish (all four must pass):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

## Tests

The suite is hermetic: the pi and claude sides are driven by Node fakes in `tests/fixtures/` (`fake-pi-pilotfish.mjs`, `fake-claude.mjs` and friends), so a full run spends no tokens and touches no network. It does need `node` on PATH. Integration tests drive the built `pilotfish` binary through `assert_cmd`, including `pilotfish monitor` as a real child process, so pid liveness, monitor exit and signals are exercised the way production runs.

Tests never resolve an ambient `PILOTFISH_DIR`: `FleetPaths::discover` prefers `$PILOTFISH_DIR` over `<cwd>/.pilotfish` — right for production, but a bare `cargo test` inside a live fleet (the monitor exports `PILOTFISH_DIR`) once operated on the real fleet. Resolution is therefore injectable end to end — `FleetPaths::discover_with_env`, `resolve_fleet_dir_with_env`, `resolve_run_with_env`, the `*_core_with_env` twins in `ops/`, `FleetServer::with_pilotfish_dir` — with public forms delegating to the ambient value and tests passing `None`. Every test that spawns the binary pins `PILOTFISH_DIR` per child with `Command::env`, or removes it where the `<cwd>/.pilotfish` fallback is the point under test (`tests/console_refusal.rs`); `std::env::set_var` is `unsafe` in edition 2024 and `unsafe_code` is forbidden crate-wide, so per-child env is the only tool. Regression: `spawn_writes_only_to_the_fleet_dir_it_was_given` in `tests/cli_e2e.rs` proves a spawned `pilotfish` writes only into the fleet dir it was given.

Knobs, all derived from `ENV_PREFIX`:

| Variable | Effect |
| --- | --- |
| `PILOTFISH_PI_BIN` | replaces the pi binary, as an executable spec split on spaces, e.g. `node /path/fake-pi-pilotfish.mjs` |
| `PILOTFISH_CLAUDE_BIN` | replaces the claude binary |
| `PILOTFISH_DIR` | points the fleet at a directory other than `<cwd>/.pilotfish` |
| `PILOTFISH_HOME` | points the user config at a directory other than `~/.pilotfish` |
| `PILOTFISH_PROMPT` | the orchestrator prompt override (set-but-missing is an error) |
| `PILOTFISH_ASK_TIMEOUT_MS`, `PILOTFISH_ASK_POLL_MS` | shorten a worker's `fleet_ask` wait and its poll interval; the timeout also bounds a spawn's wait for a model choice |
| `PILOTFISH_RUN` | the run a worker's extension reports into |
| `PILOTFISH_TYPESAFE_API_KEY`, `TYPESAFE_API_KEY` | the routing key; neither set means routing is inert |
| `PILOTFISH_TYPESAFE_URL` | points routing at another endpoint (a proxy, or a test stub) |

Run the full suite the sanctioned way, into a throwaway fleet dir, and check it stayed empty:

```bash
mkdir -p /tmp/pilotfish-canary && rm -rf /tmp/pilotfish-canary/*
PILOTFISH_DIR=/tmp/pilotfish-canary cargo test --all-features
ls -A /tmp/pilotfish-canary     # MUST print nothing
```

fmt, clippy and build all pass even when the isolation leak is live, so they cannot detect this class of failure — a non-empty canary is the only signal that catches it. The suite is green.
