//! Scripted screenshots for checking the UI, with no Screen Recording or
//! Accessibility permission: only in builds with the `desktop-shots`
//! feature, and only when `PILOTFISH_SHOTS=<dir>` is set.
//!
//! Append lines to `<dir>/script`; each runs once, in order:
//!
//! - `shot <name>` renders the window offscreen to `<dir>/<name>.png`
//! - `key <keystroke>` presses a key, as `Keystroke::parse` spells it
//! - `type <text>` types text into whatever has focus
//! - `click <x> <y>` clicks at a point in the main window (logical pixels)
//! - `drag <x1> <y1> <x2> <y2>` drags with the left button held
//! - `board` opens or closes the Board popup
//! - `wait <ms>` pauses the script
//!
//! `<dir>/done` holds how many lines have run.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AsyncApp, Keystroke, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, point, px,
};

pub fn start(main: AnyWindowHandle, cx: &mut App) {
    let Some(dir) = std::env::var_os("PILOTFISH_SHOTS").map(PathBuf::from) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    cx.spawn(async move |cx| {
        let mut done = 0usize;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let script = std::fs::read_to_string(dir.join("script")).unwrap_or_default();
            let lines: Vec<String> = script.lines().map(str::to_string).collect();
            while done < lines.len() {
                run(&lines[done], &dir, main, cx).await;
                done += 1;
                let _ = std::fs::write(dir.join("done"), done.to_string());
            }
        }
    })
    .detach();
}

async fn run(line: &str, dir: &std::path::Path, main: AnyWindowHandle, cx: &mut AsyncApp) {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    match verb {
        "wait" => {
            let ms = rest.trim().parse().unwrap_or(500);
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
        }
        "board" => {
            let _ = main.update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(super::ToggleBoard), cx);
            });
        }
        "key" => {
            if let Ok(keystroke) = Keystroke::parse(rest.trim()) {
                let _ = main.update(cx, |_, window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                });
            }
        }
        "type" => {
            let _ = main.update(cx, |_, window, cx| {
                for ch in rest.chars() {
                    let key = if ch == ' ' {
                        "space".to_string()
                    } else {
                        ch.to_string()
                    };
                    window.dispatch_keystroke(
                        Keystroke {
                            modifiers: Modifiers::default(),
                            key,
                            key_char: Some(ch.to_string()),
                        },
                        cx,
                    );
                }
            });
        }
        "click" => {
            let mut parts = rest
                .split_whitespace()
                .filter_map(|n| n.parse::<f32>().ok());
            let (Some(x), Some(y)) = (parts.next(), parts.next()) else {
                return;
            };
            let position = point(px(x), px(y));
            let _ = main.update(cx, |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            });
        }
        "drag" => {
            let n: Vec<f32> = rest
                .split_whitespace()
                .filter_map(|n| n.parse::<f32>().ok())
                .collect();
            let [x1, y1, x2, y2] = n[..] else {
                return;
            };
            let _ = main.update(cx, |_, window, cx| {
                let at = |x: f32, y: f32| point(px(x), px(y));
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: at(x1, y1),
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                for step in 1..=8 {
                    let t = step as f32 / 8.;
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position: at(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t),
                            pressed_button: Some(MouseButton::Left),
                            modifiers: Modifiers::default(),
                        }),
                        cx,
                    );
                }
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: at(x2, y2),
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            });
        }
        "shot" => {
            let name = rest.split_whitespace().next().unwrap_or("shot").to_string();
            let target = main;
            // one fresh frame first, so the shot shows the latest state
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = target.update(cx, |_, window, _| {
                if let Ok(image) = window.render_to_image() {
                    let _ = image.save(dir.join(format!("{name}.png")));
                }
            });
        }
        _ => {}
    }
}
