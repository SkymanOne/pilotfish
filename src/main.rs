//! pilotfish — binary entry point: clap parsing plus dispatch to the owning
//! module of every subcommand. Behaviour lives in `ops`, the TUI, the MCP
//! server and the two monitor modules; this file stays a dispatcher.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use clap::Parser as _;
use pilotfish::cli::{Cli, Command, ExitCode};
use pilotfish::{mcp, ops, orch, tui, worker};

/// `--route` / `--no-route` as an override, or `None` for "whatever the
/// config says". clap's `overrides_with` means at most one is set.
const fn route_override(route: bool, no_route: bool) -> Option<bool> {
    match (route, no_route) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

fn main() -> std::process::ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // --help/--version print to stdout and exit 0; parse errors exit 1,
            // matching the TypeScript CLI (unknown command included).
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::Error.into()
            } else {
                ExitCode::Ok.into()
            };
        }
    };
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(dispatch(cli)),
        Err(err) => {
            eprintln!("pilotfish: {err:#}");
            ExitCode::Error.into()
        }
    }
}

/// Dispatch a parsed command to its owning module. Exit-code-bearing results
/// pass through; any error prints and exits 1.
async fn dispatch(cli: Cli) -> std::process::ExitCode {
    match cli.command {
        None => {
            tui_arm(tui::app::TuiOptions {
                cwd: cli.cwd,
                model: cli.model,
                permission_mode: cli.permission_mode,
                remote_control: cli.remote_control,
                fresh: cli.fresh,
                budget: cli.budget,
                progress_events: cli.progress_events,
            })
            .await
        }
        Some(Command::Tui {
            cwd,
            model,
            permission_mode,
            remote_control,
            fresh,
            budget,
            progress_events,
        }) => {
            tui_arm(tui::app::TuiOptions {
                cwd,
                model,
                permission_mode,
                remote_control,
                fresh,
                budget,
                progress_events,
            })
            .await
        }
        Some(Command::Spawn {
            name,
            brief,
            cwd,
            model,
            provider,
            thinking,
            no_worktree,
            base,
            skill,
            append_system_prompt,
            session,
            tools,
            exclude_tools,
            route,
            no_route,
        }) => finish(
            ops::spawn::spawn_run(ops::spawn::SpawnRequest {
                name,
                brief: brief.join(" "),
                cwd,
                model,
                provider,
                thinking,
                worktree: !no_worktree,
                base,
                skill,
                append_system_prompt,
                session,
                tools,
                exclude_tools,
                route: route_override(route, no_route),
            })
            .await,
        ),
        Some(Command::Status {
            name,
            json,
            all,
            cwd,
        }) => finish(ops::query::status(name.as_deref(), cwd.as_deref(), json, all).await),
        Some(Command::Wait { name, timeout, cwd }) => {
            finish(ops::query::wait(&name, cwd.as_deref(), timeout).await)
        }
        Some(Command::Output { name, tail, cwd }) => {
            finish(ops::query::output(&name, cwd.as_deref(), tail).await)
        }
        Some(Command::Logs { name, tail, cwd }) => {
            finish(ops::query::logs(&name, cwd.as_deref(), tail).await)
        }
        Some(Command::Send { name, message, cwd }) => {
            finish(ops::steer::send(&name, cwd.as_deref(), &message.join(" ")).await)
        }
        Some(Command::Followup { name, message, cwd }) => {
            finish(ops::steer::followup(&name, cwd.as_deref(), &message.join(" ")).await)
        }
        Some(Command::Answer {
            name,
            message,
            question,
            cwd,
        }) => finish(
            ops::steer::answer(
                &name,
                cwd.as_deref(),
                question.as_deref(),
                &message.join(" "),
            )
            .await,
        ),
        Some(Command::Stop { name, cwd }) => finish(ops::steer::stop(&name, cwd.as_deref()).await),
        Some(Command::Report { name, cwd }) => {
            finish(ops::query::report(&name, cwd.as_deref()).await)
        }
        Some(Command::Diff {
            name,
            name_only,
            cwd,
        }) => finish(ops::integrate::diff(&name, cwd.as_deref(), name_only).await),
        Some(Command::Merge {
            name,
            no_commit,
            cwd,
        }) => finish(ops::integrate::merge(&name, cwd.as_deref(), no_commit).await),
        Some(Command::Cleanup { target, force, cwd }) => {
            finish(ops::integrate::cleanup(&target, cwd.as_deref(), force).await)
        }
        Some(Command::Attach { name, tail, cwd }) => {
            finish(ops::query::attach(&name, cwd.as_deref(), tail).await)
        }
        Some(Command::Mcp { cwd }) => finish(mcp::server::serve_stdio(cwd.as_deref()).await),
        Some(Command::Monitor { fleet_dir, run }) => {
            finish(worker::monitor::run_monitor(&fleet_dir, &run).await)
        }
        Some(Command::OrchestratorMonitor { fleet_dir, session }) => {
            finish(orch::monitor::run_orchestrator_monitor(&fleet_dir, session).await)
        }
    }
}

async fn tui_arm(options: tui::app::TuiOptions) -> std::process::ExitCode {
    finish(tui::app::run_app(options).await)
}

fn finish(result: anyhow::Result<ExitCode>) -> std::process::ExitCode {
    result
        .unwrap_or_else(|err| {
            eprintln!("pilotfish: {err:#}");
            ExitCode::Error
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(ExitCode::Ok as u8, 0);
        assert_eq!(ExitCode::Error as u8, 1);
        assert_eq!(ExitCode::NoReport as u8, 2);
        assert_eq!(ExitCode::WaitTimeout as u8, 3);
        assert_eq!(ExitCode::RunEndedBadly as u8, 4);
        assert_eq!(ExitCode::MergeConflict as u8, 5);
    }

    #[test]
    fn cli_parses_every_subcommand_surface() {
        use clap::Parser as _;
        // No subcommand: TUI flags land on the root.
        let cli = Cli::try_parse_from(["pilotfish", "--fresh", "--budget", "5", "--cwd", "/tmp"])
            .expect("root flags parse");
        assert!(cli.fresh);
        assert_eq!(cli.budget.as_deref(), Some("5"));
        assert!(cli.command.is_none());

        // `pilotfish tui` explicitly.
        let cli = Cli::try_parse_from(["pilotfish", "tui", "--fresh"]).expect("tui parses");
        assert!(matches!(
            cli.command,
            Some(Command::Tui { fresh: true, .. })
        ));

        // spawn with a `--`-separated brief and its own flags.
        let cli = Cli::try_parse_from([
            "pilotfish",
            "spawn",
            "auth",
            "--model",
            "opus",
            "--no-worktree",
            "--",
            "fix the",
            "tests",
        ])
        .expect("spawn parses");
        match cli.command {
            Some(Command::Spawn {
                name,
                brief,
                model,
                no_worktree,
                ..
            }) => {
                assert_eq!(name, "auth");
                assert_eq!(brief, vec!["fix the", "tests"]);
                assert_eq!(model.as_deref(), Some("opus"));
                assert!(no_worktree);
            }
            other => panic!("{other:?}"),
        }

        // wait --timeout default and explicit.
        let cli = Cli::try_parse_from(["pilotfish", "wait", "auth"]).expect("wait parses");
        match cli.command {
            Some(Command::Wait { timeout, .. }) => assert_eq!(timeout, 600),
            other => panic!("{other:?}"),
        }

        // --remote-control with and without a value.
        let cli = Cli::try_parse_from(["pilotfish", "--remote-control"]).expect("bare rc parses");
        assert_eq!(cli.remote_control.as_deref(), Some(""));
        let cli = Cli::try_parse_from(["pilotfish", "--remote-control", "phone"])
            .expect("named rc parses");
        assert_eq!(cli.remote_control.as_deref(), Some("phone"));

        // The hidden internal monitors parse.
        let cli = Cli::try_parse_from([
            "pilotfish",
            "monitor",
            "--fleet-dir",
            "/x/.pilotfish",
            "--run",
            "auth-20260828141530",
        ])
        .expect("monitor parses");
        assert!(matches!(cli.command, Some(Command::Monitor { .. })));
        let cli = Cli::try_parse_from([
            "pilotfish",
            "orchestrator-monitor",
            "--fleet-dir",
            "/x/.pilotfish",
        ])
        .expect("orchestrator-monitor parses");
        assert!(matches!(
            cli.command,
            Some(Command::OrchestratorMonitor { .. })
        ));
    }

    #[test]
    fn cli_rejects_unknown_commands_and_missing_values() {
        use clap::Parser as _;
        // Unknown subcommand: refusal, exit 1 at dispatch.
        assert!(Cli::try_parse_from(["pilotfish", "install-claude-skill"]).is_err());
        // spawn without any brief still parses; the refusal is spawn's job.
        let cli = Cli::try_parse_from(["pilotfish", "spawn", "x"]).expect("empty brief parses");
        match cli.command {
            Some(Command::Spawn { brief, .. }) => assert!(brief.is_empty()),
            other => panic!("{other:?}"),
        }
        // --run is required for the worker monitor.
        assert!(Cli::try_parse_from(["pilotfish", "monitor", "--fleet-dir", "/x"]).is_err());
    }
}
