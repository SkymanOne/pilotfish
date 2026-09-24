# The console

The console presents one conversation: the selected session's transcript fills the screen, with a composer below it and a status line at the bottom. The fleet is an overlay, opened on demand, rather than a permanent column beside the transcript.

![The console: the orchestrator's conversation, with the composer and status line below](../imgs/main.png)

## Typing

The composer always has keyboard focus. There are no modes, and every printable key is text. Type a message and press `enter`.

| Keys | Action |
| --- | --- |
| type + `enter` | Message the orchestrator, or steer the selected worker |
| `shift-enter` | Insert a newline — `alt-enter` or `ctrl-j` on terminals without the kitty keyboard protocol |
| `/` | Console commands, and the commands the connected agent offers |
| `@` | Workers and repository files |
| `tab` | Accept the highlighted suggestion |
| `up` / `down` | Move through suggestions, or recall earlier messages from the session |
| `esc` | Close what is open, then clear the line, then stop the orchestrator's turn |

`esc` works outwards one step at a time: the suggestion popup, an answer being composed, the line itself, and a search highlight. When nothing remains to clear, it interrupts the orchestrator's turn, which claude can resume. It never stops a worker; that requires `/stop` or `s` in the fleet, so that a key pressed reflexively to close something cannot discard work.

A multi-line brief pasted into the composer remains a single message until `enter` is pressed.

## Shortcuts

Every non-text action in the conversation is a `ctrl` chord.

| Keys | Action |
| --- | --- |
| `ctrl-f` | The fleet: every session and the actions available for the selected one |
| `ctrl-k` | The command palette |
| `ctrl-r` | Search the session; press again to move to the next match |
| `ctrl-o` | Expand an older turn's reasoning and tool output |
| `ctrl-y` | Release the mouse to select and copy; press again to recapture it |
| `pgup` / `pgdn` | Scroll the transcript (the mouse wheel scrolls half a page per notch) |
| `ctrl-home` / `ctrl-end` | The top of the transcript / return to the latest output |

`/help` lists all keys and commands in a panel sized to the terminal.

## The fleet

![The fleet overlay: the orchestrator and two running workers](../imgs/fleet.png)

`ctrl-f` opens the fleet over the conversation: the orchestrator first, then one two-line row per worker. The first line holds the state glyph, the name, and for workers the branch and diff statistics, with the session's age on the right. The dimmed second line describes the session's current activity.

The glyph indicates state: `○` idle, `…` starting, `●` running, `?` blocked or waiting for input, `✓` done, `■` stopped, `!` failed, `·` archived. The activity line shows `✻ thinking… 12s`, `✎ replying…`, `⚙ bash` for a tool call, `needs an answer`, the first line of an error, or `monitor gone` when a worker's monitor has stopped.

Within the fleet, single letters are commands:

| Keys | Action |
| --- | --- |
| `j` `k` / arrows | Move the selection |
| `g` / `G` | First / last row |
| `1`–`9` | Select the nth session |
| `enter` | Show that session's conversation |
| `esc` | Return to the conversation, keeping the selection |
| `a` | Answer the pending question, dialog or model choice |
| `s` | Stop the selected worker |
| `x` | Remove the selected worker (asks for confirmation) |
| `t` | Cycle the thinking level |
| `m` | Switch the model (the palette, restricted to models) |
| `p` | Cycle the permission mode (orchestrator only) |
| `b` | Show the selected session's full brief |
| `?` | Help |

## The command palette

`ctrl-k` opens a fuzzy palette over everything the selected session can do, ranked as you type and grouped as follows:

* `console`: commands the console handles itself, listed below.
* `agent`: commands offered by the connected agent, passed through unchanged. For the orchestrator these are Claude Code's slash commands and skills (`/model`, `/usage`, and any installed skill). For a worker they are pi's commands, skills, prompt templates and extension commands, labelled by source. Entries that take an argument prefill the composer.
* `mcp`: the orchestrator's MCP servers and their tools, with each server's connection status, for reference.
* `models`: the models pi reports for a worker (with the provider named), or the aliases claude accepts for the orchestrator. Selecting one switches the session's model immediately.
* `sessions`: other sessions to switch to.

These lists are requested from the agent itself and refreshed whenever they become stale, so a skill installed during a session appears the next time the palette opens. `m` in the fleet opens the palette restricted to models.

## Commands

With the orchestrator selected, the composer sends ordinary messages. With a worker selected, text is delivered to that worker. The following commands are available:

| Command | Short | Effect |
| --- | --- | --- |
| any text | | Steer the selected worker (delivered after its current tool call) |
| `/answer <text>` | `/a` | Answer the question or dialog the worker is blocked on |
| `/followup <text>` | `/f` | Queue a message for after the worker finishes its current work |
| `/stop` | `/s` | Abort the worker |
| `/remove` | `/rm` | Remove the worker, its worktree, branch and fleet row (asks first if work would be lost) |
| `/thinking <level>` | `/t` | Set the reasoning level: pi's `off…max` for a worker, claude's `low…max` for the orchestrator |
| `/model <model>` | | Switch the model without restarting |
| `/permissions <mode>` | `/perm` | Set how the orchestrator's tool use is approved; without an argument, show the current mode |
| `/routing` | `/jev` | Model routing: enable or disable it, set the API key, the shortlist and the confidence limit (see below) |
| `/verbose` | | Expand or collapse older turns' reasoning and tool output |
| `/clear` | | Clear this session's transcript from the console; the file on disk is unchanged |
| `/trim` | | Shorten the orchestrator's transcript file to its recent tail |
| `/mouse` | | Toggle mouse capture, as `ctrl-y` does |
| `/help` | `/h` | Keys and commands |
| `/quit` | `/q` | Close the console (workers keep running) |
| `/shutdown` | `/sd` | Stop the orchestrator and every worker, then exit. Asks first; worktrees and branches are kept |

`/compact` is intentionally not a console command: it belongs to claude and is passed through unchanged.

`/model` and `/thinking` take effect on a running session without a restart and without spending a turn, on either side. For the orchestrator, claude validates the model name, so an unknown name produces claude's own error message. For a worker, the console resolves the id against the models pi reported: an ambiguous or unknown id is rejected with an explanation, while an explicit `provider:model` is passed through.

## Copying text

The console captures the mouse so that the wheel scrolls the transcript. While capture is on, the terminal does not receive drag events, so its own text selection is unavailable.

`ctrl-y` (or `/mouse`) releases the mouse. Select and copy text as in any other program, then press `ctrl-y` again to recapture it. The status line shows `select` while the mouse is released, and keyboard scrolling works in both states. The setting is not persisted across launches.

Most terminals also bypass mouse capture while a modifier is held during a drag — `option` in iTerm2 and Terminal.app, `shift` in kitty, Ghostty and WezTerm — which requires no toggle.

## Model routing

`/routing` opens the routing panel. It shows whether routing is enabled, where the API key comes from, how many models routing chooses from, the shortlist, and the confidence limit below which you are asked to choose. With routing enabled, a worker spawned without a model has its model and reasoning level chosen from its brief; [the CLI reference](cli.md#routing-a-brief) describes how.

```text
╭ model routing ───────────────────────────────────────────────────────╮
│ routing   on                                                         │
│ api key   ••••cdef, in ~/.pilotfish/config.toml                      │
│ choosing  between 12 models                                          │
│ shortlist 12 models                                                  │
│ ask me    when jev is less than 60% sure of the model                │
│                                                                      │
│ r routing on/off · s set key · m shortlist · -/+ ask limit ·         │
│ d delete key · esc close                                             │
╰──────────────────────────────────────────────────────────────────────╯
```

| Key | Action |
| --- | --- |
| `r` | Enable or disable routing |
| `s` | Enter the TypeSafe API key |
| `d` | Remove the saved key (asks for confirmation) |
| `m` | Edit the shortlist |
| `-` / `+` | Lower or raise the confidence limit in steps of 5% (0% never asks, 100% always asks) |

All settings are written to `~/.pilotfish/config.toml`, and the rest of that file, including comments, is left as it was.

**The API key.** The key is read from `$TYPESAFE_API_KEY` when it is set, and otherwise from `[routing] api_key` in `~/.pilotfish/config.toml`. When neither holds one, the console asks for it: the panel opens directly into key entry when routing is enabled at start-up, when `/routing` is opened, and when routing is switched on. `s` enters a key at any time. The key can be pasted or typed and is displayed as dots; `enter` saves it to `~/.pilotfish/config.toml`, which is written readable only by you (mode 600). It never appears in the transcript or the command history, and the panel displays only its last four characters. An environment key takes precedence over the saved one, and the panel indicates which is in use.

**The shortlist.** `m` lists every model in pi's catalogue as `provider:id`, with its name and price. Typing filters the list, `up` and `down` move, `enter` adds or removes the selected model, and `esc` saves the shortlist and returns to the panel. One routing decision can weigh at most 255 models, so the shortlist cannot exceed 255 entries. Entries in the configuration that no longer match any model in the catalogue are preserved. The catalogue is recorded when a worker starts, so the shortlist can be edited once at least one worker has run.

## Choosing a model

When routing is less confident about a worker's model than your limit, a prompt opens on its own, as a permission prompt does, because the spawn is waiting for the answer. It lists every shortlisted model with Jev's preference first, the probability Jev assigned to each, and its price relative to the cheapest option.

| Key | Action |
| --- | --- |
| `j` `k` / arrows | Move the selection |
| `enter` | Use the selected model |
| `1`–`9` | Use the nth model |
| `d` | Keep the configured model |
| `esc` | Answer later; `a` on the orchestrator's row in the fleet reopens the prompt |

The status line counts pending model choices. Without an answer within ten minutes, the spawn continues on the configured model. Once the model is settled, the reasoning level is chosen for it automatically.

## Permissions

When the orchestrator requests an action outside its allowlist, or asks a question, a prompt opens on its own, since the orchestrator is blocked until it is answered. It does not open while you are typing, so a keystroke intended for a message cannot answer it. `y` allows the action once, `a` allows it for the session, `n` denies it with a reason, and questions present a list of options. Dismissing the prompt with `esc` leaves it pending; the status line continues to count it, and `a` in the fleet reopens it.

The frequency of these prompts depends on the permission mode:

| Mode | Behaviour |
| --- | --- |
| `default` | Asks about every action outside the allowlist |
| `auto` | Passes routine approvals to a classifier and escalates only uncertain cases |
| `acceptEdits` | Allows file edits and common filesystem commands |
| `dontAsk` | Denies any action not already allowed, instead of asking |
| `plan` | Makes the orchestrator read-only |

Start in a given mode with `pilotfish --permission-mode auto`. The status line shows the mode whenever it is not the default; the mode persists across console restarts, and `p` in the fleet cycles it during a session. `bypassPermissions` is not offered, because it would bypass the prompt entirely.

## Worker questions

A worker can block on its own question or on a pi dialog (`select`, `confirm`, `input` or `editor`). In both cases the fleet shows `needs an answer`, and `a` (or `/answer`) answers it from the console; the orchestrator can also answer it. Nothing stalls indefinitely: an unanswered dialog is cancelled shortly before pi's own timeout, and an unanswered question releases the worker after ten minutes to proceed on its own judgment, which it records in its report.

## The transcript

The transcript distinguishes the parts of a turn: your prompts in cyan, the model's reasoning dimmed and abridged, its answer as rendered markdown, tool calls in blue with their results dimmed beneath, fleet events in yellow, and errors in red, each separated by a blank line. Tool calls are shown in full so that long commands remain readable. Tool output is shown as a preview — the first few lines and a count of the remainder — since output can be very large.

Replies are rendered once: each line is rendered as markdown as it arrives, rather than displayed raw and redrawn at the end of the turn. Reasoning is streamed in the same way.

When a turn is complete, its reasoning and tool output collapse to a single row each, such as `✻ two independent steps  ⋯ 6 more`. `ctrl-o` (or `/verbose`) expands them. The model's prose, your prompts, fleet events and errors are never collapsed.

`ctrl-r` searches the transcript. Scrolling follows the latest output until you scroll up, then holds its position, and stays on the same content as older blocks are discarded.

Long sessions manage their own size. The transcript file is capped, and after `[session] auto_compact_turns` turns (60 by default; `0` disables it) the orchestrator's context is compacted with claude's `/compact`, with a line in the transcript marking the point. `/clear` clears a transcript from the console without changing the file; `/trim` shortens the file itself.

Workers are removed from the fleet when they are finished: the orchestrator removes each worker after merging and verifying its work, and the console removes any settled worker whose branch has already been merged. Unmerged, modified or running workers are never removed automatically; that requires `/remove` or `pilotfish cleanup`, both of which report exactly what would be lost before acting.
