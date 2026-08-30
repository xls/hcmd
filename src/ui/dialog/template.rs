//! The binary struct template picker, for the hex viewer.
//!
//! ```text
//!   ┌─ disk.img ───────────────────┐┌─── Template ───┐
//!   │ 00000000  eb 3c 90 4d 53 44  ││ ELF64          │
//!   │ 00000010  02 00 00 00 00 f8  ││ FAT32 BPB      │
//!   │ 00000020  00 00 00 00 00 00  ││▓GPT header▓▓▓▓▓│
//!   │ 00000030  00 00 00 00 80 00  ││ GZIP           │
//!   │ 00000040  00 00 00 00 00 00  ││ MBR            │
//!   └──────────────────────────────┘└────────────────┘
//! ```
//!
//! # Why it is narrow, like the theme picker
//!
//! The same reason, for a different subject: the answer to "is this the right
//! template" is on the dump behind the box, not in the list. A picker wide
//! enough to be comfortable would cover the bytes it is about to explain. So
//! it asks for the width of its longest name and no more.
//!
//! The list is names only. Which fields a template has is a question the
//! viewer answers by applying it, and a picker that tried to preview them
//! would be a worse hex dump beside a real one.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config::{QuickSearchCase, QuickSearchMode};
use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle};
use crate::input::quicksearch::quick_match;
use crate::input::{DialogId, KeyCode};

/// How many rows the list asks for, and how far `PageUp`/`PageDown` move.
///
/// The shipped set is long enough to scroll, and the framework clamps this to
/// the screen on a short terminal either way.
const LIST_ROWS: u16 = 14;

/// The widest the box will ask to be, borders included.
///
/// Capped rather than fitted to the longest name: the dump behind it is the
/// thing being read.
const MAX_WIDTH: u16 = 28;

/// The row that takes the applied template away again.
///
/// The list is otherwise a list of things to turn on, with no way to turn one
/// off: `Esc` deliberately leaves whatever is applied alone, so without this
/// row a template chosen once could never be removed. It is a name rather than
/// a second key because the picker is where a person already is when they
/// decide they want the plain dump back.
pub const NONE: &str = "(none)";

/// The template picker.
#[derive(Debug)]
pub struct TemplateDialog {
    /// Every template name that can be chosen, in the order they are offered.
    names: Vec<String>,
    cursor: usize,
    quick: String,
    mode: QuickSearchMode,
    case: QuickSearchCase,
}

impl TemplateDialog {
    /// Open on `current`, the template already applied, or at the top when
    /// there is none.
    ///
    /// Starting on what is applied rather than at the top is what makes
    /// stepping to the next candidate and back a comparison rather than a
    /// search.
    pub fn new(names: Vec<String>, current: Option<&str>) -> Self {
        let cursor = current
            .and_then(|current| names.iter().position(|n| n == current))
            .unwrap_or(0);
        Self {
            names,
            cursor,
            quick: String::new(),
            mode: QuickSearchMode::default(),
            case: QuickSearchCase::default(),
        }
    }

    /// Match the list's quick search to the panel's configured rules.
    #[must_use]
    pub const fn with_quick_search(mut self, mode: QuickSearchMode, case: QuickSearchCase) -> Self {
        self.mode = mode;
        self.case = case;
        self
    }

    /// The name under the cursor: the template `Enter` would apply.
    pub fn selected(&self) -> Option<&str> {
        self.names.get(self.cursor).map(String::as_str)
    }

    /// Which row the cursor is on, for tests.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The quick-search buffer, for tests.
    pub fn quick_buffer(&self) -> &str {
        &self.quick
    }

    /// Move the cursor, ending any quick search in progress.
    fn move_cursor(&mut self, to: usize) {
        if self.names.is_empty() {
            return;
        }
        self.cursor = to.min(self.names.len().saturating_sub(1));
        self.quick.clear();
    }

    /// One keystroke of panel quick search over the names.
    ///
    /// A character that matches nothing is refused rather than typed, exactly
    /// as it is in a panel and in the theme picker: with this many formats,
    /// typing `png` is the fast way in, and a buffer holding a name that is
    /// not there would leave the cursor somewhere unrelated to what was typed.
    fn quick_search(&mut self, ch: char) -> bool {
        let mut candidate = self.quick.clone();
        candidate.push(ch);
        let Some(found) = self
            .names
            .iter()
            .position(|name| quick_match(name, &candidate, self.mode, self.case))
        else {
            return false;
        };
        self.quick = candidate;
        self.cursor = found;
        true
    }

    /// The rows visible in a body `height` rows tall: the page the cursor is
    /// on, so moving within a page does not scroll.
    fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.names.is_empty() {
            return 0..0;
        }
        let page = self.cursor / height;
        let start = page.saturating_mul(height);
        let end = start.saturating_add(height).min(self.names.len());
        start..end
    }
}

impl Dialog for TemplateDialog {
    fn id(&self) -> DialogId {
        DialogId::Template
    }

    fn title(&self) -> String {
        "Template".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = self
            .names
            .iter()
            .map(|n| u16::try_from(n.chars().count()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(8);
        // Two for the borders and two for the padding either side of a name.
        let want = widest.saturating_add(4).min(MAX_WIDTH);
        (want, LIST_ROWS.saturating_add(2))
    }

    /// Without this the caller cannot read the selection at all.
    ///
    /// `Dialog::as_any` defaults to `None`, and anything that wants to know
    /// which template the cursor is on reaches this dialog by downcasting. A
    /// `None` here is not a compile error and not a panic: the downcast simply
    /// fails and the picker looks like a list that does nothing. The theme
    /// picker carries the same note for the same reason.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn mnemonic_letters(&self) -> Vec<char> {
        // None, deliberately: every letter is quick search over the names, and
        // an accelerator would take one of them away from the thing the list
        // is for.
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return match self.selected() {
                Some(name) => DialogOutcome::Accept(DialogResult::Text(name.to_string())),
                None => DialogOutcome::Cancel,
            };
        }
        let last = self.names.len().saturating_sub(1);
        let page = usize::from(LIST_ROWS);
        match key.press.code {
            KeyCode::Up => self.move_cursor(self.cursor.saturating_sub(1)),
            KeyCode::Down => self.move_cursor(self.cursor.saturating_add(1)),
            KeyCode::PageUp => self.move_cursor(self.cursor.saturating_sub(page)),
            KeyCode::PageDown => self.move_cursor(self.cursor.saturating_add(page)),
            KeyCode::Home => self.move_cursor(0),
            KeyCode::End => self.move_cursor(last),
            KeyCode::Backspace => {
                // Same as a panel's: rubbing out a character re-runs the
                // shorter search rather than leaving the cursor where the
                // longer one put it.
                self.quick.pop();
                let buffer = self.quick.clone();
                if !buffer.is_empty()
                    && let Some(found) = self
                        .names
                        .iter()
                        .position(|name| quick_match(name, &buffer, self.mode, self.case))
                {
                    self.cursor = found;
                }
            }
            KeyCode::Char(c) => {
                if !self.quick_search(c) {
                    return DialogOutcome::Ignored;
                }
            }
            _ => return DialogOutcome::Ignored,
        }
        DialogOutcome::Consumed
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let height = usize::from(area.height);
        let visible = self.window(height);
        let width = usize::from(area.width);

        for (offset, index) in visible.enumerate() {
            let Ok(row) = u16::try_from(offset) else {
                break;
            };
            if row >= area.height {
                break;
            }
            let Some(name) = self.names.get(index) else {
                break;
            };
            // The panel's own cursor bar, which is what a selected row is
            // everywhere else in the program.
            let row_style = if index == self.cursor {
                style.row_cursor(true).add_modifier(Modifier::BOLD)
            } else {
                style.body()
            };
            // Padded to the full width so the cursor bar is a bar rather than
            // a highlight the length of the word.
            let text = format!(" {name:<width$}", width = width.saturating_sub(1));
            let text = crate::ui::text::fit_left(
                &text,
                width,
                crate::ui::text::Crop::End,
                super::ellipsis(style.ascii),
            );
            let rect = Rect {
                x: area.x,
                y: area.y.saturating_add(row),
                width: area.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(text, row_style))),
                rect,
            );
        }
    }
}

#[cfg(test)]
#[path = "template_tests.rs"]
mod tests;
