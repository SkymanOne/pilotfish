# The console

The console is one conversation: the selected session's transcript filling the pane, a composer at the bottom, and a status line. The fleet is an overlay you open when you want it, not a column that takes width away from what you are reading.

```text
 parl · orchestrator + 2 workers

 › add token refresh to the auth module

 ✻ two independent steps, so two workers
   ⋯ 6 more

   I'll split this into `refresh` and its tests.

 ⚙ fleet_spawn add-auth
   → add-auth-1f2e3d4 · parl/add-auth-1f2e3d

 ⚑ settled add-auth  +12 −3

 ╭ orchestrator > ────────────────────────────────╮
 │ ▍                                              │
 ╰────────────────────────────────────────────────╯
 sonnet · 4f2a91cc · $0.42 · 6 turns   ctrl+f fleet · ctrl+k commands
```

## Typing

The composer always has focus, so there is no mode to be in and no letter that a message cannot start with. Type and press `enter`.

| Keys | What they do |
| --- | --- |
| type + `enter` | message the orchestrator, or steer the selected worker |
| `shift-enter` | a newline, not a send — `alt-enter` or `ctrl-j` on a terminal without the kitty keyboard protocol |
| `/` | console commands, and whatever the agent on the other end offers |
| `@` | workers and repository files |
| `tab` | accept the highlighted suggestion |
| `up` / `down` | move through suggestions, or recall what you sent that session before |
| `esc` | close what is open, else clear the line, else stop the orchestrator's turn |

`esc` walks outwards one step at a time: a suggestion popup, then an answer you were composing, then the line itself, then a search highlight. With nothing left to clear it interrupts the orchestrator's turn, which claude resumes from. It never stops a worker — that is `/stop`, or `s` in the fleet — because a key you press reflexively to close things must not be one that throws work away.

Pasting works the way you expect: a multi-line brief pasted into the composer stays one message until you press `enter`, rather than being sent a line at a time.

## The chords

Everything in the conversation that is not text is a `ctrl` chord, and there are only a handful.

| Keys | What they do |
| --- | --- |
| `ctrl-f` | the fleet: every session, and what you can do to the selected one |
| `ctrl-k` | the command palette |
| `ctrl-r` | search this session; press it again to step to the next match |
| `ctrl-o` | unfold an older turn's reasoning and tool output |
| `ctrl-y` | release the mouse so you can select and copy; again takes it back |
| `pgup` / `pgdn` | scroll the transcript (so does the wheel, half a page a notch) |
| `ctrl-home` / `ctrl-end` | the top of the transcript / back to the tail |

`/help` shows all of this in a panel sized to your terminal.

## The fleet

`ctrl-f` opens the whole fleet over the conversation: the orchestrator first, one two-line row per session. The primary line has the state glyph, the name, and for workers the branch and diff stat, with the age on the right. The dimmed second line is what that session is doing right now.

```text
┌ fleet ───────────────────────────────────────────────────────────────────┐
│ parl · orchestrator + 2 workers · ● running 1 · ? needs an answer 1       │
│                                                                          │
│ ▸ ○ orchestrator                                                   3m    │
│     ✻ thinking… 12s                                                      │
│   ● add-auth      parl/add-auth-9123456  +12 −3                    2m    │
│     ⚙ bash                                                               │
│   ? add-tests     parl/add-tests-9123457                           1m    │
│     needs an answer                                                      │
│                                                                          │
│ j/k move · enter open · a answer · s stop · x remove · … · esc back      │
└──────────────────────────────────────────────────────────────────────────┘
```

The glyph carries the state: `○` idle, `…` starting, `●` running, `?` blocked or waiting on you, `✓` done, `■` stopped, `!` failed, `·` archived. The detail line says what the session is in: `✻ thinking… 12s`, `✎ replying…`, `⚙ bash` for a tool call, `needs an answer`, the first line of its error, or `monitor gone` when a worker's monitor is no longer alive.

Single letters are commands in here, because nothing is being typed:

| Keys | What they do |
| --- | --- |
| `j` `k` / arrows | move the selection |
| `g` / `G` | first / last row |
| `1`–`9` | jump to the nth session |
| `enter` | show that session's conversation |
| `esc` | back to the conversation, selection unchanged |
| `a` | answer the pending question or dialog |
| `s` | stop the selected worker |
| `x` | remove the selected worker (asks first) |
| `t` | cycle the thinking level |
| `m` | switch the model (palette, over models) |
| `p` | permission mode (orchestrator only) |
| `b` | the selected session's full brief |
| `?` | help |

## The command palette

`ctrl-k` opens a fuzzy palette over everything the selected session can do, ranked as you type, grouped in this order:

* `console`, the commands the console runs itself, listed below.
* `agent`, whatever the agent on the other end offers, passed through verbatim. For the orchestrator that is Claude Code's slash commands and skills (`/model`, `/usage`, any skill you have installed). For a worker it is pi's commands, skills, prompt templates and extension commands, labelled by source. An entry that takes an argument prefills the composer so you can type it.
* `mcp`, the orchestrator's MCP servers and their tools, with each server's connection status, for reference.
* `models`, the real list from pi for a worker (with the provider named), or the aliases claude accepts for the orchestrator. Selecting one switches the session's model, live.
* `sessions`, to jump to another session.

The lists come from the agent itself, and the console asks it again whenever what it is showing has gone stale — install a skill mid-session and it appears the next time you open the palette. `m` in the fleet overlay opens the palette directly over models.

## Talking to your agents

With the orchestrator selected the composer is a normal message. With a worker selected:

| You type | Short | What happens |
| --- | --- | --- |
| any text | | steers that worker (delivered after its current tool call) |
| `/answer <text>` | `/a` | answers the question or dialog it is blocked on |
| `/followup <text>` | `/f` | queues a message for after it finishes its current work |
| `/stop` | `/s` | aborts it |
| `/remove` | `/rm` | removes it: worktree, branch and fleet row (asks first if that would destroy work) |
| `/thinking <level>` | `/t` | sets the reasoning level: pi's `off…max` for a worker, claude's `low…max` for the orchestrator |
| `/model <model>` | | switches its model, live |
| `/permissions <mode>` | `/perm` | how the orchestrator's tool use is approved. With no argument it says what is in force |
| `/routing` | `/jev` | model routing: switch it on or off, and set the TypeSafe key it uses (see below) |
| `/verbose` | | unfold an older turn's reasoning and tool output, or fold it again |
| `/clear` | | forget this session's transcript in the console; the file on disk is untouched |
| `/trim` | | cut the orchestrator's transcript file down to its recent tail |
| `/mouse` | | the same toggle as `ctrl-y`, for the palette |
| `/help` | `/h` | keys and commands |
| `/quit` | `/q` | leave the console (workers keep running) |
| `/shutdown` | `/sd` | stop the orchestrator and every worker, then exit. Asks first, and worktrees and branches are kept |

`/compact` is deliberately not one of ours: it is claude's own command and passes straight through.

## Copying text

The console captures the mouse so the wheel scrolls the transcript, and that is exactly what stops your terminal from ever seeing a drag — while it is on, the terminal's own click-and-drag selection cannot run.

`ctrl-y` (or `/mouse`) hands the mouse back. Select and copy the way you would in any other program, then press `ctrl-y` again to take it back. The status line says `select` for as long as the mouse is the terminal's, so a wheel that has stopped scrolling is never a mystery, and keyboard scrolling works in both states. The setting is deliberately not remembered across launches.

Most terminals also let you bypass mouse capture by holding a modifier while dragging — `option` in iTerm2 and Terminal.app, `shift` in kitty, Ghostty and WezTerm — which needs no toggle at all.

`/model` and `/thinking` change a running session without restarting it and without spending a turn, on either side. For the orchestrator, claude validates the model name itself, so an unknown one shows claude's own error rather than a list of ours. For a worker the console resolves the id against the models pi reported. An ambiguous or unknown id sends nothing and says so, while an explicit `provider:model` passes straight through.

## Model routing

`/routing` opens a small panel: whether routing is on, where its key comes from, and what it has to choose between. With routing on, a worker spawned without a model has one picked for it from its brief — see [the CLI](cli.md#choosing-a-model-for-a-brief) for how it decides.

```text
╭ model routing ─────────────────────────────────────────────────╮
│ routing   on                                                   │
│ api key   ••••cdef, in the macOS Keychain                       │
│ choosing  between 12 models                                    │
│                                                                │
│ r routing on/off · s set key · d delete key · esc close        │
╰────────────────────────────────────────────────────────────────╯
```

`s` asks for your TypeSafe API key; paste it or type it, and it is drawn as dots, never as text. `enter` saves it to your operating system's credential store — the macOS Keychain, the Windows Credential Manager, or the Secret Service on Linux — and nowhere else: not `~/.parl`, not the transcript, not your history. The panel only ever shows its last four characters. `r` switches routing on or off in `~/.parl/config.toml`, leaving the rest of that file as you wrote it.

A key in `$PARL_TYPESAFE_API_KEY` or `$TYPESAFE_API_KEY` wins over the stored one, and the panel says so. On a machine with no credential store (a headless Linux box without a Secret Service) the environment variable is the way — the key is never written to a file instead. On macOS the first time a newly built `parl` reads the key, the system may ask whether to allow it.

## Permissions

When the orchestrator wants to run something outside its allowlist, or asks you a question, an overlay appears on its own — it is blocking the orchestrator, so it does not wait to be found. It never pops up under your fingers, though: while you are typing it waits for you to finish, so a keystroke meant for your message cannot answer it. `y` allows once, `a` allows it for the session, `n` denies with a reason, and questions get an option picker. Dismissing one with `esc` leaves it pending; the status line keeps counting it, and `a` in the fleet overlay brings it back.

How often that happens is up to you:

| Mode | What it does |
| --- | --- |
| `default` | asks about everything outside the allowlist |
| `auto` | hands routine approvals to a classifier and escalates only what it is unsure about |
| `acceptEdits` | lets file edits and common filesystem commands through |
| `dontAsk` | denies anything not already allowed instead of asking |
| `plan` | makes the orchestrator read-only |

Start in a mode with `parl --permission-mode auto`. The mode shows in the status line whenever it is not the default, survives a console restart, and `p` in the fleet overlay cycles it mid-session. `bypassPermissions` is deliberately not offered, since it would skip the overlay altogether.

## When a worker needs an answer

A worker can block on a question of its own, or on a pi dialog (`select`, `confirm`, `input`, `editor`). Either way the fleet shows it as `needs an answer`, and `a` (or `/answer`) answers it from the console. So can the orchestrator. Nothing stalls: an unanswered dialog is cancelled just before pi's own timeout, and an unanswered question releases the worker after ten minutes to carry on with its own judgment, which it writes down in its report.

## The transcript

The transcript separates the parts of a turn: your prompts in cyan, the model's reasoning dimmed and abridged, its answer as rendered markdown, tool calls in blue with their results dimmed under them, fleet events in yellow, errors in red, each block set off by a blank line. Tool calls are shown as written rather than clipped, so a long command stays readable. Tool output is a preview: the first few lines, then a count of what was left out, since output can run to megabytes.

Replies are drawn once: every line is rendered as markdown the moment it arrives, rather than shown raw and redrawn when the turn ends. Reasoning streams the same way, so it appears while the model is thinking rather than all at once afterwards.

Once a turn is behind you its reasoning and tool output fold to a single row each — `✻ two independent steps  ⋯ 6 more` — because that is the part least worth re-reading. `ctrl-o` (or `/verbose`) unfolds everything; the model's prose, your prompts, fleet events and errors are never folded.

`ctrl-r` searches it. Scrolling follows the tail until you scroll up, then pins there while you read — and stays on the same content as older blocks age out.

A session that runs for hours keeps itself small on its own. The transcript file is capped, and once a session has run `[session] auto_compact_turns` turns (60 by default, `0` to switch it off) the orchestrator's context is compacted with claude's own `/compact`, with a line in the transcript marking the seam. `/clear` forgets a transcript in the console without touching the file; `/trim` shortens the file itself.

Workers disappear from the fleet when they are done: the orchestrator cleans each one up after it merges and verifies it, and the console removes any settled worker whose branch is already merged. Nothing unmerged, dirty or still running is ever removed for you. That waits for `/remove` or `parl cleanup`, which tell you exactly what would be lost before they do anything.
