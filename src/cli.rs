//! The command-line surface: every subcommand and flag, with the exit codes
//! the scripts and the orchestrator depend on. This file only parses and
//! dispatches — behaviour lives in `ops` (and the TUI, MCP and monitor
//! modules), so later steps never need to touch it.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use uuid::Uuid;

/// Exit codes, preserved exactly: 0 ok, 1 refusal or error, 2 no report,
/// 3 wait timed out, 4 the run ended stopped/error/dead, 5 merge conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Ok = 0,
    Error = 1,
    NoReport = 2,
    WaitTimeout = 3,
    RunEndedBadly = 4,
    MergeConflict = 5,
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        Self::from(code as u8)
    }
}

/// pilotfish — a fleet of headless pi workers orchestrated by Claude Code.
///
/// With no subcommand this opens the TUI console.
#[derive(Debug, Parser)]
#[command(
    name = "pilotfish",
    version,
    about = "A fleet of headless pi workers orchestrated by Claude Code: run `pilotfish` for the TUI, or drive workers with the subcommands."
)]
pub struct Cli {
    /// Target directory (default: current).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Model for the orchestrator (claude model alias or id).
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// How the orchestrator's tool use is approved: default, auto,
    /// acceptEdits, dontAsk, plan.
    #[arg(long, value_name = "MODE")]
    pub permission_mode: Option<String>,

    /// Put the orchestrator on Claude Code Remote Control.
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub remote_control: Option<String>,

    /// Start a new orchestrator session instead of resuming the saved one.
    #[arg(long)]
    pub fresh: bool,

    /// Stop the orchestrator after this much spend.
    #[arg(long, value_name = "USD")]
    pub budget: Option<String>,

    /// Forward worker progress notes to the orchestrator.
    #[arg(long)]
    pub progress_events: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The subcommands. The two internal monitors are hidden from help.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// open the fleet console (default)
    Tui {
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Model for the orchestrator (claude model alias or id).
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// How the orchestrator's tool use is approved.
        #[arg(long, value_name = "MODE")]
        permission_mode: Option<String>,
        /// Put the orchestrator on Claude Code Remote Control.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        remote_control: Option<String>,
        /// Start a new orchestrator session instead of resuming the saved one.
        #[arg(long)]
        fresh: bool,
        /// Stop the orchestrator after this much spend.
        #[arg(long, value_name = "USD")]
        budget: Option<String>,
        /// Forward worker progress notes to the orchestrator.
        #[arg(long)]
        progress_events: bool,
    },

    /// start a headless pi worker (git worktree by default; --no-worktree for read-only tasks)
    Spawn {
        /// Worker name; run/branch-safe names are derived from it.
        name: String,
        /// The worker's brief (everything after `--`).
        #[arg(last = true)]
        brief: Vec<String>,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// pi model pattern.
        #[arg(long, value_name = "PATTERN")]
        model: Option<String>,
        /// pi provider.
        #[arg(long, value_name = "NAME")]
        provider: Option<String>,
        /// Thinking level.
        #[arg(long, value_name = "LEVEL")]
        thinking: Option<String>,
        /// Run in place without a git worktree.
        #[arg(long)]
        no_worktree: bool,
        /// Base ref for the worker branch (default: HEAD).
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// Load an extra pi skill file or directory.
        #[arg(long, value_name = "PATH")]
        skill: Option<String>,
        /// Append to the pi system prompt.
        #[arg(long, value_name = "TEXT")]
        append_system_prompt: Option<String>,
        /// Resume a previous pi session.
        #[arg(long, value_name = "PATH_OR_ID")]
        session: Option<String>,
        /// pi tool allowlist.
        #[arg(long, value_name = "LIST")]
        tools: Option<String>,
        /// pi tool denylist.
        #[arg(long, value_name = "LIST")]
        exclude_tools: Option<String>,
        /// Let Jev choose the model, thinking level and worktree from the
        /// brief, overriding `[routing] enabled`.
        #[arg(long, overrides_with = "no_route")]
        route: bool,
        /// Choose nothing; spawn with the configured defaults.
        #[arg(long, overrides_with = "route")]
        no_route: bool,
    },

    /// fleet table, or one run's full state as JSON
    Status {
        /// A run's name or id; omit for the whole fleet.
        name: Option<String>,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Include archived runs.
        #[arg(long)]
        all: bool,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// block until the run settles (exit 0), times out (3), or ends stopped/error/dead (4)
    Wait {
        name: String,
        /// Seconds to wait (default 600).
        #[arg(long, value_name = "SEC", default_value_t = 600)]
        timeout: u64,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// last assistant text, or the last n tool results with --tail
    Output {
        name: String,
        /// Print the last n tool results instead.
        #[arg(long, value_name = "N")]
        tail: Option<usize>,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// tail the captured raw RPC stream
    Logs {
        name: String,
        /// Lines to print (default 50).
        #[arg(long, value_name = "N")]
        tail: Option<usize>,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// steer a running worker (delivered after its current tool calls)
    Send {
        name: String,
        /// The steering message.
        #[arg(last = true)]
        message: Vec<String>,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// queue a message for after the worker finishes its current work
    Followup {
        name: String,
        /// The follow-up message.
        #[arg(last = true)]
        message: Vec<String>,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// answer the worker's pending fleet_ask question (default: the question it is blocked on)
    Answer {
        name: String,
        /// The answer.
        #[arg(last = true)]
        message: Vec<String>,
        /// Question id to answer.
        #[arg(long, value_name = "ID")]
        question: Option<String>,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// abort a running worker (state becomes stopped)
    Stop {
        name: String,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// the worker's final report (or last assistant text) plus the steering log; exit 2 if none
    Report {
        name: String,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// the worker's changes vs its base commit (git diff --stat, or --name-only)
    Diff {
        name: String,
        /// List changed files only.
        #[arg(long)]
        name_only: bool,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// merge the settled worker's branch into the current checkout (exit 5 on conflicts)
    Merge {
        name: String,
        /// Stage the merge without committing (--no-commit --no-ff).
        #[arg(long)]
        no_commit: bool,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// remove a run's worktree + branch and archive it (<name> or all; --force also discards unmerged branches and uncommitted changes, and never aborts a run that `all` did not explicitly name)
    Cleanup {
        /// A run's name or id, or `all`.
        target: String,
        /// Discard unmerged branches and uncommitted changes; never aborts a run that `all` did not explicitly name.
        #[arg(long)]
        force: bool,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// print the tail of one worker's transcript (the live console is `pilotfish`)
    Attach {
        name: String,
        /// Lines to print (default 40).
        #[arg(long, value_name = "N")]
        tail: Option<usize>,
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// serve the fleet tools over stdio as an MCP server (the TUI's orchestrator uses this)
    Mcp {
        /// Target directory (default: current).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },

    /// the worker monitor (internal; spawned detached)
    #[command(hide = true)]
    Monitor {
        /// The fleet state directory.
        #[arg(long, value_name = "DIR")]
        fleet_dir: PathBuf,
        /// The run to monitor.
        #[arg(long, value_name = "RUN_ID")]
        run: String,
    },

    /// the orchestrator monitor (internal; spawned detached)
    #[command(hide = true)]
    OrchestratorMonitor {
        /// The fleet state directory.
        #[arg(long, value_name = "DIR")]
        fleet_dir: PathBuf,
        /// The session uuid this monitor serves; the most recently used session when absent.
        #[arg(long, value_name = "UUID")]
        session: Option<Uuid>,
    },
}
