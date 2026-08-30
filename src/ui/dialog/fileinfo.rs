//! The file information dialog: what this file is, in two halves.
//!
//! ```text
//!   ┌──────────────── screenshot.png ────────────────┐
//!   │ Size          24,576 bytes (24 KB)             │
//!   │ Attributes    -rw-r--r--                       │
//!   │ Modified      2024-03-01 09:15                 │
//!   │                                                │
//!   │ PNG image                                      │
//!   │ Dimensions    1920 x 1080 px                   │
//!   │ Colour        RGBA                             │
//!   │ Bit depth     8 bits per channel               │
//!   │ Compression   deflate                          │
//!   └────────────────────────────────────────────────┘
//! ```
//!
//! # Why the top half is always there
//!
//! The name, the size and the attributes are true of every file, including the
//! ones nothing recognises, and most files are ones nothing recognises. A
//! dialog that only had something to say about a PNG would be a dialog that
//! was empty most of the times it was opened; this one always answers, and the
//! second half appears when there is a second half to give.
//!
//! # Where the answer comes from
//!
//! [`crate::viewer::fileinfo::describe`], which does no I/O: the caller passes
//! the facts it already has and the bytes it has already read. This module
//! lays the answer out and nothing else.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogStyle};
use crate::input::{DialogId, KeyCode};
use crate::viewer::fileinfo::FileInfo;
use crate::viewer::summary::SummaryLine;

/// The widest the box will ask to be, borders included.
const MAX_WIDTH: u16 = 62;

/// The most rows of content it will ask for, before scrolling.
const MAX_ROWS: u16 = 20;

/// One drawn row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// A label and a value, in two columns.
    Pair(String, String),
    /// A heading, on its own and emphasised.
    Head(String),
    /// A sentence, wrapped over the width it is given.
    Note(String),
    /// Nothing, to separate the halves.
    Gap,
}

/// The file information dialog.
#[derive(Debug)]
pub struct FileInfoDialog {
    title: String,
    rows: Vec<Row>,
    /// The first visible row, for a file with more to say than fits.
    scroll: usize,
    /// The interior height of the last frame, so paging moves by a page.
    height: usize,
}

/// The rows one half contributes.
fn pairs(lines: &[SummaryLine]) -> impl Iterator<Item = Row> + '_ {
    lines
        .iter()
        .map(|line| Row::Pair(line.label.clone(), line.value.clone()))
}

impl FileInfoDialog {
    /// A dialog that states a result rather than describing a file.
    ///
    /// The two-file compare verdict is built with this. It is the same box,
    /// the same `Close`, the same scrolling and the same label alignment; a
    /// second dialog type differing only in its sentences would be this file
    /// again with other words in it.
    #[must_use]
    pub fn statement(
        title: impl Into<String>,
        facts: Vec<(String, String)>,
        note: impl Into<String>,
    ) -> Self {
        let mut rows: Vec<Row> = facts
            .into_iter()
            .map(|(label, value)| Row::Pair(label, value))
            .collect();
        rows.push(Row::Gap);
        rows.push(Row::Note(note.into()));
        Self {
            title: title.into(),
            rows,
            scroll: 0,
            height: usize::from(MAX_ROWS),
        }
    }

    /// Lay out one answer from [`crate::viewer::fileinfo::describe`].
    #[must_use]
    pub fn new(info: &FileInfo) -> Self {
        let mut rows: Vec<Row> = pairs(&info.facts).collect();
        if let Some(format) = info.format.as_ref() {
            rows.push(Row::Gap);
            rows.push(Row::Head(format.clone()));
            rows.extend(pairs(&info.lines));
        }
        if let Some(note) = info.note.as_ref() {
            rows.push(Row::Gap);
            rows.push(Row::Note(note.clone()));
        }
        Self {
            title: info.name.clone(),
            rows,
            scroll: 0,
            height: usize::from(MAX_ROWS),
        }
    }

    /// The widest label, which is where the value column starts.
    fn label_width(&self) -> usize {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Pair(label, _) => Some(label.chars().count()),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Every row as one line of text, for tests and for the width hint.
    #[must_use]
    pub fn text_rows(&self) -> Vec<String> {
        let width = self.label_width();
        self.rows
            .iter()
            .map(|row| match row {
                Row::Pair(label, value) => format!("{label:<width$}  {value}"),
                Row::Head(text) | Row::Note(text) => text.clone(),
                Row::Gap => String::new(),
            })
            .collect()
    }

    /// How many rows there are to show.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The first visible row, for tests.
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    /// Scroll, stopping at the last row rather than past it.
    fn scroll_to(&mut self, to: usize) {
        let last = self.rows.len().saturating_sub(self.height.max(1));
        self.scroll = to.min(last);
    }
}

impl Dialog for FileInfoDialog {
    fn id(&self) -> DialogId {
        DialogId::FileSummary
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = self
            .text_rows()
            .iter()
            .map(|row| u16::try_from(row.chars().count()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(20);
        // Two for the borders and two for the padding either side.
        let want = widest.saturating_add(4).min(MAX_WIDTH);
        // Never narrower than the title, which is a file name and is the one
        // thing the box must not crop.
        let title = u16::try_from(self.title.chars().count()).unwrap_or(u16::MAX);
        let want = want.max(title.saturating_add(6).min(MAX_WIDTH));
        let rows = u16::try_from(self.rows.len()).unwrap_or(MAX_ROWS);
        (want, rows.min(MAX_ROWS).saturating_add(2))
    }

    /// Without this the caller cannot read the dialog back at all.
    ///
    /// `Dialog::as_any` defaults to `None`, and a missing override is not a
    /// compile error and not a panic: the downcast simply fails and whatever
    /// wanted to inspect this dialog silently gets nothing.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn layout(&mut self, area: Rect) {
        self.height = usize::from(area.height).max(1);
        // A box that grew can leave the scroll past the end.
        let at = self.scroll;
        self.scroll_to(at);
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // There is nothing to choose here, so both keys that close a dialog
        // close it, and neither answers anything.
        if key.is_cancel() || key.is_accept() {
            return DialogOutcome::Cancel;
        }
        let page = self.height.max(1);
        match key.press.code {
            KeyCode::Up => self.scroll_to(self.scroll.saturating_sub(1)),
            KeyCode::Down => self.scroll_to(self.scroll.saturating_add(1)),
            KeyCode::PageUp => self.scroll_to(self.scroll.saturating_sub(page)),
            KeyCode::PageDown => self.scroll_to(self.scroll.saturating_add(page)),
            KeyCode::Home => self.scroll_to(0),
            KeyCode::End => self.scroll_to(self.rows.len()),
            _ => return DialogOutcome::Ignored,
        }
        DialogOutcome::Consumed
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let label_width = self.label_width();
        for offset in 0..usize::from(area.height) {
            let Some(row) = self.rows.get(self.scroll.saturating_add(offset)) else {
                break;
            };
            let Ok(y) = u16::try_from(offset) else {
                break;
            };
            let spans = match row {
                Row::Gap => vec![Span::styled(" ", style.body())],
                Row::Head(text) => vec![Span::styled(
                    text.clone(),
                    style.body().add_modifier(Modifier::BOLD),
                )],
                Row::Note(text) => vec![Span::styled(
                    crate::ui::text::fit_left(
                        text,
                        width,
                        crate::ui::text::Crop::End,
                        super::ellipsis(style.ascii),
                    ),
                    style.body(),
                )],
                Row::Pair(label, value) => vec![
                    Span::styled(
                        format!("{label:<label_width$}  "),
                        style.body().add_modifier(Modifier::DIM),
                    ),
                    Span::styled(value.clone(), style.body()),
                ],
            };
            let rect = Rect {
                x: area.x,
                y: area.y.saturating_add(y),
                width: area.width,
                height: 1,
            };
            f.render_widget(Paragraph::new(Line::from(spans)), rect);
        }
    }
}

#[cfg(test)]
#[path = "fileinfo_tests.rs"]
mod tests;
