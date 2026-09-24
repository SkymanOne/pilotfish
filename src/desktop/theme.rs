//! The desktop's two looks, Night bridge (dark) and Chart table (light), as
//! one token set. Structure is deep water or chart paper; colour is kept for
//! state, in the lights ships show: starboard green running, Q-flag yellow
//! waiting on you, port red failed, a masthead ring when done.

use std::borrow::Cow;

use gpui::{App, Hsla, Window, WindowAppearance, rgb, rgba};
use gpui_component::{Theme, ThemeMode, ThemeRegistry};

use super::backend::Lane;

pub const SANS: &str = "IBM Plex Sans Condensed";
pub const MONO: &str = "IBM Plex Mono";

/// The resolved tokens for one appearance.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Hsla,
    pub panel: Hsla,
    pub raised: Hsla,
    pub line: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub accent_ink: Hsla,
    pub run: Hsla,
    pub wait: Hsla,
    pub fail: Hsla,
    pub done: Hsla,
    pub add_bg: Hsla,
    pub del_bg: Hsla,
    pub select: Hsla,
}

const NIGHT: [u32; 12] = [
    0x0F2433, 0x15313F, 0x1B3B4B, 0x27485A, 0xDDE7EA, 0x7F9AA8, 0xC49A4C, 0x0F2433, 0x3BA776,
    0xE2B634, 0xD1473D, 0xD8E3E7,
];
const CHART: [u32; 12] = [
    0xF6F7F3, 0xDCEAF0, 0xFFFFFF, 0x9DB7C3, 0x1C2A32, 0x5C7280, 0xA8326F, 0xFFFFFF, 0x1F7A52,
    0xB8860B, 0xB3362C, 0x1C2A32,
];

impl Palette {
    #[must_use]
    pub fn new(dark: bool) -> Self {
        let c = if dark { NIGHT } else { CHART };
        let hsla = |hex: u32| Hsla::from(rgb(hex));
        let tint = |hex: u32, alpha: u8| Hsla::from(rgba((hex << 8) | u32::from(alpha)));
        Self {
            dark,
            bg: hsla(c[0]),
            panel: hsla(c[1]),
            raised: hsla(c[2]),
            line: hsla(c[3]),
            text: hsla(c[4]),
            muted: hsla(c[5]),
            accent: hsla(c[6]),
            accent_ink: hsla(c[7]),
            run: hsla(c[8]),
            wait: hsla(c[9]),
            fail: hsla(c[10]),
            done: hsla(c[11]),
            add_bg: tint(c[8], if dark { 0x26 } else { 0x1F }),
            del_bg: tint(c[10], if dark { 0x26 } else { 0x1F }),
            select: tint(c[6], 0x2E),
        }
    }

    #[must_use]
    pub const fn lane(&self, lane: Lane) -> Hsla {
        match lane {
            Lane::Started => self.run,
            Lane::Waiting => self.wait,
            Lane::Failed => self.fail,
            Lane::Finished => self.done,
        }
    }
}

/// `PILOTFISH_APPEARANCE=dark|light` pins the look (screenshots, reviews);
/// otherwise it follows the window, which follows macOS.
#[must_use]
pub fn is_dark(window: &Window) -> bool {
    match std::env::var("PILOTFISH_APPEARANCE").as_deref() {
        Ok("dark") => true,
        Ok("light") => false,
        _ => matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    }
}

/// Bundle the fonts, register both looks with gpui-component, and apply the
/// one the window wants.
pub fn install(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/IBMPlexSansCondensed-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/IBMPlexSansCondensed-Medium.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/IBMPlexSansCondensed-SemiBold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/IBMPlexSansCondensed-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/IBMPlexMono-Italic.ttf")),
    ];
    let _ = cx.text_system().add_fonts(fonts);
    let registry = ThemeRegistry::global_mut(cx);
    if registry.load_themes_from_str(&themes_json()).is_ok() {
        let themes = registry.themes().clone();
        let theme = Theme::global_mut(cx);
        if let Some(dark) = themes.get("Night bridge") {
            theme.dark_theme = dark.clone();
        }
        if let Some(light) = themes.get("Chart table") {
            theme.light_theme = light.clone();
        }
    }
}

/// Re-apply the look for the window's appearance.
pub fn apply(window: &mut Window, cx: &mut App) {
    let mode = if is_dark(window) {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, Some(window), cx);
}

fn hex(value: u32) -> String {
    format!("#{value:06X}")
}

fn hex_alpha(value: u32, alpha: u8) -> String {
    format!("#{value:06X}{alpha:02X}")
}

/// gpui-component's own widgets (inputs, scrollbars, popovers, markdown)
/// dressed in the same tokens.
fn themes_json() -> String {
    let theme = |name: &str, mode: &str, c: [u32; 12]| {
        let (bg, panel, raised, line, text, muted, accent, ink, run, wait, fail) = (
            c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9], c[10],
        );
        let dark = mode == "dark";
        let (keyword, function, string, number, comment, kind) = if dark {
            (accent, 0x8CC4E8, run, wait, muted, 0x7FD1C7)
        } else {
            (accent, 0x1F5F8B, run, 0x8A5A00, muted, 0x16786E)
        };
        let colors: serde_json::Map<String, serde_json::Value> = [
            ("background", hex(bg)),
            ("foreground", hex(text)),
            ("border", hex(line)),
            ("window.border", hex(line)),
            ("caret", hex(accent)),
            ("ring", hex(accent)),
            ("selection.background", hex_alpha(accent, 0x55)),
            ("muted.background", hex(panel)),
            ("muted.foreground", hex(muted)),
            ("accent.background", hex(raised)),
            ("accent.foreground", hex(text)),
            ("primary.background", hex(accent)),
            ("primary.foreground", hex(ink)),
            ("primary.hover.background", hex_alpha(accent, 0xDD)),
            ("primary.active.background", hex(accent)),
            ("secondary.background", hex(raised)),
            ("secondary.foreground", hex(text)),
            ("secondary.hover.background", hex(line)),
            ("secondary.active.background", hex(line)),
            ("popover.background", hex(raised)),
            ("popover.foreground", hex(text)),
            ("input.border", hex(line)),
            ("list.background", hex(panel)),
            ("list.hover.background", hex(raised)),
            ("list.active.background", hex_alpha(accent, 0x2E)),
            ("list.active.border", hex(accent)),
            ("list.even.background", hex(panel)),
            ("list.head.background", hex(panel)),
            ("link.foreground", hex(accent)),
            ("link.hover.foreground", hex(accent)),
            ("link.active.foreground", hex(accent)),
            ("scrollbar.background", hex_alpha(bg, 0x00)),
            ("scrollbar.thumb.background", hex_alpha(muted, 0x88)),
            ("scrollbar.thumb.hover.background", hex(muted)),
            ("sidebar.background", hex(panel)),
            ("sidebar.foreground", hex(text)),
            ("sidebar.border", hex(line)),
            ("title_bar.background", hex(panel)),
            ("title_bar.border", hex(line)),
            ("tab.background", hex(bg)),
            ("tab.foreground", hex(muted)),
            ("tab.active.background", hex(bg)),
            ("tab.active.foreground", hex(text)),
            ("tab_bar.background", hex(bg)),
            ("danger.background", hex(fail)),
            ("danger.foreground", hex(0xFFFFFF)),
            ("success.background", hex(run)),
            ("success.foreground", hex(0xFFFFFF)),
            ("warning.background", hex(wait)),
            ("warning.foreground", hex(bg)),
            ("info.background", hex(raised)),
            ("info.foreground", hex(text)),
            (
                "overlay",
                hex_alpha(0x000000, if dark { 0x66 } else { 0x33 }),
            ),
            ("skeleton.background", hex(raised)),
            ("group_box.background", hex(panel)),
            ("group_box.foreground", hex(text)),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), serde_json::Value::String(value)))
        .collect();
        let syntax: serde_json::Map<String, serde_json::Value> = [
            ("keyword", serde_json::json!({ "color": hex(keyword) })),
            ("function", serde_json::json!({ "color": hex(function) })),
            (
                "function.method",
                serde_json::json!({ "color": hex(function) }),
            ),
            ("string", serde_json::json!({ "color": hex(string) })),
            ("string.escape", serde_json::json!({ "color": hex(string) })),
            ("number", serde_json::json!({ "color": hex(number) })),
            ("boolean", serde_json::json!({ "color": hex(number) })),
            ("constant", serde_json::json!({ "color": hex(number) })),
            (
                "comment",
                serde_json::json!({ "color": hex(comment), "font_style": "italic" }),
            ),
            (
                "comment.doc",
                serde_json::json!({ "color": hex(comment), "font_style": "italic" }),
            ),
            ("type", serde_json::json!({ "color": hex(kind) })),
            ("constructor", serde_json::json!({ "color": hex(kind) })),
            ("attribute", serde_json::json!({ "color": hex(kind) })),
            ("tag", serde_json::json!({ "color": hex(keyword) })),
            (
                "title",
                serde_json::json!({ "color": hex(text), "font_weight": 600 }),
            ),
            ("emphasis", serde_json::json!({ "font_style": "italic" })),
            ("emphasis.strong", serde_json::json!({ "font_weight": 600 })),
            ("link_text", serde_json::json!({ "color": hex(accent) })),
            ("link_uri", serde_json::json!({ "color": hex(accent) })),
            ("text.literal", serde_json::json!({ "color": hex(string) })),
            ("punctuation", serde_json::json!({ "color": hex(muted) })),
            ("operator", serde_json::json!({ "color": hex(muted) })),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
        serde_json::json!({
            "name": name,
            "mode": mode,
            "font.family": SANS,
            "font.size": 14,
            "mono_font.family": MONO,
            "mono_font.size": 12.5,
            "radius": 6,
            "radius.lg": 10,
            "shadow": true,
            "colors": colors,
            "highlight": {
                "editor.background": hex(bg),
                "editor.foreground": hex(text),
                "syntax": syntax,
            }
        })
    };
    serde_json::json!({
        "name": "Pilotfish",
        "author": "pilotfish",
        "themes": [
            theme("Night bridge", "dark", NIGHT),
            theme("Chart table", "light", CHART),
        ]
    })
    .to_string()
}

/// A diff line's background: a lane colour washed out, or nothing.
#[must_use]
pub fn tint(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
}
