# Headless commands

The console is one client of the fleet. The same operations are available as commands for use in scripts.

| Command | Effect |
| --- | --- |
| `spawn <name> [opts] -- "<brief>"` | Start a worker (`--cwd`, `--model`, `--provider`, `--thinking`, `--no-worktree`, `--base`, `--skill`, `--append-system-prompt`, `--session`, `--tools`, `--exclude-tools`, `--route`/`--no-route`). A `--model` that pi does not offer is refused before a worktree is created, and the closest available models are listed |
| `status [<name>] [--json] [--all]` | The fleet table, or one run's full state, with the models pi has configured. `--json` includes each run's `activeModel`, `pendingQuestion` and available commands |
| `wait <name> [--timeout s]` | Block until the run reaches a terminal state |
| `output <name> [--tail n]` | The last assistant text, or the last n tool results |
| `logs <name> [--tail n]` | The tail of the raw RPC log |
| `send`, `followup`, `answer`, `stop` | Steer, queue a follow-up, answer a question, abort |
| `report <name>` | The final report, with the steering log appended. Exits 2 if there is none |
| `diff <name> [--name-only]` | The worker's changes against its base commit |
| `merge <name> [--no-commit]` | Merge the worker's branch. Exits 5 on conflicts, with the merge aborted and the checkout clean |
| `cleanup <name\|all> [--force]` | Remove the worktree and branch, and archive the run |
| `attach <name> [--tail n]` | Print the tail of a worker's transcript |
| `mcp` | Serve the fleet tools over stdio (used by the orchestrator) |

Exit codes: 0 success, 1 refusal or error, 2 no report, 3 wait timed out, 4 the run ended stopped, failed or dead, 5 merge conflict.

`diff` and `merge` only see committed work. If a worker does not commit, `diff` prints a warning on stderr, and `cleanup` without `--force` refuses to delete the dirty worktree. Worker briefs should therefore instruct the worker to commit.

## Routing a brief

With routing enabled — through `/routing` in the console, `[routing] enabled = true` in `~/.parl/config.toml`, or `--route` on a single spawn — a `spawn` without `--model` has its model and reasoning level chosen from its brief. The decisions come from TypeSafe's System One (Jev) and are made in two requests. The spawn prints what was decided and why, and records it on the run as `routing`.

**1. The model.** Jev is asked which candidate gives the best result for its cost on the brief. Each candidate is described with its context window, its price per million tokens, how many times the cheapest candidate it costs (input and output blended 3:1), and its reasoning levels. When a cheaper candidate is within 0.05 of Jev's pick in probability, the cheaper one is taken. The same request asks whether the brief will modify files and whether it can run alongside the workers already in flight.

If Jev's confidence is below `confidence_threshold`, the choice is referred to you. The console raises a prompt listing every candidate, Jev's preference first, with its probability and price; `enter` or `1`–`9` selects one and `d` keeps the configured model. The spawn — and therefore the orchestrator's `fleet_spawn` call — waits for the answer. Without an answer within ten minutes (`$PARL_ASK_TIMEOUT_MS`), or when no console is open, the configured model is used.

**2. The thinking level.** Once the model is known, whether chosen by Jev, by you, or by configuration, Jev chooses a reasoning level from the levels that model supports. A level the model lacks is never requested, because pi accepts such a level and then ignores it.

```toml
[routing]
enabled = true
models = ["anthropic:claude-opus-5", "anthropic:claude-sonnet-5", "opencode-go:deepseek-v4-flash"]
confidence_threshold = 0.6      # below this, you choose the model
```

pi can offer hundreds of models, most of them from several providers, so routing chooses from a shortlist. `models` lists the candidates as `provider:id`, or as a bare id to include every provider of that model; a `[worker] provider` narrows the list further. The shortlist is edited with `m` in the `/routing` panel, and a single request accepts at most 255 candidates. With no shortlist and more than 255 models available, routing declines and says so.

Routing stays within the limits you set. An explicit `--model` is never overridden, although a reasoning level is still chosen for it unless `--thinking` is also given. A worktree is only omitted when the brief is clearly read-only. When routing is enabled but cannot decide — no key, no network, too many candidates — the spawn proceeds on the configured defaults and prints the reason.

The API key is set with `/routing` in the console, which stores it in the operating system's credential store, or supplied through `$PARL_TYPESAFE_API_KEY` or `$TYPESAFE_API_KEY`, which take precedence when set.
