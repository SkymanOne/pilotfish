---
name: fleet-worker-report
description: How to write the final report for a pilotfish fleet worker run: the exact markdown template, what each section needs, and how to reflect steering you received mid-run. Use whenever PILOTFISH_RUN is set or a task brief asks for a fleet report.
---

# Fleet worker report

You are running as a worker for `pilotfish`. The orchestrator never reads this chat. It reads one file, `$PILOTFISH_DIR/runs/$PILOTFISH_RUN/report.md`, and nothing else. Write that file before your final turn, every time, even when the task failed or you are blocked. A run without a report looks the same as a run that did nothing.

## Template (copy it exactly, keep every heading in this order)

```markdown
# Fleet Report: <run name>

## Status

done | blocked | failed

## Summary

(3-8 sentences: what was accomplished and the outcome)

## What I did

(numbered steps actually taken)

## Files changed

(path: one-line reason, from your actual edits)

## Verification

(command run → result, for each check performed)

## Decisions & assumptions

(any choice made without explicit instruction)

## Steering received

(mid-run course corrections you were given and how you handled them; "none" if none)

## Open questions for orchestrator

(things you could not resolve; empty if none, REQUIRED if Status: blocked)

## Suggested next step

(one concrete next action for the orchestrator)
```

## What goes in each section

Status is exactly one word. Use `done` only when the definition of done in your brief is met and you verified it. Use `blocked` when you need a decision or an input you do not have; Open questions is then mandatory. Use `failed` when you tried and could not make it work.

Summary: the outcome first, then how you got there. Skip the dead ends unless they matter to the orchestrator.

What I did: numbered, past tense, concrete. "Added `parseArgs()` in src/cli.ts", not "planned to add parsing".

Files changed: one line per file, taken from your real edits. `git status` and `git diff --stat` are the source of truth. Write "none" if you changed nothing.

Verification: each check as `command → result`. If you ran nothing, say so. Never invent a result.

Decisions & assumptions: anything the brief left open that you decided on your own. The orchestrator reviews these.

Steering received: every steering or follow-up message you got, roughly when it arrived relative to your work, and what changed because of it. The orchestrator compares this section with its own steering log. Write `none` if you received nothing. Status and Verification must describe the work as it finally ended up after steering, not the original plan.

Open questions for orchestrator: precise questions someone can answer. Leave it empty when nothing is open.

Suggested next step: one action, such as "merge the branch" or "re-run with X clarified".

## Rules

- Stay inside your working directory. Never run `git merge`, never touch the parent checkout, never push.
- Commit your work in your worktree when the brief asks for commits. The orchestrator does the merging.
- On long tasks, record one-line milestones with `fleet_progress` so the orchestrator and the human console see them live.

## Talking to the orchestrator mid-run

Two tools are registered for fleet workers:

- `fleet_ask` (`question`, optional `options`, optional `context`): ask the orchestrator a question and wait for the answer. Use it when you are blocked on a decision or a missing input instead of guessing. The call returns the answer text, or, if nobody answers in time, a note telling you to proceed on your own judgment; record that choice under "Decisions & assumptions". While you wait, steering messages are delivered only after the call returns.
- `fleet_progress` (`message`): record a one-line milestone. The orchestrator and the human console see it immediately.
