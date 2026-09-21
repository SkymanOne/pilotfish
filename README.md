# parl

A fleet of [pi](https://github.com/earendil-works/pi-mono) coding agents in your terminal, with Claude Code as the orchestrator.

You describe the work. The orchestrator plans it and spawns pi workers, each in its own git worktree. You see both sides at once: the conversation on one, the whole fleet on the other.

![chat window](imgs/main.png)

* **Watch and interrupt anything.** Drill into a worker, steer it, or answer its question yourself. The orchestrator is told what you did and works with it instead of undoing it.
* **The console is disposable.** Every agent runs under a detached monitor that keeps its state on disk, so you can close the console mid-run and reopen it where you left off.
* **The orchestrator never types code.** It reads, plans, merges and verifies. `Edit` and `Write` are disabled for it. Workers do the writing, each on its own branch.
* **Nothing to install on either agent.** No skill, no plugin: the orchestrator is a plain `claude -p` process this app owns, and the pi worker extension ships inside the binary.
* **Scriptable.** The TUI is one client. `parl spawn`, `status`, `merge` and friends drive the same fleet from a shell.

## Quick start

You need a current Rust toolchain, `pi` on your PATH, and `claude` (Claude Code 2.1.x) logged in.

```bash
cargo install --git https://github.com/SkymanOne/parl
cd your-repo
parl
```

Then say what you want: *"Add token refresh to the auth module and update the tests."*

## Docs

* [Getting started](docs/getting-started.md): install, first run, launch options, where the state lives
* [The console](docs/console.md): the conversation, the fleet overlay, keys, palette, permissions, transcript
* [Headless commands](docs/cli.md): the CLI surface and its exit codes
* [AGENTS.md](AGENTS.md): the map for anyone working on the code, human or agent. Module layout, the on-disk contract, what was verified about the pi and claude protocols, and why things are the way they are.
