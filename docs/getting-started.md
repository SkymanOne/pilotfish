# Getting started

## Requirements

* A current stable Rust toolchain (the crate uses edition 2024)
* `pi` on your `PATH`
* `claude` (Claude Code 2.1.x) on your `PATH` and signed in

## Installation

```bash
cargo install --locked --git https://github.com/SkymanOne/parl   # or, from a clone:
cargo install --locked --path .                                  # or: cargo build --release  →  target/release/parl
parl --help                                                      # verify the installation
```

`--locked` builds against the versions in `Cargo.lock`. Without it, Cargo resolves the newest compatible versions of every dependency, which have not necessarily been tested with `parl`.

No installation is needed on the pi side. The worker extension and the report skill are embedded in the binary and written to `<repo>/.parl/pi/` whenever a worker starts, so each worker always runs the current version, regardless of whether the checkout `parl` was built from still exists.

## First run

```bash
cd your-repo
parl
```

Describe the task to the orchestrator, for example: *"Add token refresh to the auth module and update the tests."* It writes the briefs, spawns the workers, and reports as each one finishes.

## Launch options

| Option | Effect |
| --- | --- |
| `--cwd <dir>` | Work in another directory |
| `--model <model>` | The orchestrator's model (a claude alias or full id) |
| `--permission-mode <mode>` | How the orchestrator's tool use is approved: `default`, `auto`, `acceptEdits`, `dontAsk`, `plan` |
| `--remote-control [name]` | Connect the orchestrator to Claude Code Remote Control |
| `--fresh` | Start a new orchestrator session instead of resuming the saved one |
| `--budget <usd>` | Stop the orchestrator after the given spend |
| `--progress-events` | Forward workers' progress notes to the orchestrator (off by default because of their volume) |

These choices are recorded, so a restarted orchestrator resumes with the same model, permission mode and Remote Control setting.

## Closing and reopening

Quitting closes only the console. Orchestrators and workers are detached processes that keep their state on disk, so `parl` reopens the most recently used orchestrator session, with its transcript replayed and any turn in progress still running. A permission prompt raised while no console was open is still waiting when the console returns.

If the orchestrator is no longer running — after a reboot or a `/shutdown` — a new one resumes the same claude session beneath the existing transcript, and a line marks the point of resumption. `--fresh` starts over.

## The orchestrator's limits

The orchestrator coordinates work and does not write code: `Edit`, `Write` and `NotebookEdit` are disabled for it. It may read the repository and run read-only git commands without approval; anything else raises a prompt. Merge conflicts are returned to the worker as a rebase brief rather than resolved in place.

Its brief is embedded in the binary, and no file is copied into your project. To use a different brief, set `$PARL_PROMPT` to a file path, or place one at `<repo>/.parl/orchestrator.md` or `~/.parl/orchestrator.md`. The brief the orchestrator actually received, with placeholders filled in, is written to `.parl/orchestrators/<session>/prompt.md` (one directory per orchestrator session).

## User configuration

User-level defaults live in `~/.parl/config.toml` (`$PARL_HOME` overrides the directory). A missing or empty file means defaults; a malformed one is reported as an error naming the file.

| Setting | Meaning |
| --- | --- |
| `[orchestrator] model` | The orchestrator's default model |
| `[worker] model`, `provider` | The model and provider workers run on when none is specified |
| `[session] auto_compact_turns` | Turns before the orchestrator's context is compacted (default 60, `0` disables) |
| `[routing] enabled` | Choose each worker's model and reasoning level from its brief (off by default) |
| `[routing] models` | The shortlist routing chooses from, as `provider:id` (at most 255) |
| `[routing] confidence_threshold` | Below this confidence, you are asked to choose the model (default 0.6) |
| `[limits] max_workers_per_session` | The maximum number of live workers per session (default 3) |

Routing is off unless enabled. `/routing` in the console enables it, edits the shortlist and the confidence limit, and stores the TypeSafe key in the system credential store; see [the CLI reference](cli.md#routing-a-brief) for how it decides.

## State

State is kept in `<repo>/.parl/`, which `parl` creates and adds to `.gitignore`. It serves as an audit trail — reports, transcripts, mailboxes and raw logs, one directory per run — and nothing other than `parl` needs to read it. The layout is documented in [AGENTS.md](../AGENTS.md).
