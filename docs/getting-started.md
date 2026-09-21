# Getting started

## Requirements

* A current stable Rust toolchain (the crate is edition 2024)
* `pi` on your PATH
* `claude` (Claude Code 2.1.x) on your PATH and logged in

## Install

```bash
cargo install --git https://github.com/SkymanOne/parl        # or, from a clone:
cargo install --path .                                       # or: cargo build --release  →  target/release/parl
parl --help                                                  # verify
```

There is nothing to install on the pi side. The worker extension and the report skill ship inside the binary and are written into `<repo>/.parl/pi/` when a worker starts, so every worker gets the current version whether or not the checkout you installed from still exists.

## First run

```bash
cd your-repo
parl
```

Then talk to the orchestrator: *"Add token refresh to the auth module and update the tests."* It writes briefs, spawns workers, and reports back as they finish.

## Launch options

| Option | What it does |
| --- | --- |
| `--cwd <dir>` | work in another directory |
| `--model <model>` | the orchestrator's model (a claude alias or full id) |
| `--permission-mode <mode>` | how its tool use is approved: `default`, `auto`, `acceptEdits`, `dontAsk`, `plan` |
| `--remote-control [name]` | put the orchestrator on Claude Code Remote Control |
| `--fresh` | start a new orchestrator session instead of resuming the saved one |
| `--budget <usd>` | stop the orchestrator after this much spend |
| `--progress-events` | forward workers' progress notes to the orchestrator (off by default, they are chatty) |

Your choices are remembered, so a restarted orchestrator comes back with the same model, permission mode and Remote Control setting.

## Coming and going

Quitting closes the console, nothing else. The orchestrators and their workers are detached processes with their state on disk, so `parl` reopens where you left off: the most recently used orchestrator session, still mid-thought if it was working, with its transcript replayed. A permission prompt raised while no console was open is still waiting for you when you return.

If that orchestrator is no longer running, after a reboot or a `/shutdown`, a new one resumes the same claude session under the transcript you already had, with a line marking the seam. `--fresh` is the way to start over.

## What the orchestrator will not do

It coordinates the work and never types code. `Edit`, `Write` and `NotebookEdit` are disabled for it. It can read the repository and run read-only git commands without asking. Anything else prompts you. Merge conflicts go back to the worker as a rebase brief rather than being fixed in place.

Its brief ships inside the binary, and no file is ever copied into your project. To run a different one, point `$PARL_PROMPT` at a file, or drop one at `<repo>/.parl/orchestrator.md` or `~/.parl/orchestrator.md`. Whatever it was actually told, placeholders filled in, is written to `.parl/orchestrators/<session>/prompt.md` for you to read (one directory per orchestrator session). User-level defaults — `[orchestrator] model`, `[worker] model`/`provider`, `[session] auto_compact_turns`, `[routing] enabled`, `[limits] max_workers_per_session` — live in `~/.parl/config.toml`. Routing is off unless you ask for it; see [the CLI](cli.md).

## Where the state lives

`<repo>/.parl/`, which `parl` creates and adds to `.gitignore`. It is an audit trail: reports, transcripts, mailboxes and raw logs, one directory per run. Nothing but `parl` needs to read it. The layout is written out in [AGENTS.md](../AGENTS.md).
