# Headless commands

The TUI is one client. The same fleet is driveable from scripts.

| Command | What it does |
| --- | --- |
| `spawn <name> [opts] -- "<brief>"` | start a worker (`--cwd`, `--model`, `--provider`, `--thinking`, `--no-worktree`, `--base`, `--skill`, `--append-system-prompt`, `--session`, `--tools`, `--exclude-tools`, `--route`/`--no-route`). A `--model` pi does not have is refused before a worktree exists, naming the closest models it does have |
| `status [<name>] [--json] [--all]` | fleet table, or one run's full state, plus the models pi has configured. `--json` includes each run's `activeModel`, `pendingQuestion` and the commands it offers |
| `wait <name> [--timeout s]` | block until the run reaches a terminal state |
| `output <name> [--tail n]` | last assistant text, or the last n tool results |
| `logs <name> [--tail n]` | tail of the raw RPC log |
| `send`, `followup`, `answer`, `stop` | steer, queue a follow-up, answer a question, abort |
| `report <name>` | the final report with the steering log appended. Exit 2 if there is none |
| `diff <name> [--name-only]` | what the worker changed against its base commit |
| `merge <name> [--no-commit]` | merge the worker's branch. Exit 5 on conflicts, with the merge aborted and the checkout clean |
| `cleanup <name\|all> [--force]` | remove the worktree and branch, archive the run |
| `attach <name> [--tail n]` | print a worker's transcript tail |
| `mcp` | serve the fleet tools over stdio (what the orchestrator runs) |

Exit codes: 0 ok, 1 refusal or error, 2 no report, 3 wait timed out, 4 the run ended stopped/error/dead, 5 merge conflict.

`diff` and `merge` only see committed work. If a worker forgets to commit, `diff` warns on stderr and a non-forced `cleanup` refuses to delete the dirty worktree. Brief your workers to commit.

## Choosing a model for a brief

With routing on — `/routing` in the console, `[routing] enabled = true` in `~/.parl/config.toml`, or `--route` on one spawn — a `spawn` that names no `--model` has one chosen from its brief, along with a thinking level and whether the work needs its own worktree. The judgment comes from TypeSafe's System One; the spawn prints what was chosen and why, and records it on the run.

```toml
[routing]
enabled = true
models = ["anthropic:claude-opus-5", "anthropic:claude-sonnet-5", "deepseek-v4-flash"]
confidence_threshold = 0.6      # below this, the configured model stands
```

pi can offer hundreds of models, and most are served by several providers, so routing chooses between the ones you allow: `models` lists them as `provider:id`, or a bare id to allow every provider of it; a `[worker] provider` narrows it further. With more than 255 left, routing declines and says so.

What it decides stays within what you set. A `--model` you pass is never second-guessed. The thinking level is one the chosen model actually has. A worktree is only skipped when the brief is clearly read-only. And when routing is on but cannot decide — no key, no network, too many models — the spawn goes ahead on your configured defaults and prints why.

The key is set with `/routing` in the console, which keeps it in your operating system's credential store, or comes from `$PARL_TYPESAFE_API_KEY` / `$TYPESAFE_API_KEY`, which win when set.
