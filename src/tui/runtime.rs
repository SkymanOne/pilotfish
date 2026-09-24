//! Crossterm runtime and terminal setup: raw mode, the alternate screen, the
//! event stream, restore on panic and on every exit path — a console that
//! dies in raw mode leaves the user's shell unusable, so teardown outranks
//! every feature here. The loop itself is a [`Driver`] fed by terminal
//! events.

use std::io;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::time::MissedTickBehavior;

use crate::cli::ExitCode;
use crate::paths::FleetPaths;
use crate::tui::app::{Effect, TuiOptions};
use crate::tui::driver::{ConsoleLock, Driver, FEED_MS, HEARTBEAT_MS, TAIL_MS};
use crate::tui::theme::Palette;
use crate::tui::view::{self, Feeds};
use crate::util::now_ms;

/// The clock: ages, elapsed counters and spinners move on this tick even
/// when no file changed.
const TICK_MS: u64 = 250;

// ---------------------------------------------------------------------------
// Terminal install / teardown

/// Is this an interactive terminal? Both ends must be a TTY, the way the
/// TypeScript console asked (`process.stdin.isTTY && process.stdout.isTTY`).
/// A crossterm size query lies here: its `tput` fallback answers even on a
/// worker session with no controlling terminal, and raw mode would then die
/// later on the missing `/dev/tty` with a bare io error.
#[must_use]
pub fn is_interactive() -> bool {
    use crossterm::tty::IsTty as _;
    io::stdin().is_tty() && io::stdout().is_tty()
}

/// Install the terminal: raw mode, alternate screen, mouse capture, the
/// kitty keyboard protocol, backend.
///
/// # Errors
/// Raw mode or the screen switch failing — there is nothing to restore yet.
pub fn enter() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Without bracketed paste a pasted brief arrives as keystrokes, and its
    // first newline is an enter that sends the rest half-typed. With it the
    // terminal hands over the whole paste as one event.
    let _ = execute!(stdout, EnableBracketedPaste);
    // Without this a terminal cannot tell shift-enter from enter — both are
    // a bare CR — so the composer could never bind the newline everyone
    // reaches for. `DISAMBIGUATE_ESCAPE_CODES` makes the terminal send
    // `CSI 13;2u` instead. Pushed blind and best effort: a terminal that
    // does not speak the protocol ignores both the push and the pop, while
    // `supports_keyboard_enhancement` costs a 2 s round trip on exactly
    // those terminals, and a slow console start is worse than a key that
    // falls back to alt-enter.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    Terminal::new(CrosstermBackend::new(stdout))
}

/// Undo [`enter`]: release the mouse, leave the alternate screen, drop raw
/// mode, flush. Best effort and idempotent — called from the panic hook and
/// every exit path, and it must never mask the error that brought us here.
pub fn restore() {
    use std::io::Write as _;
    // the enhancement flags pop first: leaving them on would outlive the
    // console and confuse whatever the shell runs next
    let _ = execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = io::stdout().flush();
}

/// Take the mouse for the console, or hand it back to the terminal so its
/// own click-and-drag selection works. Best effort: a terminal that ignores
/// the sequence simply keeps whatever it was doing, and the console's
/// keyboard scrolling is unaffected either way.
pub fn set_mouse_capture(on: bool) {
    let _ = if on {
        execute!(io::stdout(), EnableMouseCapture)
    } else {
        execute!(io::stdout(), DisableMouseCapture)
    };
}

/// Restore the terminal when the process panics anywhere.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Raw mode means ctrl-c never becomes SIGINT: the runtime reads it as the
/// console's own quit, the way the old ink app's `exitOnCtrlC` did.
fn is_interrupt(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
}

// ---------------------------------------------------------------------------
// The event loop

/// Run the console until the user quits: one draw per pass, key events
/// through the state machine and its effects, `.pilotfish` polled into the feeds
/// on a timer, the lock heartbeating. Workers keep running afterwards.
///
/// # Errors
/// Terminal draw failures only; console errors surface as notices.
pub async fn run_console(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    fleet: FleetPaths,
    lock: &ConsoleLock,
    options: TuiOptions,
) -> anyhow::Result<ExitCode> {
    let mut driver = Driver::open(fleet, options).await;

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    // the transcript tail is two file reads at an offset, so it can run often;
    // reloading every run.json and the diff stats cannot
    let mut tail = tokio::time::interval(Duration::from_millis(TAIL_MS));
    let mut feed = tokio::time::interval(Duration::from_millis(FEED_MS));
    let mut heartbeat = tokio::time::interval(Duration::from_millis(HEARTBEAT_MS));
    for timer in [&mut tick, &mut tail, &mut feed, &mut heartbeat] {
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    }
    // Reading `$NO_COLOR` and friends once per frame told us the same thing
    // every time; the environment does not change under a running console.
    let palette = Palette::detect();
    // Skipped only for a tail that found nothing: every other wake-up changes
    // something on screen, and a missed redraw is worse than a wasted one.
    let mut draw = true;

    loop {
        // the flash's expiry is a clock matter, not an event matter
        driver.console.tick(now_ms());
        if draw {
            let (console, orch, runs) = driver.parts();
            let feeds = Feeds { orch, runs };
            terminal.draw(|frame| view::draw(frame, console, &feeds, &palette))?;
        }
        draw = true;

        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(Event::Key(key))) => {
                    if is_interrupt(&key) {
                        break;
                    }
                    let effects = driver.console.handle_key(key);
                    let mouse = effects.iter().find_map(|effect| match effect {
                        Effect::SetMouseCapture(on) => Some(*on),
                        _ => None,
                    });
                    if let Some(on) = mouse {
                        set_mouse_capture(on);
                    }
                    if driver.apply(effects).await {
                        break;
                    }
                }
                Some(Ok(Event::Mouse(mouse))) => {
                    // the wheel scrolls half a viewport, in both modes and
                    // inside the brief popup; other buttons stay unbound
                    let action = crate::tui::keys::map_mouse(mouse);
                    if action != crate::tui::keys::KeyAction::Ignored {
                        let effects = driver.console.handle_action(action);
                        driver.apply(effects).await;
                    }
                }
                Some(Ok(Event::Paste(text))) => {
                    driver.console.paste(&text);
                }
                // resize redraws on the next pass; focus is unused
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            // the clock: ages, activity lines and the elapsed counters move
            _ = tick.tick() => {}
            _ = tail.tick() => {
                draw = driver.tail();
            }
            _ = feed.tick() => driver.feed().await,
            _ = heartbeat.tick() => lock.refresh(),
        }
    }
    driver.close();
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_key() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_interrupt(&ctrl_c));
        assert!(!is_interrupt(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        // release events never count
        let released = KeyEvent {
            kind: KeyEventKind::Release,
            ..KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        };
        assert!(!is_interrupt(&released));
    }
}
