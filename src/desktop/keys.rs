//! A GPUI keystroke, spelled the way crossterm spells it. The console's
//! overlays already know what every key means (`y`/`a`/`n`, arrows, enter,
//! esc, typing into the palette); the desktop hands them the same event
//! instead of learning the table twice.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gpui::Keystroke;

#[must_use]
pub fn to_crossterm(keystroke: &Keystroke) -> Option<KeyEvent> {
    let m = &keystroke.modifiers;
    // ⌘-chords are the window's own (palette, board, quit…), never text
    if m.platform {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    if m.control {
        modifiers |= KeyModifiers::CONTROL;
    }
    if m.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if m.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    let code = match keystroke.key.as_str() {
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" if m.shift => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        key => {
            // the typed character (shift already applied), else the key
            let text = keystroke.key_char.as_deref().unwrap_or(key);
            let mut chars = text.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return None;
            };
            // crossterm reports a shifted letter as the letter itself
            modifiers.remove(KeyModifiers::SHIFT);
            KeyCode::Char(ch)
        }
    };
    Some(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystroke_table() {
        let key = |spec: &str| to_crossterm(&Keystroke::parse(spec).unwrap());
        let plain = |code| Some(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(key("enter"), plain(KeyCode::Enter));
        assert_eq!(key("escape"), plain(KeyCode::Esc));
        assert_eq!(key("down"), plain(KeyCode::Down));
        assert_eq!(key("y"), plain(KeyCode::Char('y')));
        assert_eq!(key("space"), plain(KeyCode::Char(' ')));
        assert_eq!(
            key("shift-enter"),
            Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        );
        assert_eq!(
            key("ctrl-c"),
            Some(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        );
        // ⌘-chords belong to the window
        assert_eq!(key("cmd-k"), None);
    }
}
