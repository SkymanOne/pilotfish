//! The console's colors and glyphs: prompts in cyan, reasoning dimmed, tool
//! calls in blue, fleet events in yellow, errors red; notices and tool output
//! dim. Everything style-shaped flows through [`Palette`], which folds to a
//! colorless form under `NO_COLOR` (attributes like bold and italic stay, so
//! emphasis survives even a monochrome terminal).

use ratatui::style::{Color, Modifier, Style};

use crate::tui::transcript::BlockKind;

/// What the frame is painted with. One per draw call, resolved once so the
/// `NO_COLOR` check is not scattered over the view code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    color: bool,
}

impl Palette {
    /// The terminal's palette: colorless when `NO_COLOR` is set and non-empty
    /// (the NO_COLOR convention), colored otherwise.
    #[must_use]
    pub fn detect() -> Self {
        let color = std::env::var("NO_COLOR").map_or(true, |value| value.is_empty());
        Self { color }
    }

    /// A monochrome palette (tests, and callers outside a terminal).
    #[must_use]
    pub const fn plain() -> Self {
        Self { color: false }
    }

    /// A full-color palette, ignoring the environment (tests).
    #[must_use]
    pub const fn colored() -> Self {
        Self { color: true }
    }

    fn paint(&self, color: Color) -> Option<Color> {
        self.color.then_some(color)
    }

    fn with(&self, color: Color, modifier: Modifier) -> Style {
        let style = Style::default().add_modifier(modifier);
        self.paint(color).map_or(style, |c| style.fg(c))
    }

    /// The color for a transcript block kind — the colour language the old
    /// console established.
    #[must_use]
    pub fn block(&self, kind: BlockKind) -> Style {
        match kind {
            // the human's words, the loudest thing on the pane
            BlockKind::User => self.with(Color::Cyan, Modifier::BOLD),
            BlockKind::Fleet => self.plain_fg(Color::Yellow),
            BlockKind::Text => Style::default(),
            BlockKind::Thinking => self.with(Color::DarkGray, Modifier::ITALIC),
            BlockKind::Tool => self.plain_fg(Color::Blue),
            // output and notices are secondary: dim
            BlockKind::ToolResult | BlockKind::System => self.plain_fg(Color::DarkGray),
            BlockKind::Error => self.plain_fg(Color::Red),
        }
    }

    fn plain_fg(&self, color: Color) -> Style {
        self.paint(color).map_or_else(
            || Style::default().add_modifier(Modifier::DIM),
            |c| Style::default().fg(c),
        )
    }

    /// Dim secondary text (hints, details, counts of what was left out).
    #[must_use]
    pub fn dim(&self) -> Style {
        self.plain_fg(Color::DarkGray)
    }

    /// The console's accent: prompts, the orchestrator's name, borders that
    /// mean "you are typing here".
    #[must_use]
    pub fn accent(&self) -> Style {
        self.plain_fg(Color::Cyan)
    }

    /// Something wants the human: a question, an approval, a failure.
    #[must_use]
    pub fn attention(&self) -> Style {
        self.plain_fg(Color::Yellow)
    }

    /// A failure.
    #[must_use]
    pub fn error(&self) -> Style {
        self.plain_fg(Color::Red)
    }

    /// The row the selection is on: a quiet band, not a flash.
    #[must_use]
    pub fn selected(&self) -> Style {
        self.paint(Color::DarkGray).map_or_else(
            || Style::default().add_modifier(Modifier::REVERSED),
            |bg| Style::default().bg(bg),
        )
    }

    /// A search match under the caret sits on plain matches.
    #[must_use]
    pub fn current_match(&self) -> Style {
        self.paint(Color::Yellow).map_or_else(
            || Style::default().add_modifier(Modifier::REVERSED),
            |bg| Style::default().fg(Color::Black).bg(bg),
        )
    }

    /// Every other search match.
    #[must_use]
    pub fn other_match(&self) -> Style {
        self.paint(Color::DarkGray).map_or_else(
            || Style::default().add_modifier(Modifier::UNDERLINED),
            |bg| Style::default().bg(bg),
        )
    }

    /// Markdown styling, by what the span is.
    #[must_use]
    pub fn heading(&self) -> Style {
        self.with(Color::White, Modifier::BOLD)
    }

    /// Inline and fenced code.
    #[must_use]
    pub fn code(&self) -> Style {
        self.plain_fg(Color::Green)
    }

    /// A link: the text only, underlined.
    #[must_use]
    pub fn link(&self) -> Style {
        self.with(Color::Cyan, Modifier::UNDERLINED)
    }

    /// A quoted block.
    #[must_use]
    pub fn quote(&self) -> Style {
        self.plain_fg(Color::DarkGray)
    }

    /// A suggestion's kind color in the completion popup.
    #[must_use]
    pub fn suggestion(&self, kind: crate::tui::completions::SuggestionKind) -> Style {
        use crate::tui::completions::SuggestionKind as K;
        match kind {
            K::Command => self.accent(),
            K::Agent => self.plain_fg(Color::Magenta),
            K::Worker => self.attention(),
            K::File => self.plain_fg(Color::Green),
        }
    }
}

/// Which overlay is up; borders carry the mood (approval is yellow, removal
/// is red, the palette is the console's accent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayRole {
    Help,
    /// The fleet panel.
    Fleet,
    Confirm,
    Permission,
    Palette,
    Search,
}

impl Palette {
    /// The border style for an overlay.
    #[must_use]
    pub fn border(&self, role: OverlayRole) -> Style {
        match role {
            OverlayRole::Help => self.dim(),
            OverlayRole::Fleet => self.heading(),
            OverlayRole::Confirm => self.error(),
            OverlayRole::Permission => self.attention(),
            OverlayRole::Palette | OverlayRole::Search => self.accent(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_kinds_carry_the_old_console_color_language() {
        let pal = Palette::colored();
        assert_eq!(pal.block(BlockKind::User).fg, Some(Color::Cyan));
        assert!(
            pal.block(BlockKind::User)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(pal.block(BlockKind::Fleet).fg, Some(Color::Yellow));
        assert_eq!(pal.block(BlockKind::Tool).fg, Some(Color::Blue));
        assert_eq!(pal.block(BlockKind::Error).fg, Some(Color::Red));
        // notices and tool output are dim: DarkGray in color, DIM without it
        assert_eq!(pal.block(BlockKind::System).fg, Some(Color::DarkGray));
        assert_eq!(pal.block(BlockKind::ToolResult).fg, Some(Color::DarkGray));
        assert_eq!(pal.block(BlockKind::Text), Style::default());
    }

    #[test]
    fn no_color_folds_to_attributes_and_dim() {
        let pal = Palette::plain();
        assert_eq!(pal.block(BlockKind::User).fg, None);
        // bold survives: emphasis is not color
        assert!(
            pal.block(BlockKind::User)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        // yellow-on-nothing becomes DIM so "attention" still reads
        assert!(
            pal.plain_fg(Color::Yellow)
                .add_modifier
                .contains(Modifier::DIM)
        );
        // selected folds to reversed instead of a background
        assert!(pal.selected().add_modifier.contains(Modifier::REVERSED));
    }
}
