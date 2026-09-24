# parl

`parl` runs a fleet of [pi](https://github.com/earendil-works/pi-mono) coding agents from the terminal, coordinated by Claude Code.

You describe the work to an orchestrator. It plans the change, writes a brief for each step, and runs one pi worker per step, each in its own git worktree. It then reviews and merges what the workers produce. The whole exchange happens in a single conversation, and the fleet view can be opened at any time to inspect or direct an individual worker.

![The parl console: a conversation with the orchestrator, which has spawned two workers and answered one of their questions](imgs/main.png)

## Features

* **Full oversight.** Any worker can be opened, steered, stopped, or answered directly. The orchestrator is informed of each intervention and incorporates it rather than reversing it.
* **Durable sessions.** Every agent runs under a detached monitor that records its state on disk. The console can be closed mid-run and reopened later without interrupting any work.
* **Separation of duties.** The orchestrator reads, plans, merges and verifies; `Edit` and `Write` are disabled for it. Only workers modify code, each on a dedicated branch.
* **No agent-side setup.** The orchestrator is a `claude -p` process managed by `parl`, and the pi worker extension is embedded in the binary.
* **Model routing.** Optionally, each worker's model and reasoning level are chosen from its brief, weighing capability against cost. Uncertain choices are referred to you.
* **Scriptable.** The console is one client among several. `parl spawn`, `status`, `merge` and related commands operate on the same fleet from a shell.

## Installation

Requirements: a current stable Rust toolchain, `pi` on your `PATH`, and `claude` (Claude Code 2.1.x) signed in. The orchestrator uses your Claude Code login — your subscription, unless `ANTHROPIC_API_KEY` is set, in which case claude uses the key. Workers use whichever providers pi is configured with.

```bash
cargo install --locked --git https://github.com/SkymanOne/parl
parl --help
```

`--locked` builds against the dependency versions recorded in `Cargo.lock`, which are the versions the test suite runs against.

## Usage

### Starting a session

```bash
cd your-repo
parl
```

Describe the task and press `enter`, for example: *"Add token refresh to the auth module and update the tests."* The orchestrator states its plan, spawns the workers it needs, and reports on each as it finishes: its status, what changed, and how the change was verified. Once a worker's branch has been merged and checked, the orchestrator removes it.

The composer always has keyboard focus, so every printable key is text. `shift-enter` inserts a newline (`alt-enter` or `ctrl-j` on terminals that cannot distinguish it), and a pasted multi-line brief is kept as a single message. `esc` closes the open panel, then clears the line, and finally interrupts the orchestrator's current turn.

### Keyboard shortcuts

| Keys | Action |
| --- | --- |
| `ctrl-f` | Open the fleet: every session and the actions available for the selected one |
| `ctrl-k` | Open the command palette: console commands, the agent's own commands, models, sessions |
| `ctrl-r` | Search the conversation; press again for the next match |
| `ctrl-o` | Expand older reasoning and tool output |
| `ctrl-y` | Release the mouse to select and copy text |
| `pgup` / `pgdn` | Scroll (the mouse wheel also scrolls) |
| `/help` | Show all keys and commands |

### Inspecting a worker

![The fleet overlay over the conversation, listing the orchestrator and two running workers](imgs/fleet.png)

`ctrl-f` opens the fleet over the conversation. Each row shows a session's state, branch, diff statistics and current activity. Select a row with `j`/`k` or `1`–`9`, and press `enter` to open that session's conversation. Within the fleet, single letters act on the selected row:

| Key | Action |
| --- | --- |
| `a` | Answer the question or dialog the worker is blocked on |
| `s` | Stop the worker |
| `x` | Remove the worker, its worktree and its branch (asks for confirmation) |
| `t` / `m` | Cycle the thinking level / switch the model, without restarting |
| `b` | Show the full brief |

While a worker is open, anything you type is delivered to it as steering after its current tool call. `/answer <text>`, `/followup <text>` and `/stop` cover the remaining interactions. To return to the orchestrator, press `ctrl-f`, `1`, `enter`.

### Permissions

The orchestrator may read files and run read-only git commands without approval. Any other action raises a prompt in the console: `y` allows it once, `a` allows it for the rest of the session, and `n` denies it with a reason. The frequency of these prompts is set with `--permission-mode`:

```bash
parl --permission-mode auto      # a classifier approves routine actions
```

`p` in the fleet cycles the mode during a session. The available modes are `default`, `auto`, `acceptEdits`, `dontAsk` and `plan`.

### Resuming a session

`/quit` (or `ctrl-c`) closes the console only; the orchestrator and its workers continue to run. Running `parl` again in the same repository restores the session, including any turn in progress and any permission prompt raised while the console was closed. `parl --fresh` starts a new orchestrator session, and `/shutdown` stops all agents.

### Model selection

```bash
parl --model opus                # the orchestrator's model; /model changes it during a session
```

By default a worker runs on `[worker] model` from `~/.parl/config.toml`, unless the orchestrator specifies another. Routing can choose instead: `/routing` enables it and stores a [TypeSafe](https://docs.typesafe.ai) API key in the operating system's credential store, and nowhere else. With routing enabled, a worker spawned without a model is routed in two steps:

1. **Model.** TypeSafe's System One (Jev) selects the model that gives the best result for its cost from your shortlist. Each candidate is presented with its price relative to the cheapest option, and where two candidates are nearly tied, the cheaper one is chosen. If Jev's confidence is below your limit, the console asks you to choose, and the spawn waits for your answer for up to ten minutes before falling back to the configured model.
2. **Thinking level.** Jev then selects a reasoning level from those the chosen model supports.

The shortlist (`m` in the `/routing` panel) and the confidence limit (`-` / `+` in the same panel) are saved to the user configuration:

```toml
# ~/.parl/config.toml
[worker]
model = "claude-sonnet-5"

[routing]
enabled = true
confidence_threshold = 0.6       # below this, you choose the model
models = ["anthropic:claude-opus-5", "anthropic:claude-sonnet-5", "opencode-go:deepseek-v4-flash"]
```

A single decision can weigh at most 255 models, so the shortlist is capped at 255 entries.

### Long-running sessions

Sessions manage their own size. After `[session] auto_compact_turns` turns (60 by default; `0` disables it), the orchestrator's context is summarised with claude's `/compact`, and the transcript file is capped on disk. Older reasoning and tool output are collapsed to one line each, and `ctrl-o` expands them. `/clear` empties the view and `/trim` shortens the file.

### Scripting

The same fleet can be driven without the console:

```bash
parl spawn add-auth -- "Add token refresh to src/auth. Run cargo test. Commit your work."
parl status                      # the fleet table
parl wait add-auth               # exit 0 settled, 3 timed out, 4 stopped/error/dead
parl report add-auth             # the worker's report; exit 2 if there is none
parl diff add-auth
parl merge add-auth              # exit 5 on conflicts, leaving the checkout clean
parl cleanup add-auth
```

Every command accepts `--help`. The exit codes are the same values the orchestrator acts on.

## Documentation

* [Getting started](docs/getting-started.md): installation, launch options, user configuration, and where state is stored
* [The console](docs/console.md): keys and commands, the fleet, the palette, permissions, routing, and the transcript
* [Headless commands](docs/cli.md): the CLI, its exit codes, and how routing decides
* [AGENTS.md](AGENTS.md): the reference for contributors, human or agent — module layout, the on-disk contract, verified facts about the pi and claude protocols, and the reasoning behind the design
