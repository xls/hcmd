//! A one-line field with a **preselected range**.
//!
//! This exists for one sentence in
//!
//! > The **mask portion is preselected** so typing replaces it and leaves the
//! > path - this is what makes "copy these as `*.bak`" a two-second operation.
//!
//! That is the point of the copy dialog, and neither [`crate::input::CommandLine`]
//! nor [`crate::dialog::InputDialog`] has a selection: nothing in v0.1 needed
//! one, because the command line's caret is persistent state and
//! a persistent selection would be a second thing to reason about there.
//!
//! # The editor underneath is still `CommandLine`
//!
//! the caret is a **character** index and can never
//! slice a `String` at a non-boundary, and word kill, line kill and the
//! display-width caret column come for free. This type adds a selection and
//! nothing else - it does not re-implement a line editor, which is how the
//! second, subtly different one gets written.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dialog::{DialogKey, DialogStyle};
use crate::input::{Action, CommandLine, KeyCode};
use crate::ui::text;

/// A one-line text field that can carry a preselected range.
#[derive(Debug)]
pub struct Field {
    line: CommandLine,
    /// The selected range as `(start, end)` **character** indices, `start <=
    /// end`. Cleared by any caret movement or edit, exactly like a terminal
    /// selection: it exists to be typed over, not to be maintained.
    selection: Option<(usize, usize)>,
}

impl Field {
    /// An empty field.
    pub fn new() -> Self {
        Self {
            line: CommandLine::new(),
            selection: None,
        }
    }

    /// A field holding `text`, caret at the end.
    pub fn with_text(text: impl Into<String>) -> Self {
        let mut field = Self::new();
        field.set_text(text);
        field
    }

    /// A target field with its **mask portion preselected**.
    ///
    /// The mask is everything after the last separator: `/srv/media/*.*`
    /// preselects `*.*`, so the first character typed replaces it and leaves
    /// `/srv/media/` alone. A target with no mask - one that ends in a
    /// separator - selects nothing, because there is nothing to type over.
    pub fn with_mask_selected(text: impl Into<String>) -> Self {
        let mut field = Self::with_text(text);
        let chars: Vec<char> = field.line.text().chars().collect();
        let start = chars
            .iter()
            .rposition(|c| *c == '/')
            .map_or(0, |i| i.saturating_add(1));
        let end = chars.len();
        if start < end {
            field.select(start, end);
        }
        field
    }

    /// What has been typed.
    pub fn text(&self) -> &str {
        self.line.text()
    }

    /// Replace the contents, caret at the end, selection cleared.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.line.set_text(text);
        self.line.move_end();
        self.selection = None;
    }

    /// The caret, as a character index.
    pub fn caret(&self) -> usize {
        self.line.caret()
    }

    /// The selected range, as character indices.
    pub const fn selection(&self) -> Option<(usize, usize)> {
        self.selection
    }

    /// The selected text, for tests and for a "what will typing replace?"
    /// question.
    pub fn selected_text(&self) -> String {
        let Some((start, end)) = self.selection else {
            return String::new();
        };
        self.line
            .text()
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect()
    }

    /// Select `start..end`, clamped to the text, and put the caret at `end`.
    pub fn select(&mut self, start: usize, end: usize) {
        let len = self.line.char_count();
        let start = start.min(len);
        let end = end.min(len).max(start);
        self.selection = (start < end).then_some((start, end));
        self.line.set_caret(end);
    }

    /// Drop the selection without touching the text.
    pub const fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// True when the field holds nothing but whitespace.
    pub fn is_empty(&self) -> bool {
        self.line.text().trim().is_empty()
    }

    /// Delete the selection, if there is one. Returns whether anything went.
    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection.take() else {
            return false;
        };
        self.line.set_caret(start);
        for _ in start..end {
            if !self.line.delete() {
                break;
            }
        }
        true
    }

    /// Handle one key. Returns false when the key is not a field key, so the
    /// dialog can do something else with it.
    ///
    /// `Enter`, `Esc` and `Tab` are deliberately **not** handled here: they
    /// belong to the dialog, which has buttons to press and a stack to unwind.
    pub fn handle(&mut self, key: &DialogKey) -> bool {
        // A binding resolved in the `dialog` context wins over the raw key
        // (the design steps 1-3), the same order `InputDialog` uses.
        match key.action {
            Some(Action::KillLine) => {
                self.selection = None;
                self.line.kill_line();
                return true;
            }
            Some(Action::KillWord) => {
                self.delete_selection();
                self.line.kill_word();
                return true;
            }
            Some(Action::KillToEnd) => {
                self.delete_selection();
                self.line.kill_to_end();
                return true;
            }
            _ => {}
        }

        match key.press.code {
            KeyCode::Left => {
                self.selection = None;
                self.line.move_left();
            }
            KeyCode::Right => {
                self.selection = None;
                self.line.move_right();
            }
            KeyCode::Home => {
                self.selection = None;
                self.line.move_home();
            }
            KeyCode::End => {
                self.selection = None;
                self.line.move_end();
            }
            KeyCode::Backspace => {
                if !self.delete_selection() {
                    self.line.backspace();
                }
            }
            KeyCode::Delete => {
                if !self.delete_selection() {
                    self.line.delete();
                }
            }
            KeyCode::Insert => self.line.overwrite = !self.line.overwrite,
            _ => match key.text() {
                // The whole point: the first character typed replaces the
                // preselected mask and leaves the path.
                Some(c) => {
                    self.delete_selection();
                    self.line.insert_char(c);
                }
                None => return false,
            },
        }
        true
    }

    /// Draw the field into one row, scrolled so the caret is visible.
    pub fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let caret_col = self.line.display_width_to_caret();
        let scroll = text::caret_window(caret_col, width);

        let ellipsis = super::ellipsis(style.ascii);
        let base = style.input();
        let Some((start, end)) = self.selection else {
            let body = text::slice_columns(self.line.text(), scroll, width);
            let padded = text::fit_left(&body, width, text::Crop::End, ellipsis);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(padded, base))).style(base),
                area,
            );
            return;
        };

        // Three spans: before the selection, the selection, after it. Split on
        // *character* indices and measured in display columns, so a CJK path
        // cannot shear the highlight off the text.
        let chars: Vec<char> = self.line.text().chars().collect();
        let take = |from: usize, to: usize| -> String {
            chars
                .iter()
                .skip(from)
                .take(to.saturating_sub(from))
                .collect()
        };
        let head = take(0, start);
        let mid = take(start, end);
        let tail = take(end, chars.len());
        let selected = base.add_modifier(Modifier::REVERSED);

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize;
        let mut used = 0usize;
        for (piece, piece_style) in [(head, base), (mid, selected), (tail, base)] {
            let piece_w = text::width(&piece);
            let end_col = col.saturating_add(piece_w);
            // The visible window is `scroll..scroll + width`; a piece entirely
            // outside it contributes nothing.
            if end_col > scroll && used < width {
                let from = scroll.saturating_sub(col);
                let body = text::slice_columns(&piece, from, width.saturating_sub(used));
                used = used.saturating_add(text::width(&body));
                if !body.is_empty() {
                    spans.push(Span::styled(body, piece_style));
                }
            }
            col = end_col;
        }
        if used < width {
            spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), base));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
    }

    /// Where the hardware cursor goes when this field has focus.
    pub fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let caret_col = self.line.display_width_to_caret();
        let col = caret_col.saturating_sub(text::caret_window(caret_col, usize::from(area.width)));
        let x = area
            .x
            .saturating_add(u16::try_from(col).unwrap_or(u16::MAX))
            .min(area.right().saturating_sub(1));
        Some((x, area.y))
    }
}

impl Default for Field {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyModifiers, KeyPress};

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn typed(field: &mut Field, text: &str) {
        for c in text.chars() {
            field.handle(&key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_over_the_preselected_mask_leaves_the_path() {
        // the whole reason this type exists.
        let mut field = Field::with_mask_selected("/srv/media/*.*");
        assert_eq!(field.selected_text(), "*.*");
        typed(&mut field, "*.bak");
        assert_eq!(field.text(), "/srv/media/*.bak");
        assert_eq!(field.selection(), None, "one keystroke ends the selection");
    }

    #[test]
    fn a_target_with_no_mask_preselects_nothing() {
        let field = Field::with_mask_selected("/srv/media/");
        assert_eq!(field.selection(), None);
        assert_eq!(field.text(), "/srv/media/");

        // A bare name is all mask.
        let field = Field::with_mask_selected("*.*");
        assert_eq!(field.selected_text(), "*.*");
    }

    #[test]
    fn moving_the_caret_abandons_the_selection_rather_than_editing_it() {
        let mut field = Field::with_mask_selected("/srv/media/*.*");
        field.handle(&key(KeyCode::Left));
        assert_eq!(field.selection(), None);
        typed(&mut field, "z");
        assert_eq!(field.text(), "/srv/media/*.z*", "an ordinary insert");
    }

    #[test]
    fn backspace_deletes_the_selection_whole() {
        let mut field = Field::with_mask_selected("/srv/media/*.*");
        field.handle(&key(KeyCode::Backspace));
        assert_eq!(field.text(), "/srv/media/");
        assert_eq!(field.selection(), None);
        // And a second backspace is an ordinary one.
        field.handle(&key(KeyCode::Backspace));
        assert_eq!(field.text(), "/srv/media");
    }

    #[test]
    fn delete_also_removes_the_selection_whole() {
        let mut field = Field::with_mask_selected("/srv/media/*.*");
        field.handle(&key(KeyCode::Delete));
        assert_eq!(field.text(), "/srv/media/");
    }

    #[test]
    fn a_modified_key_never_types_into_the_field() {
        let mut field = Field::with_text("abc");
        assert!(!field.handle(&DialogKey::raw(KeyPress::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        ))));
        assert_eq!(field.text(), "abc");
    }

    #[test]
    fn a_bound_editing_action_reaches_the_field() {
        let mut field = Field::with_text("one two");
        let k = DialogKey {
            press: KeyPress::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            action: Some(Action::KillWord),
        };
        assert!(field.handle(&k));
        assert_eq!(field.text(), "one ");
    }

    #[test]
    fn a_multibyte_selection_is_sliced_on_characters_and_not_bytes() {
        // The caret is a character index; a mask over a CJK path is where a
        // byte slice would panic.
        let mut field = Field::with_mask_selected("/srv/媒体/報告*.*");
        assert_eq!(field.selected_text(), "報告*.*");
        typed(&mut field, "*.bak");
        assert_eq!(field.text(), "/srv/媒体/*.bak");
    }

    #[test]
    fn the_preselected_mask_is_visibly_highlighted_and_only_the_mask() {
        // preselected. If it is not *shown* as selected, the
        // first keystroke silently eats three characters.
        use crate::config::{ColorDepth, Theme};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let field = Field::with_mask_selected("/srv/media/*.*");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        let backend = TestBackend::new(20, 1);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| field.render(f, f.area(), &style))
            .expect("draw");
        let buf = terminal.backend().buffer().clone();

        let reversed: String = (0..20u16)
            .filter_map(|x| buf.cell((x, 0)))
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert_eq!(reversed, "*.*", "exactly the mask, and nothing else");
    }

    #[test]
    fn select_clamps_rather_than_panicking() {
        let mut field = Field::with_text("abc");
        field.select(99, 200);
        assert_eq!(field.selection(), None);
        field.select(2, 1);
        assert_eq!(field.selection(), None, "an inverted range selects nothing");
        field.select(0, 99);
        assert_eq!(field.selected_text(), "abc");
    }
}
