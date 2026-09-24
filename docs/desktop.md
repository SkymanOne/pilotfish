# The desktop window

`pilotfish desktop` opens the console in a native window instead of the terminal. It is the same console: the same orchestrator, sessions, commands, prompts and routing, reading and writing the same `.pilotfish` state. Only the presentation differs.

![The desktop window: sessions, the auth-rework session's workers, a worker's transcript, and its diff](../imgs/desktop.png)

## Installing

The window is an optional feature, because it adds a large GUI toolkit (GPUI) to the build.

```bash
xcodebuild -downloadComponent MetalToolchain        # once, on macOS: GPUI compiles its shaders with it
cargo install --locked --git https://github.com/SkymanOne/pilotfish --features desktop
```

macOS is the supported platform. A binary built without the feature explains how to get it when `pilotfish desktop` is run.

## Opening

```bash
cd your-repo
pilotfish desktop
```

It takes the same options as the terminal console (`--cwd`, `--model`, `--permission-mode`, `--remote-control`, `--fresh`, `--budget`, `--progress-events`) and runs in the foreground until the window closes. `ctrl-c` in the launching terminal closes it as ⌘Q does. The desktop and the terminal console share one lock, so only one of them can be open on a repository at a time.

The window reopens the session you used last. With none, it says so and offers **New session**. A session is never started for you.

## Layout

From left to right:

* **Sessions**: every session of the repository, most recently used first. Under each name, a bar per worker in its state's colour. Hover a session for its **⋯** menu. **New session** is at the foot.
* **Workers**: the open session's orchestrator, then its workers, those waiting on you first. The one you are talking to is inked. A running worker's bar streams, and a worker waiting on you shows its question and an **Answer** button. Click a session again to fold this pane away.
* **Conversation**: the session's name as the title, the chips for what it runs on, anything waiting on you, then the transcript with markdown and code highlighted. Clicking a worker adds its tab beside the orchestrator's: that tab shows the worker's transcript, and the input then steers the worker.
* **Changes**: the selected worker's diff against the commit its worktree was cut from. It includes changes the worker has not committed, lists untracked files at the end, and refreshes every few seconds while the worker runs. Files fold with a click, long lines scroll sideways, and lockfiles start folded. With no worker selected, the pane lists each worker's line counts.

Colour means one thing each: ink is running, buoy orange is waiting on you, port red is failed, and grey is finished.

Drag the gap between two panes to resize them, and double-click it to restore the default width. ⌘1, ⌘2 and ⌘3 fold the sessions (to a rail of initials), workers and changes panes, as do the three toggles in the title bar. Widths and folds are remembered per repository.

## Without typing a command

Everything the console does by command can also be clicked:

| Where | What it does |
| --- | --- |
| **New session** (sessions pane, or the empty window) | `/session new [name]`: the name is optional |
| The session's title | Rename it (`/session rename`) |
| A session's **⋯** | Rename it or shut down its orchestrator. For the open session, also show its brief, expand reasoning (⌘O), clear the view or trim the transcript. Remove it with its workers, after a confirmation |
| The chips under the title | Change the model, the thinking level and, for the orchestrator, the permission mode. A worker's model chip opens every model pi offers |
| The waiting strip | **Review** a pending approval or model choice, or **Answer** a worker's question |
| **Answer** on a worker | Opens the worker with the input set to answer it (`/answer`). `esc` goes back |
| **Now** / **After this step** | On a worker's tab: steer it at once (`/send`) or queue the message for when it finishes its step (`/followup`) |
| The square beside send | Stop the orchestrator's turn (`esc` in an empty input does the same) |
| **Merge** on a finished worker | Merge its branch into the checkout it came from, after a confirmation |
| The changes header | **Show in Finder**, **Stop** a running worker, and **⋯**: its brief, copy the branch name, fold every file, remove the worker |

The command palette (⌘K, or **Commands** in the title bar) lists every command and session. **Routing** opens the routing panel.

## The Board

![The Board: every session's workers in four lanes](../imgs/desktop-board.png)

⌘B opens the Board over the window: every session's workers in four lanes, Started, Waiting on you, Failed and Finished. The chips narrow it to one session. Clicking a card switches to that worker's session, selects it and shows its diff. A waiting card's **Answer** and a finished card's **Merge** act on it directly. ⌘B, `esc` or a click outside closes the Board.

## Keys

| Keys | Action |
| --- | --- |
| `enter` / `shift-enter` | Send, or insert a newline |
| `tab` | Accept the highlighted completion (`/` for commands, `@` for workers and files) |
| `esc` | Close a menu or the completions, then clear the input, then leave answer mode, then stop the orchestrator's turn |
| ⌘K | The command palette |
| ⌘F | Search the transcript |
| ⌘B | The Board |
| ⌘1 ⌘2 ⌘3 | Fold sessions, workers, changes |
| ⌘O | Expand older reasoning and tool output |
| ⇧⌘F | The fleet list |
| ⌘/ | Keys and commands |
| ⌘Q | Close the window. Orchestrators and workers keep running |

Permission prompts, questions from `AskUserQuestion`, model choices, confirmations, routing and the shortlist editor open as sheets over the window, with the same keys as in the terminal (`y`, `a`, `n`, arrows, `enter`, `esc`). Every row and button in a sheet can also be clicked. The [console guide](console.md) describes what each of them does.

## Appearance

![The dark theme, answering a worker's question](../imgs/desktop-dark.png)

The window follows macOS: Chart, white sheets on a chart-paper ground, when the system is light, and Deep water when it is dark. Names are set in Fraunces, everything else in Geist, and code and diffs in Geist Mono. All three are bundled with the binary. Sheets and menus rise into place, a running worker's bar streams, and anything waiting on you breathes. With Reduce Motion on in macOS, all of it holds still.
