//! Selecting and copying the *rendered* text of mode 3.
//!
//! The byte selection cannot serve here: a rendered line is assembled from
//! bytes the file holds in different places, and copying "the bytes behind
//! it" would hand back Markdown markers and HTML tags the reader never saw.
//! So mode 3 has a selection of its own over what is drawn - a cursor that is
//! a (line, column) into the rendered text, an anchor set by the first
//! `Shift`, and a copy that joins the drawn lines with newlines. A collapsed
//! fold shows its summary and that is what is selected and copied for it;
//! lines a fold hides are skipped, as they are on screen.

use std::ops::Range;

use super::Viewer;
use super::select::{Extend, Motion};

/// The byte index of the character before `col` in `text`, or `0`.
fn prev_char(text: &str, col: usize) -> usize {
    text.get(..col)
        .and_then(|head| head.char_indices().next_back())
        .map_or(0, |(at, _)| at)
}

/// The byte index just past the character at `col` in `text`, or `col` at
/// the end.
fn next_char(text: &str, col: usize) -> usize {
    text.get(col..)
        .and_then(|tail| tail.chars().next())
        .map_or(col, |c| col.saturating_add(c.len_utf8()))
}

/// `col` moved back onto a character boundary of `text`, and inside it.
fn snap(text: &str, col: usize) -> usize {
    let mut col = col.min(text.len());
    while col > 0 && !text.is_char_boundary(col) {
        col = col.saturating_sub(1);
    }
    col
}

impl Viewer {
    /// The text a rendered line shows: its summary when it is collapsed.
    fn shown_text(&self, line: usize) -> Option<&str> {
        let rendered = self.rendered.as_ref()?;
        let source = rendered.lines.get(line)?;
        if self.render_folds.contains(&line)
            && let Some(fold) = &source.fold
        {
            return Some(&fold.summary);
        }
        Some(&source.text)
    }

    /// How long the shown text of `line` is, in bytes.
    pub(super) fn shown_len(&self, line: usize) -> usize {
        self.shown_text(line).map_or(0, str::len)
    }

    /// The rendered cursor's column.
    #[must_use]
    pub fn render_col(&self) -> usize {
        self.render_col
    }

    /// The rendered selection as `(from, to)` in (line, column), ordered,
    /// when it covers anything. An anchor the cursor has walked back onto is
    /// not a selection, the same rule the byte selection follows.
    #[must_use]
    pub fn render_selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.render_anchor?;
        let head = (self.render_cursor, self.render_col);
        if anchor == head {
            return None;
        }
        Some(if anchor < head {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }

    /// The part of `line`'s shown text the selection covers, for the row
    /// painter.
    pub(super) fn render_row_sel(&self, line: usize) -> Option<Range<usize>> {
        let (lo, hi) = self.render_selection()?;
        if line < lo.0 || line > hi.0 {
            return None;
        }
        let len = self.shown_len(line);
        let start = if line == lo.0 { lo.1 } else { 0 };
        let end = if line == hi.0 { hi.1 } else { len };
        (start < end).then_some(start..end)
    }

    /// Move the rendered cursor, extending the selection when asked.
    ///
    /// `Left`/`Right` walk characters and cross line ends; `Home`/`End` are
    /// the line's ends; everything else is the line walk [`Viewer::move_render`]
    /// already does, aiming for the column the last sideways move set so a
    /// blank line on the way does not lose it. A plain move drops the
    /// selection, as a plain arrow does in text mode.
    pub fn move_render_sel(&mut self, motion: Motion, extend: Extend) {
        match extend {
            Extend::None => self.render_anchor = None,
            Extend::Linear | Extend::Rectangular => {
                if self.render_anchor.is_none() {
                    self.render_anchor = Some((self.render_cursor, self.render_col));
                }
            }
        }
        let line = self.render_cursor;
        let col = self.render_col;
        match motion {
            Motion::Left => {
                if col > 0 {
                    let to = self.shown_text(line).map_or(0, |text| prev_char(text, col));
                    self.render_col = to;
                } else {
                    self.move_render(Motion::Up);
                    if self.render_cursor != line {
                        self.render_col = self.shown_len(self.render_cursor);
                    }
                }
            }
            Motion::Right => {
                let len = self.shown_len(line);
                if col < len {
                    let to = self
                        .shown_text(line)
                        .map_or(len, |text| next_char(text, col));
                    self.render_col = to;
                } else {
                    self.move_render(Motion::Down);
                    if self.render_cursor != line {
                        self.render_col = 0;
                    }
                }
            }
            Motion::RowStart => self.render_col = 0,
            Motion::RowEnd => self.render_col = self.shown_len(line),
            Motion::Up
            | Motion::Down
            | Motion::PageUp
            | Motion::PageDown
            | Motion::FileStart
            | Motion::FileEnd => {
                self.move_render(motion);
                let goal = self.render_goal;
                let snapped = self
                    .shown_text(self.render_cursor)
                    .map_or(0, |text| snap(text, goal));
                self.render_col = snapped;
                // A vertical move keeps aiming where it was.
                return;
            }
        }
        self.render_goal = self.render_col;
    }

    /// The selected rendered text, lines joined with newlines and hidden
    /// lines left out, or `None` when nothing is selected.
    #[must_use]
    pub fn rendered_selection_text(&self) -> Option<String> {
        let (lo, hi) = self.render_selection()?;
        let folded = self.folded();
        let mut out = String::new();
        let mut first = true;
        for line in lo.0..=hi.0 {
            if !folded.shows(line) {
                continue;
            }
            let Some(text) = self.shown_text(line) else {
                continue;
            };
            let start = if line == lo.0 { lo.1 } else { 0 };
            let end = if line == hi.0 { hi.1 } else { text.len() };
            if !first {
                out.push('\n');
            }
            first = false;
            out.push_str(text.get(start.min(end)..end).unwrap_or(""));
        }
        (!out.is_empty()).then_some(out)
    }

    /// `Esc`, stage one, for mode 3: true when there was a selection to drop.
    pub fn clear_render_selection(&mut self) -> bool {
        let had = self.render_selection().is_some();
        self.render_anchor = None;
        had
    }

    /// `Ctrl+A` in mode 3: everything drawn, first line to last.
    pub fn select_all_rendered(&mut self) {
        let Some(last) = self
            .rendered
            .as_ref()
            .map(|r| r.lines.len())
            .filter(|n| *n > 0)
        else {
            return;
        };
        let last = last.saturating_sub(1);
        self.render_anchor = Some((0, 0));
        self.render_cursor = last;
        self.render_col = self.shown_len(last);
        self.render_goal = self.render_col;
        self.reveal_render();
    }
}

#[cfg(test)]
#[path = "rendersel_tests.rs"]
mod tests;
