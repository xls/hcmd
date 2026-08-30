//! A message the user has to acknowledge (`DialogId::Message`).
//!
//! The simplest dialog there is, and the one every other feature falls back to:
//! the end-of-batch summary the design asks for, a warning that would be
//! lost in the status line, and anything a status line cannot hold because it
//! runs to more than one line.
//!
//! One button. `Esc` and `Enter` both close it, because a message has nothing
//! to say no to.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, draw_buttons, draw_text};
use crate::input::DialogId;

/// A modal message box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDialog {
    title: String,
    lines: Vec<String>,
    button: String,
    /// Which question this message is the answer to.
    ///
    /// [`DialogId::Message`] for the ordinary case. It is a field rather than
    /// a constant because the changed-key refusal has to be
    /// distinguishable from every other message on the stack: it is the one
    /// that aborts a connect attempt, and `dialog_answered` routes on the id.
    ///
    id: DialogId,
}

impl MessageDialog {
    /// A message box with an `OK` button.
    pub fn new(title: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            id: DialogId::Message,
            title: title.into(),
            lines,
            button: "OK".to_string(),
        }
    }

    /// A one-line message box.
    pub fn line(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(title, vec![text.into()])
    }

    /// Answer under a different [`DialogId`] than [`DialogId::Message`].
    ///
    /// Used by the changed-host-key refusal, which is a message
    /// with nothing to accept and still has to be told apart from the rest.
    pub fn with_id(mut self, id: DialogId) -> Self {
        self.id = id;
        self
    }

    /// Rename the button.
    #[must_use]
    pub fn with_button(mut self, label: impl Into<String>) -> Self {
        self.button = label.into();
        self
    }

    /// The lines being shown, for tests and for the caller that built it.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }
}

impl Dialog for MessageDialog {
    fn id(&self) -> DialogId {
        self.id
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = self
            .lines
            .iter()
            .map(|l| crate::ui::text::width(l))
            .chain(std::iter::once(crate::ui::text::width(&self.title)))
            .max()
            .unwrap_or(0);
        // Borders, a column of padding each side, and room for the title's
        // own spaces.
        let w = u16::try_from(widest.saturating_add(6)).unwrap_or(u16::MAX);
        let rows = u16::try_from(self.lines.len()).unwrap_or(1);
        // Text rows, a blank line, the button row, two borders.
        (w, rows.saturating_add(4))
    }

    /// None, and that is a decision rather than an omission.
    ///
    ///
    /// There is one button, its label is the caller's, and `Enter` and `Esc`
    /// both press it - so there is no control to jump to and no `const` table
    /// that could name a letter of a string it has never seen.
    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if key.is_cancel() || key.is_accept() {
            // A message has one answer, so `Esc` and `Enter` agree.
            return DialogOutcome::Accept(DialogResult::None);
        }
        DialogOutcome::Ignored
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        let body = style.body();
        let mut y = area.y;
        for line in &self.lines {
            if y >= area.bottom().saturating_sub(1) {
                break;
            }
            draw_text(
                f,
                Rect::new(area.x, y, area.width, 1),
                line,
                body,
                style.ascii,
            );
            y = y.saturating_add(1);
        }
        if area.height >= 1 {
            let row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
            draw_buttons(f, row, &[self.button.as_str()], 0, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyCode, KeyPress};

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    #[test]
    fn both_esc_and_enter_close_a_message() {
        let mut d = MessageDialog::line("Done", "copied 3 files");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Accept(DialogResult::None)
        ));
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Accept(DialogResult::None)
        ));
    }

    #[test]
    fn anything_else_is_swallowed_rather_than_passed_on() {
        let mut d = MessageDialog::line("Done", "ok");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Char('x'))),
            DialogOutcome::Ignored
        ));
    }

    #[test]
    fn the_size_hint_grows_with_the_content() {
        let short = MessageDialog::line("t", "hi").size_hint();
        let long = MessageDialog::new(
            "t",
            vec!["a much longer line than the other one".to_string()],
        )
        .size_hint();
        assert!(long.0 > short.0);
        assert_eq!(short.1, 5, "one text row, a gap, a button row, two borders");
    }

    #[test]
    fn a_message_declares_that_it_has_no_mnemonics() {
        // one button, a caller-supplied label,
        // and `Enter` and `Esc` both press it - so there is no control for
        // `Alt`+letter to jump to. The empty answer is the decision, and
        // `Dialog::mnemonic_letters` being required is what makes saying so
        // compulsory rather than optional (I11).
        let d = MessageDialog::line("Done", "copied 3 files");
        assert!(d.mnemonic_letters().is_empty());

        // And an `Alt`+letter is swallowed rather than leaking to the panel
        // underneath.
        let mut d = d;
        assert!(matches!(
            d.handle_key(&DialogKey::raw(KeyPress::new(
                KeyCode::Char('o'),
                crate::input::KeyModifiers::ALT
            ))),
            DialogOutcome::Ignored
        ));
    }
}
