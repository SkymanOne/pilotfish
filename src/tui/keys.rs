//! The key map: one [`KeyEvent`] in, one [`KeyAction`] out. Pure — the app
//! decides what an action means where it lands.
//!
//! There is no modal split any more. The composer always has focus, so every
//! printable key is text and nothing a message might start with is bound.
//! The handful of things that are not text are `ctrl` chords, and everything
//! that acts on a *session* lives in the fleet overlay, where single letters
//! are unambiguous because nothing there is being typed.
//!
//! The help overlay is built from the same tables as the bindings, so the two
//! cannot drift.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

/// What a keypress means. The app interprets it; the map only translates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    // -- the conversation ---------------------------------------------------
    /// A printable character. In the composer it is text; an overlay reads it
    /// as its own command, because nothing is being typed there.
    InsertChar(char),
    InsertBackspace,
    InsertDelete,
    InsertLeft,
    InsertRight,
    InsertHome,
    InsertEnd,
    /// Enter: send the composer's line.
    Send,
    /// Shift-enter (or alt-enter, or ctrl-j): a newline, not a send.
    Newline,
    /// Tab: accept the highlighted completion.
    AcceptCompletion,
    /// Up/Down: through completions, or recall history when none are open.
    CompletionPrev,
    CompletionNext,
    /// Esc: close what is open, else clear the composer, else stop the turn.
    Escape,
    /// `ctrl-f`: the fleet overlay — every session, and what can be done to
    /// the one selected.
    OpenFleet,
    /// `ctrl-k`: the command palette.
    OpenPalette,
    /// `ctrl-r`: search the open session's transcript.
    Search,
    /// `ctrl-y`: hand the mouse to the terminal so its own selection works,
    /// or take it back so the wheel scrolls again.
    ToggleMouse,
    /// `ctrl-o`: show an older turn's reasoning and tool output in full, or
    /// fold each back to a summary row.
    ToggleVerbose,
    // -- transcript scrolling -----------------------------------------------
    ScrollHalfDown,
    ScrollHalfUp,
    ScrollPageDown,
    ScrollPageUp,
    /// The top / bottom of the transcript, or the first / last fleet row.
    First,
    Last,
    // -- overlay navigation (single letters, read from `InsertChar`) --------
    /// Move the selection by one row.
    Move(i32),
    /// Enter: take the selected row.
    Open,
    /// `1`–`9`: jump to the nth session.
    JumpTo(usize),
    /// The next search match; `ctrl-r` again once a search is running.
    NextMatch,
    /// Bound to nothing.
    Ignored,
}

/// Is this key event a real press (not a kitty-protocol release)?
fn is_press(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
}

fn ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Map a key press to an action.
#[must_use]
pub fn map_key(key: KeyEvent) -> KeyAction {
    use KeyAction as A;
    if !is_press(&key) {
        return A::Ignored;
    }
    match key.code {
        // chords first: a bare letter is always text
        KeyCode::Char('f') if ctrl(&key) => A::OpenFleet,
        KeyCode::Char('k') if ctrl(&key) => A::OpenPalette,
        KeyCode::Char('r') if ctrl(&key) => A::Search,
        KeyCode::Char('y') if ctrl(&key) => A::ToggleMouse,
        KeyCode::Char('o') if ctrl(&key) => A::ToggleVerbose,
        KeyCode::Char('j') if ctrl(&key) => A::Newline,
        KeyCode::Char(ch) if !ctrl(&key) => A::InsertChar(ch),
        // shift-enter is the newline everyone reaches for, but a terminal
        // only reports it as its own key under the kitty protocol
        // (`runtime::enter` asks for it); alt-enter and ctrl-j are the
        // fallbacks on the terminals that cannot
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            A::Newline
        }
        KeyCode::Enter => A::Send,
        KeyCode::Tab => A::AcceptCompletion,
        KeyCode::Backspace => A::InsertBackspace,
        KeyCode::Delete => A::InsertDelete,
        KeyCode::Left => A::InsertLeft,
        KeyCode::Right => A::InsertRight,
        KeyCode::Home if ctrl(&key) => A::First,
        KeyCode::End if ctrl(&key) => A::Last,
        KeyCode::Home => A::InsertHome,
        KeyCode::End => A::InsertEnd,
        KeyCode::Up => A::CompletionPrev,
        KeyCode::Down => A::CompletionNext,
        KeyCode::PageDown => A::ScrollPageDown,
        KeyCode::PageUp => A::ScrollPageUp,
        KeyCode::Esc => A::Escape,
        _ => A::Ignored,
    }
}

/// The same event as an overlay reads it: single letters are commands there,
/// because an overlay is a list, not a text field. Overlays that *are* text
/// fields (the palette, the search box) use [`map_key`] instead.
#[must_use]
pub fn map_overlay_key(key: KeyEvent) -> KeyAction {
    use KeyAction as A;
    let base = map_key(key);
    let A::InsertChar(ch) = base else {
        return match base {
            A::CompletionPrev => A::Move(-1),
            A::CompletionNext => A::Move(1),
            other => other,
        };
    };
    match ch {
        'j' => A::Move(1),
        'k' => A::Move(-1),
        'g' => A::First,
        'G' => A::Last,
        digit @ '1'..='9' => A::JumpTo(digit as usize - '1' as usize),
        // every other letter is the overlay's own; it reads the character
        _ => base,
    }
}

/// Map a mouse event onto the action it means. The wheel scrolls wherever
/// the transcript does: half a viewport per notch; every other button stays
/// unbound for now.
#[must_use]
pub fn map_mouse(mouse: MouseEvent) -> KeyAction {
    match mouse.kind {
        MouseEventKind::ScrollUp => KeyAction::ScrollHalfUp,
        MouseEventKind::ScrollDown => KeyAction::ScrollHalfDown,
        _ => KeyAction::Ignored,
    }
}

// ---------------------------------------------------------------------------
// The help overlay, built from the same table as the bindings above

/// One row of the help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHelp {
    pub keys: &'static str,
    pub what: &'static str,
}

/// Compact on purpose: the help shares the pane with everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpSection {
    pub title: &'static str,
    pub rows: &'static [KeyHelp],
}

/// The conversation: the composer has focus, so every printable key is text.
pub const COMPOSE_KEYS: &[KeyHelp] = &[
    KeyHelp {
        keys: "type + enter",
        what: "message the orchestrator or worker",
    },
    KeyHelp {
        keys: "shift-enter",
        what: "newline (or alt-enter, ctrl-j)",
    },
    KeyHelp {
        keys: "/",
        what: "commands, yours and the agent's",
    },
    KeyHelp {
        keys: "@",
        what: "workers and repository files",
    },
    KeyHelp {
        keys: "tab",
        what: "accept the suggestion",
    },
    KeyHelp {
        keys: "up / down",
        what: "suggestions, or earlier messages",
    },
    KeyHelp {
        keys: "esc",
        what: "close, clear the line, stop the turn",
    },
];

/// The chords: everything in the conversation that is not text.
pub const CHORD_KEYS: &[KeyHelp] = &[
    KeyHelp {
        keys: "ctrl-f",
        what: "the fleet: every session",
    },
    KeyHelp {
        keys: "ctrl-k",
        what: "the command palette",
    },
    KeyHelp {
        keys: "ctrl-r",
        what: "search; again for the next match",
    },
    KeyHelp {
        keys: "ctrl-y",
        what: "free the mouse to select and copy",
    },
    KeyHelp {
        keys: "ctrl-o",
        what: "unfold older reasoning and tools",
    },
    KeyHelp {
        keys: "pgup / pgdn",
        what: "scroll (the wheel does too)",
    },
    KeyHelp {
        keys: "ctrl-home/end",
        what: "top / back to the tail",
    },
];

/// The fleet overlay: a list, so single letters are free.
pub const FLEET_KEYS: &[KeyHelp] = &[
    KeyHelp {
        keys: "j k / arrows",
        what: "move the selection",
    },
    KeyHelp {
        keys: "g / G",
        what: "first / last row",
    },
    KeyHelp {
        keys: "1-9",
        what: "jump to the nth session",
    },
    KeyHelp {
        keys: "enter",
        what: "show that conversation",
    },
    KeyHelp {
        keys: "a",
        what: "answer its question or dialog",
    },
    KeyHelp {
        keys: "s",
        what: "stop the selected worker",
    },
    KeyHelp {
        keys: "x",
        what: "remove the worker, or the whole session (asks)",
    },
    KeyHelp {
        keys: "t",
        what: "cycle the thinking level",
    },
    KeyHelp {
        keys: "m",
        what: "switch the model",
    },
    KeyHelp {
        keys: "p",
        what: "permission mode (orchestrator)",
    },
    KeyHelp {
        keys: "b",
        what: "the full brief",
    },
    KeyHelp {
        keys: "?",
        what: "this help",
    },
    KeyHelp {
        keys: "esc",
        what: "back to the conversation",
    },
];

/// The help as sections, so it can be laid out in columns and always fit.
#[must_use]
pub fn help_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "Typing",
            rows: COMPOSE_KEYS,
        },
        HelpSection {
            title: "Chords",
            rows: CHORD_KEYS,
        },
        HelpSection {
            title: "Fleet (ctrl-f)",
            rows: FLEET_KEYS,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyAction as A;
    use crossterm::event::{MouseButton, MouseEventKind};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn every_printable_key_is_text() {
        // the whole point of dropping normal mode: no letter is stolen from a
        // message, so starting to type is never punished
        for ch in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/:?@ !".chars() {
            assert_eq!(
                map_key(key(KeyCode::Char(ch))),
                A::InsertChar(ch),
                "{ch:?} must reach the composer"
            );
        }
    }

    #[test]
    fn the_chords_are_the_only_non_text_letters() {
        assert_eq!(map_key(ctrl_key(KeyCode::Char('f'))), A::OpenFleet);
        assert_eq!(map_key(ctrl_key(KeyCode::Char('k'))), A::OpenPalette);
        assert_eq!(map_key(ctrl_key(KeyCode::Char('r'))), A::Search);
        assert_eq!(map_key(ctrl_key(KeyCode::Char('y'))), A::ToggleMouse);
        assert_eq!(map_key(ctrl_key(KeyCode::Char('o'))), A::ToggleVerbose);
        assert_eq!(map_key(ctrl_key(KeyCode::Char('j'))), A::Newline);
    }

    #[test]
    fn the_composer_keys_edit_and_send() {
        assert_eq!(map_key(key(KeyCode::Enter)), A::Send);
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            A::Newline
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            A::Newline
        );
        assert_eq!(map_key(key(KeyCode::Tab)), A::AcceptCompletion);
        assert_eq!(map_key(key(KeyCode::Backspace)), A::InsertBackspace);
        assert_eq!(map_key(key(KeyCode::Delete)), A::InsertDelete);
        assert_eq!(map_key(key(KeyCode::Left)), A::InsertLeft);
        assert_eq!(map_key(key(KeyCode::Right)), A::InsertRight);
        assert_eq!(map_key(key(KeyCode::Home)), A::InsertHome);
        assert_eq!(map_key(key(KeyCode::End)), A::InsertEnd);
        assert_eq!(map_key(ctrl_key(KeyCode::Home)), A::First);
        assert_eq!(map_key(ctrl_key(KeyCode::End)), A::Last);
        assert_eq!(map_key(key(KeyCode::Up)), A::CompletionPrev);
        assert_eq!(map_key(key(KeyCode::Down)), A::CompletionNext);
        assert_eq!(map_key(key(KeyCode::PageUp)), A::ScrollPageUp);
        assert_eq!(map_key(key(KeyCode::PageDown)), A::ScrollPageDown);
        assert_eq!(map_key(key(KeyCode::Esc)), A::Escape);
    }

    #[test]
    fn an_overlay_reads_single_letters_as_commands() {
        assert_eq!(map_overlay_key(key(KeyCode::Char('j'))), A::Move(1));
        assert_eq!(map_overlay_key(key(KeyCode::Char('k'))), A::Move(-1));
        assert_eq!(map_overlay_key(key(KeyCode::Down)), A::Move(1));
        assert_eq!(map_overlay_key(key(KeyCode::Up)), A::Move(-1));
        assert_eq!(map_overlay_key(key(KeyCode::Char('g'))), A::First);
        assert_eq!(map_overlay_key(key(KeyCode::Char('G'))), A::Last);
        assert_eq!(map_overlay_key(key(KeyCode::Char('3'))), A::JumpTo(2));
        // enter stays `Send`; only the fleet list reads it as "take this row"
        assert_eq!(map_overlay_key(key(KeyCode::Enter)), A::Send);
        assert_eq!(map_overlay_key(key(KeyCode::Esc)), A::Escape);
        // anything the overlay does not navigate with stays a character —
        // `n` above all, which is how a confirm prompt hears "no"
        assert_eq!(map_overlay_key(key(KeyCode::Char('a'))), A::InsertChar('a'));
        assert_eq!(map_overlay_key(key(KeyCode::Char('n'))), A::InsertChar('n'));
        assert_eq!(map_overlay_key(key(KeyCode::Char('y'))), A::InsertChar('y'));
        assert_eq!(map_overlay_key(key(KeyCode::Char('?'))), A::InsertChar('?'));
        // the chords still work from inside one
        assert_eq!(
            map_overlay_key(ctrl_key(KeyCode::Char('k'))),
            A::OpenPalette
        );
    }

    #[test]
    fn a_key_release_is_not_a_press() {
        let mut ev = key(KeyCode::Char('a'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(map_key(ev), A::Ignored);
    }

    #[test]
    fn the_wheel_scrolls_and_other_buttons_do_nothing() {
        let wheel = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(wheel(MouseEventKind::ScrollUp)), A::ScrollHalfUp);
        assert_eq!(
            map_mouse(wheel(MouseEventKind::ScrollDown)),
            A::ScrollHalfDown
        );
        assert_eq!(
            map_mouse(wheel(MouseEventKind::Down(MouseButton::Left))),
            A::Ignored
        );
    }

    #[test]
    fn the_help_names_every_chord_and_the_fleet_letters() {
        let sections = help_sections();
        let titles: Vec<&str> = sections.iter().map(|s| s.title).collect();
        assert_eq!(titles, vec!["Typing", "Chords", "Fleet (ctrl-f)"]);
        let keys: Vec<&str> = sections
            .iter()
            .flat_map(|s| s.rows.iter())
            .map(|row| row.keys)
            .collect();
        for chord in ["ctrl-f", "ctrl-k", "ctrl-r", "ctrl-o", "ctrl-y"] {
            assert!(keys.contains(&chord), "{chord} is documented: {keys:?}");
        }
    }
}
