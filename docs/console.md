# The console

The console is a dashboard with drill-down, and it has two key modes.

## The dashboard

The home view: the whole fleet at a glance, the orchestrator first, one two-line row per session. The primary line has the state glyph, the name, and for workers the branch and diff stat, with the age on the right. The dimmed second line is what the session is doing right now. Nothing is clipped to a narrow column, so a dozen workers stay readable.

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ parl · orchestrator + 2 workers · ● running 1 · ? needs an answer 1      │
│                                                                          │
│ ▸ ○ orchestrator                                                   3m    │
│     ✻ thinking… 12s                                                      │
│   ● add-auth      parl/add-auth-9123456  +12 −3                    2m    │
│     ⚙ bash                                                               │
│ ? add-tests       parl/add-tests-9123457                           1m    │
│     needs an answer                                                      │
│                                                                          │
│ j/k move · enter open · a answer · s stop · i compose · : palette · …    │
├ add-tests · needs an answer · sonnet-4.5 · high · parl/add-tests-9123457 ┤
└──────────────────────────────────────────────────────────────────────────┘
```

The glyph carries the state: `○` idle, `…` starting, `●` running, `?` blocked or waiting on you, `✓` done, `■` stopped, `!` failed, `·` archived. The detail line says what the session is in: `✻ thinking… 12s`, `✎ replying…`, `⚙ bash` for a tool call, `needs an answer`, the first line of its error, or `monitor gone` when a worker's monitor is no longer alive. The bottom row describes whatever is selected. For a worker that is its state, model, reasoning level and branch (what pi resolved, not just the pattern you asked for). For the orchestrator it is the model, session, spend and turns.

`enter` opens the selected session's drill-down: a slim session list on the left, the transcript filling the rest, the composer below it. `esc` goes back to the dashboard.

## The two modes

In normal mode the composer does not have focus, so single-letter keys are free. Any printable key that binds to nothing starts a message and switches to insert mode keeping the key, so starting to type is never punished. `i` gets you there without typing anything.

In insert mode the composer has focus and types freely. Only its own keys are bound, and `esc` comes back.

Keys in normal mode:

| Keys | What they do |
| --- | --- |
| `j` `k` / arrows | move the selection |
| `g` / `G` | first / last row, or top / bottom of the transcript |
| `enter` | open the selected session |
| `esc` | back to the dashboard |
| `tab` / `shift-tab` | next / previous session |
| `1`–`9` | jump to the nth session |
| `/` | search this session (`n`/`N` next / previous match) |
| `:` or `ctrl-k` | the command palette |
| `?` | help |
| `a` | answer the pending question or dialog |
| `s` | stop the selected worker |
| `x` | remove the selected worker (asks first) |
| `t` | cycle the thinking level |
| `m` | switch the model (palette, over models) |
| `p` | permission mode (orchestrator only) |
| `v` | release the mouse so you can select and copy; `v` again takes it back |
| `ctrl-d` / `ctrl-u` | scroll half a page down / up |
| `ctrl-f` / `ctrl-b` | scroll a page down / up |
| `q` | close the console (workers keep running) |
| `Q` | stop the orchestrator and every worker, then exit (asks first) |

Keys in insert mode:

| Keys | What they do |
| --- | --- |
| type + `enter` | message the orchestrator, or steer the selected worker |
| `shift-enter` | a newline, not a send — `alt-enter` or `ctrl-j` on a terminal without the kitty keyboard protocol |
| `/` | commands and skills |
| `@` | workers and repository files |
| `tab` | accept the highlighted suggestion |
| `up` / `down` | move through suggestions, or recall what you sent that session before |
| `ctrl-k` | the command palette |
| `esc` | back to normal mode |

## The command palette

`:` or `ctrl-k` opens a fuzzy palette over everything the selected session can do, ranked as you type, grouped in this order:

* `console`, the commands the console runs itself, listed below.
* `agent`, whatever the agent on the other end offers, passed through verbatim. For the orchestrator that is Claude Code's slash commands and skills (`/model`, `/usage`, any skill you have installed). For a worker it is pi's commands, skills, prompt templates and extension commands, labelled by source. An entry that takes an argument prefills the composer so you can type it.
* `mcp`, the orchestrator's MCP servers and their tools, with each server's connection status, for reference.
* `models`, the real list from pi for a worker (with the provider named), or the aliases claude accepts for the orchestrator. Selecting one switches the session's model, live.
* `sessions`, to jump to another dashboard row.

`m` opens the palette directly over models.

## Talking to your agents

With the orchestrator selected the composer is a normal message. With a worker selected:

| You type | Short | What happens |
| --- | --- | --- |
| any text | | steers that worker (delivered after its current tool call) |
| `/answer <text>` | `/a` | answers the question or dialog it is blocked on |
| `/followup <text>` | `/f` | queues a message for after it finishes its current work |
| `/stop` | `/s` | aborts it |
| `/remove` | `/rm` | removes it: worktree, branch and dashboard row (asks first if that would destroy work) |
| `/thinking <level>` | `/t` | sets the reasoning level: pi's `off…max` for a worker, claude's `low…max` for the orchestrator |
| `/model <model>` | | switches its model, live |
| `/permissions <mode>` | `/perm` | how the orchestrator's tool use is approved. With no argument it says what is in force |
| `/rail <mode>` | `/rw` | width of the drill-down's session list: `compact`, `auto`, `wide`, or `full` |
| `/mouse` | | the same toggle as `v`, for the palette |
| `/clear` | | forget this session's transcript in the console; the file on disk is untouched |
| `/trim` | | cut the orchestrator's transcript file down to its recent tail |
| `/help` | `/h` | keys and commands |
| `/quit` | `/q` | leave the console (workers keep running) |
| `/shutdown` | `/sd` | stop the orchestrator and every worker, then exit. Asks first, and worktrees and branches are kept |

## Copying text

The console captures the mouse so the wheel scrolls the transcript, and that is exactly what stops your terminal from ever seeing a drag — while it is on, the terminal's own click-and-drag selection cannot run.

`v` (or `/mouse`) hands the mouse back. Select and copy the way you would in any other program, then press `v` again to take it back. The status line says `select` for as long as the mouse is the terminal's, so a wheel that has stopped scrolling is never a mystery, and keyboard scrolling (`j`/`k`, `ctrl-d`/`ctrl-u`, `ctrl-f`/`ctrl-b`, `g`/`G`) works in both states. The setting is deliberately not remembered across launches.

Most terminals also let you bypass mouse capture by holding a modifier while dragging — `option` in iTerm2 and Terminal.app, `shift` in kitty, Ghostty and WezTerm — which needs no toggle at all.

`/model` and `/thinking` change a running session without restarting it and without spending a turn, on either side. For the orchestrator, claude validates the model name itself, so an unknown one shows claude's own error rather than a list of ours. For a worker the console resolves the id against the models pi reported. An ambiguous or unknown id sends nothing and says so, while an explicit `provider:model` passes straight through.

## Permissions

When the orchestrator wants to run something outside its allowlist, or asks you a question, an overlay appears: `y` allows once, `a` allows it for the session, `n` denies with a reason, and questions get an option picker.

How often that happens is up to you:

| Mode | What it does |
| --- | --- |
| `default` | asks about everything outside the allowlist |
| `auto` | hands routine approvals to a classifier and escalates only what it is unsure about |
| `acceptEdits` | lets file edits and common filesystem commands through |
| `dontAsk` | denies anything not already allowed instead of asking |
| `plan` | makes the orchestrator read-only |

Start in a mode with `parl --permission-mode auto`. The mode shows in the status line whenever it is not the default, survives a console restart, and `p` cycles it mid-session. `bypassPermissions` is deliberately not offered, since it would skip the overlay altogether.

## When a worker needs an answer

A worker can block on a question of its own, or on a pi dialog (`select`, `confirm`, `input`, `editor`). Either way the dashboard shows it as `needs an answer`, and `a` (or `/answer`) answers it from the console. So can the orchestrator. Nothing stalls: an unanswered dialog is cancelled just before pi's own timeout, and an unanswered question releases the worker after ten minutes to carry on with its own judgment, which it writes down in its report.

## The transcript

The transcript separates the parts of a turn: your prompts in cyan, the model's reasoning dimmed and abridged, its answer as rendered markdown, tool calls in blue with their results dimmed under them, fleet events in yellow, errors in red, each block set off by a blank line. Tool calls are shown as written rather than clipped, so a long command stays readable. Tool output is a preview: the first few lines, then a count of what was left out, since output can run to megabytes. `/` searches it. Scrolling follows the tail until you scroll up, then pins there while you read — and it stays on the same content as older blocks age out.

Replies are drawn once: each line is rendered as markdown the moment it arrives, rather than shown raw and redrawn when the turn ends. Reasoning streams the same way, so it appears while the model is thinking rather than all at once afterwards.

A session that runs for hours keeps itself small on its own. The transcript file is capped, and once a session has run `[session] auto_compact_turns` turns (60 by default, `0` to switch it off) the orchestrator's context is compacted with claude's own `/compact`, with a line in the transcript marking the seam. `/clear` forgets a transcript in the console without touching the file; `/trim` shortens the file itself.

Workers disappear from the dashboard when they are done: the orchestrator cleans each one up after it merges and verifies it, and the console removes any settled worker whose branch is already merged. Nothing unmerged, dirty or still running is ever removed for you. That waits for `/remove` or `parl cleanup`, which tell you exactly what would be lost before they do anything.
