//! The look: Chart & Buoy. White sheets float on a chart-paper canvas (deep
//! water at night), names are set in a soft serif, ink means running, and
//! buoy orange — the lifebuoy colour — appears only where you are needed.

use std::borrow::Cow;

use gpui::{App, AssetSource, Hsla, SharedString, Window, WindowAppearance, rgb, rgba};
use gpui_component::{Theme, ThemeMode, ThemeRegistry};

use super::backend::Lane;

/// Everything you read and click.
pub const SANS: &str = "Geist";
/// Code, diffs, tool lines.
pub const MONO: &str = "Geist Mono";
/// Titles: the session, the Board, empty states (a Fraunces cut for size).
pub const DISPLAY: &str = "Pilotfish Display";
/// Names at reading size: sessions, workers, cards (Fraunces, text optical size).
pub const SERIF: &str = "Pilotfish Serif";

/// The resolved tokens for one appearance.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// The window's ground.
    pub canvas: Hsla,
    /// The floating sheets: workers, chat, changes, dialogs.
    pub sheet: Hsla,
    /// Text, running work, primary buttons.
    pub ink: Hsla,
    pub muted: Hsla,
    /// Hairlines and borders.
    pub hair: Hsla,
    /// A quiet fill: chips, hunk headers, your own messages.
    pub tint: Hsla,
    /// Waiting on you; the one accent.
    pub buoy: Hsla,
    pub buoy_soft: Hsla,
    /// Failed, destructive.
    pub port: Hsla,
    /// Added lines.
    pub sea: Hsla,
    pub add_bg: Hsla,
    pub del_bg: Hsla,
    pub select: Hsla,
    pub shadow: Hsla,
}

const CHART: [u32; 14] = [
    0xEDEEEA, 0xFFFFFF, 0x0C1417, 0x6A7478, 0xE3E5E0, 0xF4F5F2, 0xFF5B14, 0xFFEADF, 0xD2352B,
    0x1F7A6B, 0xE9F4EF, 0xFBECEA, 0xECEEE9, 0x0C1417,
];
const DEEP: [u32; 14] = [
    0x0A1114, 0x121B1F, 0xEEF2EF, 0x8C999D, 0x223035, 0x18242A, 0xFF6B2C, 0x3A2217, 0xF0584C,
    0x4FC2A8, 0x12302A, 0x3A1D1C, 0x1C292F, 0x000000,
];

impl Palette {
    #[must_use]
    pub fn new(dark: bool) -> Self {
        let c = if dark { DEEP } else { CHART };
        let hsla = |hex: u32| Hsla::from(rgb(hex));
        Self {
            dark,
            canvas: hsla(c[0]),
            sheet: hsla(c[1]),
            ink: hsla(c[2]),
            muted: hsla(c[3]),
            hair: hsla(c[4]),
            tint: hsla(c[5]),
            buoy: hsla(c[6]),
            buoy_soft: hsla(c[7]),
            port: hsla(c[8]),
            sea: hsla(c[9]),
            add_bg: hsla(c[10]),
            del_bg: hsla(c[11]),
            select: hsla(c[12]),
            shadow: Hsla::from(rgba((c[13] << 8) | 0xFF)),
        }
    }

    /// A lane's colour: ink running, buoy waiting, port failed, and a quiet
    /// grey once finished.
    #[must_use]
    pub const fn lane(&self, lane: Lane) -> Hsla {
        match lane {
            Lane::Started => self.ink,
            Lane::Waiting => self.buoy,
            Lane::Failed => self.port,
            Lane::Finished => self.muted,
        }
    }

    /// The soft layered shadow every sheet casts.
    #[must_use]
    pub fn sheet_shadow(&self) -> Vec<gpui::BoxShadow> {
        let layer = |alpha: f32, y: f32, blur: f32| gpui::BoxShadow {
            color: tint(self.shadow, alpha),
            offset: gpui::point(gpui::px(0.), gpui::px(y)),
            blur_radius: gpui::px(blur),
            spread_radius: gpui::px(0.),
            inset: false,
        };
        if self.dark {
            vec![layer(0.4, 1., 2.), layer(0.35, 14., 36.)]
        } else {
            vec![layer(0.05, 1., 2.), layer(0.08, 12., 32.)]
        }
    }
}

/// `color` at `alpha`.
#[must_use]
pub fn tint(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
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

/// The logo, served to GPUI's `svg()`: an ink layer and an accent layer, so
/// each takes its colour from the theme.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "logo/ink.svg" => Some(Cow::Borrowed(include_bytes!("../../assets/logo/ink.svg"))),
            "logo/accent.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../assets/logo/accent.svg"
            ))),
            _ => None,
        })
    }

    fn list(&self, _: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(vec!["logo/ink.svg".into(), "logo/accent.svg".into()])
    }
}

/// Bundle the fonts, register both looks with gpui-component.
pub fn install(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../../assets/fonts/Geist-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/Geist-Medium.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/Geist-SemiBold.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/GeistMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/GeistMono-Medium.ttf")),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/PilotfishDisplay-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/PilotfishDisplay-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/PilotfishSerif-Regular.ttf"
        )),
    ];
    let _ = cx.text_system().add_fonts(fonts);
    let registry = ThemeRegistry::global_mut(cx);
    if registry.load_themes_from_str(&themes_json()).is_ok() {
        let themes = registry.themes().clone();
        let theme = Theme::global_mut(cx);
        if let Some(dark) = themes.get("Deep water") {
            theme.dark_theme = dark.clone();
        }
        if let Some(light) = themes.get("Chart") {
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

/// gpui-component's own widgets (inputs, scrollbars, markdown, code blocks)
/// in the same tokens.
fn themes_json() -> String {
    let theme = |name: &str, mode: &str, c: [u32; 14]| {
        let (canvas, sheet, ink, muted, hair, tint, buoy, port, sea) =
            (c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[8], c[9]);
        let dark = mode == "dark";
        let (keyword, function, string, number, kind) = if dark {
            (0xFF9A62, 0x8FC4F2, 0x6FD3B8, 0xE2B96A, 0xC3A6F2)
        } else {
            (0xB4400E, 0x1D5C8E, 0x1F7A6B, 0x8A5A00, 0x6B3FA0)
        };
        let colors: serde_json::Map<String, serde_json::Value> = [
            ("background", hex(sheet)),
            ("foreground", hex(ink)),
            ("border", hex(hair)),
            ("window.border", hex(hair)),
            ("caret", hex(buoy)),
            ("ring", hex(ink)),
            ("selection.background", hex_alpha(buoy, 0x40)),
            ("muted.background", hex(tint)),
            ("muted.foreground", hex(muted)),
            ("accent.background", hex(tint)),
            ("accent.foreground", hex(ink)),
            ("primary.background", hex(ink)),
            ("primary.foreground", hex(sheet)),
            ("primary.hover.background", hex_alpha(ink, 0xDD)),
            ("primary.active.background", hex(ink)),
            ("secondary.background", hex(tint)),
            ("secondary.foreground", hex(ink)),
            ("secondary.hover.background", hex(hair)),
            ("secondary.active.background", hex(hair)),
            ("popover.background", hex(sheet)),
            ("popover.foreground", hex(ink)),
            ("input.border", hex(hair)),
            ("list.background", hex(sheet)),
            ("list.hover.background", hex(tint)),
            ("list.active.background", hex(tint)),
            ("list.active.border", hex(ink)),
            ("list.even.background", hex(sheet)),
            ("list.head.background", hex(sheet)),
            ("link.foreground", hex(ink)),
            ("link.hover.foreground", hex(buoy)),
            ("link.active.foreground", hex(buoy)),
            ("scrollbar.background", hex_alpha(canvas, 0x00)),
            ("scrollbar.thumb.background", hex_alpha(muted, 0x66)),
            ("scrollbar.thumb.hover.background", hex(muted)),
            ("sidebar.background", hex(canvas)),
            ("sidebar.foreground", hex(ink)),
            ("sidebar.border", hex(hair)),
            ("title_bar.background", hex(canvas)),
            ("title_bar.border", hex_alpha(canvas, 0x00)),
            ("tab.background", hex(sheet)),
            ("tab.foreground", hex(muted)),
            ("tab.active.background", hex(sheet)),
            ("tab.active.foreground", hex(ink)),
            ("tab_bar.background", hex(sheet)),
            ("danger.background", hex(port)),
            ("danger.foreground", hex(0xFFFFFF)),
            ("success.background", hex(sea)),
            ("success.foreground", hex(0xFFFFFF)),
            ("warning.background", hex(buoy)),
            ("warning.foreground", hex(0xFFFFFF)),
            ("info.background", hex(tint)),
            ("info.foreground", hex(ink)),
            (
                "overlay",
                hex_alpha(0x000000, if dark { 0x80 } else { 0x26 }),
            ),
            ("skeleton.background", hex(tint)),
            ("group_box.background", hex(tint)),
            ("group_box.foreground", hex(ink)),
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
                serde_json::json!({ "color": hex(muted), "font_style": "italic" }),
            ),
            (
                "comment.doc",
                serde_json::json!({ "color": hex(muted), "font_style": "italic" }),
            ),
            ("type", serde_json::json!({ "color": hex(kind) })),
            ("constructor", serde_json::json!({ "color": hex(kind) })),
            ("attribute", serde_json::json!({ "color": hex(kind) })),
            ("tag", serde_json::json!({ "color": hex(keyword) })),
            (
                "title",
                serde_json::json!({ "color": hex(ink), "font_weight": 600 }),
            ),
            ("emphasis", serde_json::json!({ "font_style": "italic" })),
            ("emphasis.strong", serde_json::json!({ "font_weight": 600 })),
            ("link_text", serde_json::json!({ "color": hex(ink) })),
            ("link_uri", serde_json::json!({ "color": hex(muted) })),
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
            "radius": 8,
            "radius.lg": 16,
            "shadow": true,
            "colors": colors,
            "highlight": {
                "editor.background": hex(tint),
                "editor.foreground": hex(ink),
                "syntax": syntax,
            }
        })
    };
    serde_json::json!({
        "name": "Pilotfish",
        "author": "pilotfish",
        "themes": [theme("Chart", "light", CHART), theme("Deep water", "dark", DEEP)]
    })
    .to_string()
}
