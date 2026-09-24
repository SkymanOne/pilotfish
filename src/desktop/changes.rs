//! The changes pane: the selected worker's patch as one virtual list of
//! fixed-height rows — file headers, hunk headers, numbered lines — so a
//! twenty-thousand-line diff costs what a twenty-line one does. Lines are
//! highlighted per hunk with gpui-component's tree-sitter grammars.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, App, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, StyledText, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::highlighter::SyntaxHighlighter;
use gpui_component::{ActiveTheme as _, Rope};

use super::theme::{MONO, Palette};
use crate::patch::{FileDiff, FileStatus, LineKind, Patch};

/// Every row is this tall: `uniform_list` sizes them all from the first.
/// A line's highlight runs, by byte range.
type Spans = Vec<(Range<usize>, HighlightStyle)>;

pub const ROW_HEIGHT: f32 = 22.;

/// Lockfiles and generated output start folded: nobody reviews them.
fn starts_folded(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "Cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "go.sum"
    ) || name.ends_with(".min.js")
        || name.ends_with(".min.css")
}

/// What one row of the pane is.
#[derive(Debug, Clone)]
pub enum Row {
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    UntrackedHeader,
    Untracked(usize),
    Truncated,
}

/// The pane's memory across refreshes of the same worker: folds, and the
/// highlights of the patch it last saw.
#[derive(Default)]
pub struct ChangesState {
    run_id: Option<String>,
    patch: Option<Arc<Patch>>,
    folded: HashSet<String>,
    touched: HashSet<String>,
    highlights: HashMap<(usize, usize, usize), Spans>,
    pub rows: Vec<Row>,
    /// The row with the longest text: what the list measures to know how
    /// far it scrolls sideways.
    pub widest: usize,
}

impl ChangesState {
    /// Take in a (possibly) new patch; the rows and highlights are rebuilt
    /// only when it actually changed.
    pub fn update(&mut self, run_id: &str, patch: &Arc<Patch>, cx: &App) {
        if self.run_id.as_deref() != Some(run_id) {
            self.folded.clear();
            self.touched.clear();
            self.run_id = Some(run_id.to_string());
            self.patch = None;
        }
        if self.patch.as_ref().is_some_and(|p| Arc::ptr_eq(p, patch)) {
            return;
        }
        for file in &patch.files {
            if !self.touched.contains(&file.path) && starts_folded(&file.path) {
                self.folded.insert(file.path.clone());
            }
        }
        self.patch = Some(patch.clone());
        self.highlight(cx);
        self.rebuild();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn toggle(&mut self, path: &str) {
        self.touched.insert(path.to_string());
        if !self.folded.remove(path) {
            self.folded.insert(path.to_string());
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let Some(patch) = &self.patch else {
            self.rows.clear();
            return;
        };
        let mut rows = Vec::new();
        for (f, file) in patch.files.iter().enumerate() {
            rows.push(Row::File(f));
            if self.folded.contains(&file.path) {
                continue;
            }
            for (h, hunk) in file.hunks.iter().enumerate() {
                rows.push(Row::Hunk(f, h));
                rows.extend((0..hunk.lines.len()).map(|l| Row::Line(f, h, l)));
            }
        }
        if patch.truncated {
            rows.push(Row::Truncated);
        }
        if !patch.untracked.is_empty() {
            rows.push(Row::UntrackedHeader);
            rows.extend((0..patch.untracked.len()).map(Row::Untracked));
        }
        let width = |row: &Row| match row {
            Row::File(f) => file_title(&patch.files[*f]).chars().count() + 16,
            Row::Hunk(f, h) => patch.files[*f].hunks[*h].header.chars().count(),
            Row::Line(f, h, l) => {
                patch.files[*f].hunks[*h].lines[*l]
                    .text
                    .replace('\t', "    ")
                    .chars()
                    .count()
                    + 12
            }
            Row::Untracked(i) => patch.untracked[*i].chars().count() + 4,
            Row::UntrackedHeader | Row::Truncated => 0,
        };
        self.widest = rows
            .iter()
            .enumerate()
            .max_by_key(|(_, row)| width(row))
            .map_or(0, |(ix, _)| ix);
        self.rows = rows;
    }

    /// One snippet per hunk, in the language its file's extension names.
    fn highlight(&mut self, cx: &App) {
        self.highlights.clear();
        let Some(patch) = self.patch.clone() else {
            return;
        };
        let theme = cx.theme().highlight_theme.clone();
        for (f, file) in patch.files.iter().enumerate() {
            let Some(ext) = file.path.rsplit_once('.').map(|(_, ext)| ext) else {
                continue;
            };
            if file.binary {
                continue;
            }
            for (h, hunk) in file.hunks.iter().enumerate() {
                let mut text = String::new();
                let mut starts = Vec::with_capacity(hunk.lines.len());
                for line in &hunk.lines {
                    starts.push(text.len());
                    text.push_str(&line.text);
                    text.push('\n');
                }
                let mut highlighter = SyntaxHighlighter::new(ext);
                highlighter.update(None, &Rope::from(text.as_str()), None);
                let styles = highlighter.styles(&(0..text.len()), &*theme);
                for (l, line) in hunk.lines.iter().enumerate() {
                    let start = starts[l];
                    let end = start + line.text.len();
                    let spans: Vec<_> = styles
                        .iter()
                        .filter(|(range, _)| range.start < end && range.end > start)
                        .map(|(range, style)| {
                            (
                                range.start.max(start) - start..range.end.min(end) - start,
                                *style,
                            )
                        })
                        .filter(|(range, _)| !range.is_empty())
                        .collect();
                    if !spans.is_empty() {
                        self.highlights.insert((f, h, l), spans);
                    }
                }
            }
        }
    }

    pub fn is_folded(&self, path: &str) -> bool {
        self.folded.contains(path)
    }

    /// Draw row `ix`; `on_fold` is what clicking a file header does.
    pub fn render_row(
        &self,
        ix: usize,
        pal: &Palette,
        on_fold: impl Fn(String, &mut App) + 'static,
    ) -> AnyElement {
        let Some(patch) = &self.patch else {
            return div().into_any_element();
        };
        let base = div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap();
        match self.rows.get(ix) {
            Some(Row::File(f)) => {
                let file = &patch.files[*f];
                let path = file.path.clone();
                let folded = self.is_folded(&path);
                base.id(("file", *f))
                    .px(px(12.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(pal.line)
                    .bg(pal.panel)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| on_fold(path.clone(), cx))
                    .child(
                        div()
                            .text_color(pal.muted)
                            .child(if folded { "▸" } else { "▾" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .font_weight(FontWeight::MEDIUM)
                            .child(file_title(file)),
                    )
                    .children(file_tag(file, pal))
                    .child(stat(file.added, file.removed, pal))
                    .into_any_element()
            }
            Some(Row::Hunk(f, h)) => base
                .px(px(12.))
                .bg(pal.panel)
                .font_family(MONO)
                .text_size(px(11.5))
                .text_color(pal.muted)
                .child(patch.files[*f].hunks[*h].header.clone())
                .into_any_element(),
            Some(Row::Line(f, h, l)) => {
                let line = &patch.files[*f].hunks[*h].lines[*l];
                let (bg, sign, sign_color) = match line.kind {
                    LineKind::Added => (Some(pal.add_bg), "+", pal.run),
                    LineKind::Removed => (Some(pal.del_bg), "−", pal.fail),
                    LineKind::Context => (None, "", pal.muted),
                };
                let number = |n: Option<u32>| {
                    div()
                        .w(px(36.))
                        .flex_none()
                        .pr(px(6.))
                        .text_right()
                        .text_color(pal.muted)
                        .child(n.map(|n| n.to_string()).unwrap_or_default())
                };
                let code: SharedString = line.text.replace('\t', "    ").into();
                let text = match self.highlights.get(&(*f, *h, *l)) {
                    Some(spans) if !line.text.contains('\t') => {
                        StyledText::new(code).with_highlights(spans.iter().cloned())
                    }
                    _ => StyledText::new(code),
                };
                base.font_family(MONO)
                    .text_size(px(11.5))
                    .when_some(bg, |this, bg| this.bg(bg))
                    .child(number(line.old))
                    .child(number(line.new))
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .text_color(sign_color)
                            .child(sign),
                    )
                    .child(div().text_color(pal.text).child(text))
                    .into_any_element()
            }
            Some(Row::Truncated) => base
                .px(px(12.))
                .text_size(px(12.))
                .text_color(pal.wait)
                .child("Diff truncated at 512 KiB.")
                .into_any_element(),
            Some(Row::UntrackedHeader) => base
                .px(px(12.))
                .border_t_1()
                .border_color(pal.line)
                .bg(pal.panel)
                .text_size(px(12.))
                .text_color(pal.muted)
                .child("Untracked, not in the diff yet")
                .into_any_element(),
            Some(Row::Untracked(i)) => base
                .px(px(26.))
                .font_family(MONO)
                .text_size(px(11.5))
                .text_color(pal.text)
                .child(patch.untracked[*i].clone())
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}

fn file_title(file: &FileDiff) -> String {
    match (&file.status, &file.old_path) {
        (FileStatus::Renamed, Some(old)) => format!("{old} → {}", file.path),
        _ => file.path.clone(),
    }
}

fn file_tag(file: &FileDiff, pal: &Palette) -> Option<AnyElement> {
    let text = if file.binary {
        "binary"
    } else {
        match file.status {
            FileStatus::Added => "new",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed if file.added + file.removed == 0 => "renamed",
            _ if file.mode_change && file.hunks.is_empty() => "mode",
            _ => return None,
        }
    };
    Some(
        div()
            .px(px(5.))
            .rounded(px(4.))
            .border_1()
            .border_color(pal.muted)
            .text_size(px(10.5))
            .text_color(pal.muted)
            .child(text)
            .into_any_element(),
    )
}

/// `+12 −3`, in the lane colours, tabular.
pub fn stat(added: usize, removed: usize, pal: &Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .gap(px(4.))
        .text_size(px(12.))
        .child(div().text_color(pal.run).child(format!("+{added}")))
        .child(div().text_color(pal.fail).child(format!("−{removed}")))
}
