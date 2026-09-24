# The desktop window

`pilotfish desktop` opens the console in a native window instead of the terminal. It is the same console: the same orchestrator, sessions, commands, prompts and routing, reading and writing the same `.pilotfish` state. Only the presentation differs.

![The desktop window: sessions, the auth-rework session's workers, the orchestrator's plan, and the selected worker's diff](../imgs/desktop.png)

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

## Layout

From left to right:

* **Sessions**: every session of the repository, most recently used first. The light is the session's orchestrator: green running, yellow stalled, a dashed ring stopped. The small lights are its workers. `+` starts a new session.
* **Workers**: the open session's orchestrator, then its workers, those waiting on you first. Click a session again to fold this pane away.
* **Chat**: the conversation, with markdown and code highlighted. Clicking a worker adds its tab beside the orchestrator's: that tab shows the worker's transcript, and the input then steers the worker.
* **Changes**: the selected worker's diff against the commit its worktree was cut from. It includes changes the worker has not committed, lists untracked files at the end, and refreshes every few seconds while the worker runs. Files fold with a click, long lines scroll sideways, and lockfiles start folded. With no worker selected, the pane lists each worker's line counts.

Status uses ship's lights throughout: green for running, yellow for waiting on you, red for failed, and a white ring once a worker has finished.

Drag any border to resize a pane; double-click it to restore the default width. ⌘1, ⌘2 and ⌘3 fold the sessions (to a rail of lights), workers and changes panes. Widths and folds are remembered per repository.

## The Board

![The Board: every session's workers in four lanes](../imgs/desktop-board.png)

⌘B opens the Board over the window: every session's workers in four lanes, Started, Waiting on you, Failed and Finished. The chips narrow it to one session. Clicking a card switches to that worker's session, selects it and shows its diff. ⌘B or `esc` closes the Board.

## Keys

| Keys | Action |
| --- | --- |
| `enter` / `shift-enter` | Send, or insert a newline |
| `tab` | Accept the highlighted completion (`/` for commands, `@` for workers and files) |
| `esc` | Close the completions, then clear the input, then stop the orchestrator's turn |
| ⌘K | The command palette |
| ⌘F | Search the transcript |
| ⌘B | The Board |
| ⌘1 ⌘2 ⌘3 | Fold sessions, workers, changes |
| ⌘O | Expand older reasoning and tool output |
| ⇧⌘F | The fleet list |
| ⌘/ | Keys and commands |
| ⌘Q | Close the window; orchestrators and workers keep running |

Permission prompts, questions from `AskUserQuestion`, model choices, confirmations, routing and the shortlist editor open as sheets over the window, with the same keys as in the terminal (`y`, `a`, `n`, arrows, `enter`, `esc`). Every row and button in a sheet can also be clicked. The [console guide](console.md) describes what each of them does.

## Appearance

The window follows macOS: Night bridge when the system is dark, Chart table when it is light. The fonts are IBM Plex Sans Condensed and IBM Plex Mono, bundled with the binary.
