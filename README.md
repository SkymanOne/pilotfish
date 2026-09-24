# parl

A fleet of [pi](https://github.com/earendil-works/pi-mono) coding agents in your terminal, with Claude Code as the orchestrator.

You describe the work. The orchestrator plans it, briefs a pi worker for each step, runs them side by side in their own git worktrees, and merges what they finish. You talk to it in one conversation and open the fleet whenever you want to see or touch a worker.

![The parl console: a conversation with the orchestrator, which has spawned two workers and answered one of their questions](imgs/main.png)

* **Watch and interrupt anything.** Open any worker, steer it, or answer its question yourself. The orchestrator is told what you did and works with it instead of undoing it.
* **The console is disposable.** Every agent runs under a detached monitor that keeps its state on disk, so you can close the console mid-run and reopen it where you left off.
* **The orchestrator never types code.** It reads, plans, merges and verifies; `Edit` and `Write` are disabled for it. Workers do the writing, each on its own branch.
* **Nothing to install on either agent.** The orchestrator is a plain `claude -p` process this app owns, and the pi worker extension ships inside the binary.
* **Scriptable.** The console is one client. `parl spawn`, `status`, `merge` and friends drive the same fleet from a shell.

## Install

You need a current Rust toolchain, `pi` on your PATH, and `claude` (Claude Code 2.1.x) logged in. The orchestrator runs on your Claude Code login — your subscription, unless `ANTHROPIC_API_KEY` is set, which claude then uses instead. Workers use whatever providers pi is set up with.

```bash
cargo install --git https://github.com/SkymanOne/parl
parl --help
```

## Usage

### Start a session

```bash
cd your-repo
parl
```

Type what you want done and press `enter`: *"Add token refresh to the auth module and update the tests."* The orchestrator plans the work, tells you what it is spawning, and reports back as each worker finishes — status, what changed, how it was verified. When a worker's branch is merged and checked, the orchestrator cleans it up.

The composer always has focus, so any key you press is text. `shift-enter` adds a line (`alt-enter` or `ctrl-j` on terminals that cannot tell them apart), and a pasted multi-line brief stays one message until you send it. `esc` closes whatever is open, clears the line, and with nothing left interrupts the orchestrator's turn.

### Keys worth knowing

| Keys | What they do |
| --- | --- |
| `ctrl-f` | the fleet: every session, and what you can do to the selected one |
| `ctrl-k` | the command palette: console commands, the agent's own, models, sessions |
| `ctrl-r` | search the conversation; again for the next match |
| `ctrl-o` | unfold older reasoning and tool output |
| `ctrl-y` | release the mouse so you can select and copy text |
| `pgup` / `pgdn` | scroll (the wheel does too) |
| `/help` | everything else |

### Look at a worker

![The fleet overlay over the conversation, listing the orchestrator and two running workers](imgs/fleet.png)

`ctrl-f` opens the fleet over the conversation. Each row is a session with its state, branch, diff stat and what it is doing right now. Move with `j`/`k` or `1`–`9` and press `enter` to open that worker's conversation. In the fleet, single letters act on the selected row:

| Key | Action |
| --- | --- |
| `a` | answer the question or dialog it is blocked on |
| `s` | stop it |
| `x` | remove it: worktree, branch and row (asks first) |
| `t` / `m` | cycle its thinking level / switch its model, live |
| `b` | read its full brief |

With a worker open, what you type steers it, delivered after its current tool call. `/answer <text>`, `/followup <text>` and `/stop` do the rest; `ctrl-f`, `1`, `enter` takes you back to the orchestrator.

### Permissions

The orchestrator runs reads and read-only git freely. Anything else raises a prompt in the console: `y` allows once, `a` allows it for the session, `n` denies with a reason. Choose how often that happens with `--permission-mode`:

```bash
parl --permission-mode auto      # a classifier handles routine approvals
```

`p` in the fleet cycles the mode mid-session; `default`, `auto`, `acceptEdits`, `dontAsk` and `plan` are on offer.

### Come back later

`/quit` (or `ctrl-c`) closes the console and nothing else: the orchestrator and its workers keep running. Run `parl` again in the same repository and you are back where you were, mid-thought if it was working, with any permission prompt that came up meanwhile still waiting for you. `parl --fresh` starts a new orchestrator session instead; `/shutdown` stops everything.

### Choose models

```bash
parl --model opus                # the orchestrator's model; /model switches it live
```

A worker gets `[worker] model` from `~/.parl/config.toml` unless the orchestrator names one. Or let routing pick: `/routing` switches it on and asks for a [TypeSafe](https://docs.typesafe.ai) key, which goes to your operating system's keychain and nowhere else. With routing on, a worker spawned without a model has one chosen from its brief, among the models you allow:

```toml
# ~/.parl/config.toml
[worker]
model = "claude-sonnet-5"

[routing]
enabled = true
models = ["anthropic:claude-opus-5", "anthropic:claude-sonnet-5", "deepseek-v4-flash"]
```

### Keep long sessions small

A session that runs for hours compacts itself: after `[session] auto_compact_turns` turns (60 by default, `0` to turn it off) the orchestrator's context is summarised with claude's own `/compact`, and its transcript file is capped on disk. Older reasoning and tool output fold to one line each; `ctrl-o` unfolds them. `/clear` empties the view, `/trim` shortens the file.

### Script it

The same fleet, without the console:

```bash
parl spawn add-auth -- "Add token refresh to src/auth. Run cargo test. Commit your work."
parl status                      # the fleet table
parl wait add-auth               # exit 0 settled, 3 timed out, 4 stopped/error/dead
parl report add-auth             # the worker's report; exit 2 if there is none
parl diff add-auth
parl merge add-auth              # exit 5 on conflicts, with the checkout left clean
parl cleanup add-auth
```

Every command has `--help`; exit codes are the same numbers the orchestrator branches on.

## Docs

* [Getting started](docs/getting-started.md): install, launch options, the user config, where the state lives
* [The console](docs/console.md): every key and command, the fleet, the palette, permissions, routing, the transcript
* [Headless commands](docs/cli.md): the CLI surface, its exit codes, and how routing chooses
* [AGENTS.md](AGENTS.md): the map for anyone working on the code, human or agent. Module layout, the on-disk contract, what was verified about the pi and claude protocols, and why things are the way they are.
