//! `pilotfish desktop`: the console in a macOS window, drawn with GPUI.
//!
//! Two halves. The tokio half ([`backend`]) runs the same [`Driver`] the TUI
//! runs — lock, poll, fleet watcher, effects — and publishes a [`Snapshot`]
//! after every change. The GPUI half (the window, with the Board as a popup) draws
//! the latest snapshot and sends [`UiCmd`]s back; it never touches a file or
//! git on the UI thread.
//!
//! [`Driver`]: crate::tui::driver::Driver

pub mod backend;
mod board;
mod changes;
mod chat;
mod keys;
mod main_view;
mod sheets;
#[cfg(feature = "desktop-shots")]
mod shots;
pub mod theme;
mod ui;

use std::sync::Arc;

use anyhow::Context as _;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, KeyBinding, Window, WindowBounds, WindowOptions,
    actions, px, size,
};
use gpui_component::{Root, TitleBar};
use tokio::sync::{mpsc, watch};

use crate::cli::ExitCode;
use crate::paths::FleetPaths;
use crate::tui::app::TuiOptions;
use crate::tui::driver::ConsoleLock;
use backend::{Snapshot, UiCmd};

actions!(
    pilotfish,
    [
        Quit,
        ToggleBoard,
        ToggleSessions,
        ToggleWorkers,
        ToggleChanges,
        OpenPalette,
        OpenSearch,
        OpenHelp,
        OpenFleet,
        ToggleVerbose,
    ]
);

/// What the window reads: the latest snapshot, the way back to the backend,
/// and the look.
pub struct Shared {
    pub snap: Arc<Snapshot>,
    pub cmds: mpsc::UnboundedSender<UiCmd>,
    pub palette: theme::Palette,
}

impl Shared {
    pub fn send(&self, cmd: UiCmd) {
        let _ = self.cmds.send(cmd);
    }
}

/// Open the window and run until it closes. Blocks the main thread: AppKit
/// lives there.
///
/// # Errors
/// The fleet cannot be created, or another console holds its lock.
pub fn run(runtime: tokio::runtime::Runtime, options: TuiOptions) -> anyhow::Result<ExitCode> {
    let cwd = match options.cwd.clone() {
        Some(dir) => dir,
        None => std::env::current_dir().context("no working directory")?,
    };
    let fleet = FleetPaths::discover(&cwd);
    fleet
        .ensure()
        .context("creating the fleet state directory")?;
    let lock = ConsoleLock::acquire(&fleet)?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (snap_tx, snap_rx) = watch::channel(Arc::new(Snapshot::default()));
    // tokio futures created from GPUI callbacks find this runtime
    let _enter = runtime.enter();
    runtime.spawn(backend::serve(
        fleet.clone(),
        options,
        lock,
        cmd_rx,
        snap_tx,
    ));
    // ctrl-c in the launching terminal closes the window the same way ⌘Q does
    let interrupt = cmd_tx.clone();
    runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = interrupt.send(UiCmd::Quit);
        }
    });

    // the repo's folder name; the full path is the window's own title
    let repo = fleet.root().parent().unwrap_or(fleet.root()).to_path_buf();
    let title = repo
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let full_title = crate::tui::app::home_relative(&repo);
    gpui_platform::application()
        .with_assets(theme::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::install(cx);
            bind_keys(cx);
            let shared = cx.new(|_| Shared {
                snap: Arc::new(Snapshot::default()),
                cmds: cmd_tx.clone(),
                palette: theme::Palette::new(true),
            });
            pump(shared.clone(), snap_rx, cx);
            let quit = cmd_tx.clone();
            cx.on_action(move |_: &Quit, _| {
                let _ = quit.send(UiCmd::Quit);
            });
            // the main window closing is the console closing
            let main_id = std::rc::Rc::new(std::cell::Cell::new(None));
            let closing = cmd_tx.clone();
            let watched = main_id.clone();
            cx.on_window_closed(move |_, id| {
                if watched.get() == Some(id) {
                    let _ = closing.send(UiCmd::Quit);
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(980.), px(560.))),
                ..TitleBar::window_options()
            };
            let title = title.clone();
            let full_title = full_title.clone();
            let fleet_root = fleet.root().to_path_buf();
            let opened = cx.open_window(options, |window, cx| {
                window.set_window_title(&format!("pilotfish — {full_title}"));
                theme::apply(window, cx);
                let view = cx.new(|cx| {
                    main_view::MainView::new(shared.clone(), title, &fleet_root, window, cx)
                });
                cx.new(|cx| Root::new(view, window, cx))
            });
            match opened {
                Ok(handle) => {
                    main_id.set(Some(handle.window_id()));
                    #[cfg(feature = "desktop-shots")]
                    shots::start(handle.into(), cx);
                }
                Err(_) => {
                    let _ = cmd_tx.send(UiCmd::Quit);
                }
            }
            cx.activate(true);
        });
    Ok(ExitCode::Ok)
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-b", ToggleBoard, None),
        KeyBinding::new("cmd-1", ToggleSessions, None),
        KeyBinding::new("cmd-2", ToggleWorkers, None),
        KeyBinding::new("cmd-3", ToggleChanges, None),
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-f", OpenSearch, None),
        KeyBinding::new("cmd-/", OpenHelp, None),
        KeyBinding::new("cmd-shift-f", OpenFleet, None),
        KeyBinding::new("cmd-o", ToggleVerbose, None),
    ]);
}

/// Carry each published snapshot onto the UI thread; a closed one quits.
fn pump(shared: Entity<Shared>, mut rx: watch::Receiver<Arc<Snapshot>>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while rx.changed().await.is_ok() {
            let snap = rx.borrow_and_update().clone();
            let closed = snap.closed;
            shared.update(cx, |shared, cx| {
                shared.snap = snap;
                cx.notify();
            });
            if closed {
                break;
            }
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

/// The shared look, refreshed from the window it is drawn in.
pub fn palette(window: &Window) -> theme::Palette {
    theme::Palette::new(theme::is_dark(window))
}

/// Keep the look in step with macOS while a view lives.
pub fn follow_appearance<V: 'static>(window: &mut Window, cx: &mut Context<V>) {
    cx.observe_window_appearance(window, |_, window, cx| {
        theme::apply(window, cx);
        cx.notify();
    })
    .detach();
}
