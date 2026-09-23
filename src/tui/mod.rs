//! The console: a ratatui TUI with an orchestrator-and-workers dashboard,
//! per-session drill-down, and modal (normal/insert) keys. The state half
//! (this module's `app`/`model`/`keys`/`palette`/`completions`/`transcript`)
//! is pure and testable without a terminal; `view/`, `markdown`, `theme` and
//! `runtime` draw it.
//!
//! # For the rendering worker — the view model you consume
//!
//! Own one [`app::Console`] and drive it; everything you draw is a method
//! away. Construction and feeds:
//!
//! - `Console::new(FleetPaths::discover(cwd))`, then `load_prefs()`.
//! - Poll the fleet and feed it: `set_runs(vec![RunEntry { run_id, state }])`
//!   (from `fleet::run::list_runs` + `load_state`), `set_orchestrator_state`
//!   (from `orch::records` state.json), `set_files(list_repo_files().await)`
//!   for `@` completion. `set_diff_stat` is optional garnish for dashboard rows.
//! - Fold transcript records as they arrive: `ingest_orchestrator_record`
//!   (orchestrator `events.jsonl` lines) and `ingest_worker_event(run_id, ev)`
//!   (worker `events.jsonl` lines). On console open, replay with
//!   `Transcript::replay_worker(path)` per run and `apply_orchestrator_record`
//!   over the orchestrator file.
//! - `ingest_fleet_events(events, batch_text)` returns the
//!   `Effect::SendToOrchestrator(batch_text)` you must execute (the watcher's
//!   batch, formatted with `fleet::event::format_fleet_batch`).
//! - `tick(now_ms)` each frame (expires toolbar notes); set
//!   `console.viewport_rows` so half/full-page scrolling knows the pane height.
//!
//! Input and effects:
//!
//! - Feed every key: `let effects = console.handle_key(key_event);` then
//!   `console.execute_all(effects).await;` (ops + mailbox writes live there).
//! - The composer line is `submit`ed for you by Enter; the palette routes
//!   through it too.
//!
//! What to draw, per frame:
//!
//! - `view()` → `Dashboard` or `Session`; `mode()` → normal or insert (the
//!   composer has focus only in insert; draw its prompt via
//!   `composer_prompt()` and text/cursor via `composer()`).
//! - `rows()` → the dashboard: one [`model::DashboardRow`] per session,
//!   orchestrator first — glyph, name, `detail` (what it is doing now), `age`,
//!   `attention` (needs you), branch, diff stat. `selected()` marks the row.
//! - `open_transcript().blocks()` → `Vec<transcript::Block>` (`kind` + text,
//!   markdown raw — run `markdown::render` over `Text` blocks) and
//!   `.partial()` for the in-flight stream. Scroll per `scroll()` (`None` =
//!   follow the tail) and highlight `search()`.
//! - `overlay()` → one of Help (use `keys::help_sections`), Confirm,
//!   Permission (the approval/question picker: `y`/`a`/`n`, options,
//!   "something else"), Palette (`state.items`/`state.visible` +
//!   `state.selected_item()`, grouped by `group`), Search (live query).
//! - Chrome: `activity_line(now)` above the composer, `flash()` for the
//!   toolbar note, and the status line from the selected row plus
//!   `orchestrator_transcript()` facts (`session_id`, `model`, `cost_usd`,
//!   `num_turns`), `effort()`, `orch.permission_mode`, pending approvals.
//!
//! `app::run_app` is the one unwired seam: it constructs nothing yet and
//! waits for `runtime::with_terminal` to own the event loop.

pub mod app;
pub mod completions;
pub mod keys;
pub mod markdown;
pub mod model;
pub mod palette;
pub mod runtime;
pub mod theme;
pub mod transcript;
pub mod view;
