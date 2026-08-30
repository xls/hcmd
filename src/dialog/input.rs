//! A one-line prompt with `OK` and `Cancel` (`DialogId::Mkdir`,
//! `DialogId::SelectMask`, `DialogId::UnselectMask`).
//!
//! Three v0.2 features are exactly this dialog with a different label:
//!
//! * **`F7`** - the new directory name.
//! * **`+`** - "mark by wildcard - opens a mask prompt, default `*`".
//!
//! * **`-`** - unmark by wildcard, "the last mask is remembered per session and
//!   offered as the default on the next open".
//!
//! # The field is a [`CommandLine`]
//!
//! Not a `String`. `CommandLine` already holds the invariant that matters -
//! the caret is a **character** index and can never be used to slice a `String`
//! at a non-boundary - along with word kill, line
//! kill, `Home`/`End`, and a display-width caret column that a CJK filename
//! does not break. Re-implementing a line editor for a prompt is how the
//! second, subtly different one gets written.
//!
//! The history is unused here and deliberately so: a per-prompt history is
//! the `+ F8` button, which belongs to the copy dialog rather than
//! to this primitive.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    draw_mnemonic_buttons, draw_text,
};
use crate::input::{Action, CommandLine, DialogId, KeyCode};

/// Which control has focus.
const FIELD: usize = 0;
/// The `OK` button.
const OK: usize = 1;
/// The `Cancel` button.
const CANCEL: usize = 2;

/// the `Alt` mnemonics for this dialog.
///
/// The field has none, and that is a decision rather than an omission: its
/// label is the caller's - `Path:`, `New directory name:`, `Save search as:` -
/// and a `const` table cannot name a letter of a string it has never seen.
/// The dialog opens with the field focused and
/// `Tab` is the way back to it, so nothing is out of reach.
///
/// `o` and `n` are the program-wide `OK` and
/// `Cancel`.
pub const MNEMONICS: &[(usize, char)] = &[(OK, 'o'), (CANCEL, 'n')];

/// A modal one-line prompt.
pub struct InputDialog {
    id: DialogId,
    title: String,
    label: String,
    line: CommandLine,
    ring: FocusRing,
    allow_empty: bool,
}

impl InputDialog {
    /// A prompt with a label and an initial value.
    ///
    /// The caret starts at the end of the initial value, which is what makes
    /// `-` offering the last mask a one-keystroke confirmation.
    pub fn new(
        id: DialogId,
        title: impl Into<String>,
        label: impl Into<String>,
        initial: impl Into<String>,
    ) -> Self {
        let mut line = CommandLine::new();
        line.set_text(initial);
        line.move_end();
        Self {
            id,
            title: title.into(),
            label: label.into(),
            line,
            ring: FocusRing::new(3),
            allow_empty: false,
        }
    }

    /// Accept an empty answer. Off by default: an empty directory name or an
    /// empty mask is a mistake, and refusing it in the dialog is better than
    /// failing the job.
    #[must_use]
    pub const fn allowing_empty(mut self) -> Self {
        self.allow_empty = true;
        self
    }

    /// What has been typed.
    pub fn text(&self) -> &str {
        self.line.text()
    }

    /// The caret, as a character index.
    pub fn caret(&self) -> usize {
        self.line.caret()
    }

    /// Which control has focus.
    pub const fn focused(&self) -> usize {
        self.ring.index()
    }

    /// True when the current text may be accepted.
    pub fn is_acceptable(&self) -> bool {
        self.allow_empty || !self.line.text().trim().is_empty()
    }

    /// The answer, or `None` when the field is empty and empties are refused.
    fn accept(&self) -> Option<DialogOutcome> {
        self.is_acceptable()
            .then(|| DialogOutcome::Accept(DialogResult::Text(self.line.text().to_string())))
    }
}

impl Accelerated for InputDialog {
    /// Ring indices rather than an enum: three controls is under
    /// the five-control floor, and `usize` is not one
    /// of this crate's enums, so a `_` arm here is not a wildcard on one.
    type Control = usize;

    fn mnemonics(&self) -> &'static [(usize, char)] {
        MNEMONICS
    }

    fn accel(&self, control: usize) -> Accel<usize> {
        match control {
            OK | CANCEL => Accel::Press,
            _ => Accel::Focus,
        }
    }

    fn focus_control(&mut self, control: usize) {
        self.ring.set(control);
    }

    /// Nothing here is a checkbox, so the "never turns anything off"
    /// has nothing to switch on.
    fn switch_on(&mut self, _control: usize) {}

    fn press(&mut self, control: usize) -> DialogOutcome {
        match control {
            // The same refusal `Enter` gets: an empty answer that is not
            // allowed leaves the dialog open rather than failing the job.
            OK => self.accept().unwrap_or(DialogOutcome::Consumed),
            CANCEL => DialogOutcome::Cancel,
            _ => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for InputDialog {
    fn id(&self) -> DialogId {
        self.id
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = crate::ui::text::width(&self.label)
            .max(crate::ui::text::width(&self.title))
            .max(crate::ui::text::width(self.line.text()));
        // A prompt narrower than this is not worth drawing; wider than the
        // screen is clamped by `centred`.
        let w = u16::try_from(widest.saturating_add(8)).unwrap_or(u16::MAX);
        // Label, field, gap, buttons, two borders.
        (w.clamp(36, 76), 6)
    }

    /// `Alt+O` and `Alt+N`, and nothing for the caller-supplied label.
    ///
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action` or reaches the
        // field.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            // `Enter` on Cancel cancels; anywhere else it accepts, so the
            // common case is type-and-Enter with no Tab at all.
            if self.ring.is(CANCEL) {
                return DialogOutcome::Cancel;
            }
            return self.accept().unwrap_or(DialogOutcome::Consumed);
        }

        // Editing keys only reach the field. On a button, Left/Right walk the
        // buttons instead, which is what a button row is for.
        if !self.ring.is(FIELD) {
            return match key.press.code {
                KeyCode::Left => {
                    self.ring.prev();
                    DialogOutcome::Consumed
                }
                KeyCode::Right => {
                    self.ring.next();
                    DialogOutcome::Consumed
                }
                KeyCode::Up => {
                    self.ring.set(FIELD);
                    DialogOutcome::Consumed
                }
                _ => DialogOutcome::Ignored,
            };
        }

        // A binding resolved in the `dialog` context wins over the raw key
        // (the design steps 1-3), so a user who rebound `kill_line` gets it.
        match key.action {
            Some(Action::KillLine) => {
                self.line.kill_line();
                return DialogOutcome::Consumed;
            }
            Some(Action::KillWord) => {
                self.line.kill_word();
                return DialogOutcome::Consumed;
            }
            Some(Action::KillToEnd) => {
                self.line.kill_to_end();
                return DialogOutcome::Consumed;
            }
            _ => {}
        }

        match key.press.code {
            KeyCode::Left => self.line.move_left(),
            KeyCode::Right => self.line.move_right(),
            KeyCode::Home => self.line.move_home(),
            KeyCode::End => self.line.move_end(),
            KeyCode::Backspace => {
                self.line.backspace();
            }
            KeyCode::Delete => {
                self.line.delete();
            }
            KeyCode::Down => self.ring.set(OK),
            KeyCode::Insert => self.line.overwrite = !self.line.overwrite,
            _ => match key.text() {
                Some(c) => self.line.insert_char(c),
                None => return DialogOutcome::Ignored,
            },
        }
        DialogOutcome::Consumed
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        let body = style.body();
        if area.height == 0 || area.width == 0 {
            return;
        }
        draw_text(
            f,
            Rect::new(area.x, area.y, area.width, 1),
            &self.label,
            body,
            style.ascii,
        );

        if area.height >= 2 {
            let field = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
            let width = usize::from(field.width);
            // Keep the caret on screen, leaving its own cell free.
            let caret_col = self.line.display_width_to_caret();
            let scroll = crate::ui::text::caret_window(caret_col, width);
            let text = crate::ui::text::slice_columns(self.line.text(), scroll, width);
            let padded = crate::ui::text::fit_left(
                &text,
                width,
                crate::ui::text::Crop::End,
                if style.ascii { "..." } else { "\u{2026}" },
            );
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(padded, style.input())))
                    .style(style.input()),
                field,
            );
        }

        if area.height >= 3 {
            let row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
            let focus = match self.ring.index() {
                OK => 0,
                CANCEL => 1,
                // The field has focus, so neither button is highlighted.
                _ => usize::MAX,
            };
            draw_mnemonic_buttons(
                f,
                row,
                &[("OK", Some('o')), ("Cancel", Some('n'))],
                focus,
                style,
            );
        }

        // Silence an unused-import warning in builds where nothing else in
        // this function needs `Style`.
        let _: Style = body;
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if !self.ring.is(FIELD) || area.width == 0 || area.height < 2 {
            return None;
        }
        let caret_col = self.line.display_width_to_caret();
        let col = caret_col.saturating_sub(crate::ui::text::caret_window(
            caret_col,
            usize::from(area.width),
        ));
        let x = area
            .x
            .saturating_add(u16::try_from(col).unwrap_or(u16::MAX))
            .min(area.right().saturating_sub(1));
        Some((x, area.y.saturating_add(1)))
    }
}

impl std::fmt::Debug for InputDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputDialog")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("text", &self.line.text())
            .field("focused", &self.ring.index())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::{KeyModifiers, KeyPress};
    use ratatui::style::Modifier;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    /// `Alt`+letter, the keystroke the design is about.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    /// Every character drawn with [`Modifier::UNDERLINED`], in screen order.
    fn underlined(d: &InputDialog, w: u16, h: u16) -> Vec<char> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &style);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let Some(cell) = buffer.cell((x, y)) else {
                    continue;
                };
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    out.extend(cell.symbol().chars());
                }
            }
        }
        out
    }

    fn prompt() -> InputDialog {
        InputDialog::new(DialogId::Mkdir, "Create directory", "New name:", "")
    }

    fn typed(d: &mut InputDialog, text: &str) {
        for c in text.chars() {
            d.handle_key(&key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_then_enter_is_the_whole_interaction() {
        let mut d = prompt();
        typed(&mut d, "photos");
        assert_eq!(d.text(), "photos");
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Text(text)) => assert_eq!(text, "photos"),
            other => panic!("expected an accepted name, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_answer_is_refused_unless_it_is_allowed() {
        let mut d = prompt();
        assert!(!d.is_acceptable());
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));

        let mut d = prompt().allowing_empty();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Accept(DialogResult::Text(_))
        ));
    }

    #[test]
    fn esc_cancels_from_anywhere() {
        let mut d = prompt();
        typed(&mut d, "half-typed");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn tab_walks_field_ok_cancel_and_wraps() {
        let mut d = prompt();
        assert_eq!(d.focused(), FIELD);
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), OK);
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), CANCEL);
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), FIELD, "and it wraps");

        d.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(d.focused(), CANCEL, "Shift+Tab goes the other way");
    }

    #[test]
    fn enter_on_cancel_cancels() {
        let mut d = prompt();
        typed(&mut d, "x");
        d.handle_key(&key(KeyCode::Tab));
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), CANCEL);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn arrows_edit_in_the_field_and_walk_buttons_outside_it() {
        let mut d = prompt();
        typed(&mut d, "abc");
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(d.caret(), 2, "Left is an edit while the field has focus");

        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), OK);
        d.handle_key(&key(KeyCode::Right));
        assert_eq!(d.focused(), CANCEL, "and a button move outside it");
        assert_eq!(d.caret(), 2, "the caret did not move");
    }

    #[test]
    fn a_modified_key_never_types_into_the_field() {
        let mut d = prompt();
        d.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(d.text(), "", "Ctrl+K is not the letter k");
    }

    #[test]
    fn the_initial_value_is_offered_with_the_caret_at_its_end() {
        // `-` offers the last mask as the default.
        let d = InputDialog::new(DialogId::UnselectMask, "Unselect", "Mask:", "*.bak");
        assert_eq!(d.text(), "*.bak");
        assert_eq!(d.caret(), 5);
    }

    #[test]
    fn a_bound_editing_action_reaches_the_field() {
        let mut d = prompt();
        typed(&mut d, "one two");
        let k = DialogKey {
            press: KeyPress::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            action: Some(Action::KillWord),
        };
        d.handle_key(&k);
        assert_eq!(d.text(), "one ");
    }

    #[test]
    fn the_two_buttons_are_reachable_by_their_alt_letter() {
        // the design on a button: focus it and press it. `Alt+O` is `OK` and
        // `Alt+N` is `Cancel` in every dialog that has them.
        //
        let mut d = prompt();
        typed(&mut d, "photos");
        match d.handle_key(&alt('o')) {
            DialogOutcome::Accept(DialogResult::Text(text)) => assert_eq!(text, "photos"),
            other => panic!("Alt+O pressed OK, got {other:?}"),
        }

        let mut d = prompt();
        typed(&mut d, "photos");
        assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));
    }

    #[test]
    fn alt_o_gets_the_same_refusal_enter_gets() {
        // A greyed or refused control answers the same way whichever route
        // reached it: an empty answer that is not
        // allowed leaves the dialog open.
        let mut d = prompt();
        assert!(matches!(d.handle_key(&alt('o')), DialogOutcome::Consumed));
        assert_eq!(d.focused(), OK, "and focus moved to the button first");
        assert_eq!(d.text(), "");
    }

    #[test]
    fn a_mnemonic_never_types_and_never_edits() {
        // the design I8. `DialogKey::text` is `None` under `ALT`,
        // so no letter can reach the field - claimed by the table or not.
        let mut d = prompt().allowing_empty();
        typed(&mut d, "abc");
        d.handle_key(&key(KeyCode::Left));
        let (text, caret) = (d.text().to_string(), d.caret());
        for letter in ['q', 'z', 'k'] {
            d.handle_key(&alt(letter));
            assert_eq!(d.text(), text, "Alt+{letter} typed something");
            assert_eq!(d.caret(), caret, "Alt+{letter} moved the caret");
        }
    }

    #[test]
    fn mnemonics_are_unique_within_this_dialog() {
        // a duplicate is a bug rather than a first-one-wins rule.
        let mut seen: Vec<char> = Vec::new();
        for (control, letter) in MNEMONICS {
            assert!(letter.is_ascii_lowercase(), "{control}: stored folded");
            assert!(!seen.contains(letter), "{control}: Alt+{letter} is taken");
            seen.push(*letter);
        }
        assert_eq!(prompt().mnemonic_letters(), vec!['o', 'n']);
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // the underline, read off the buffer. The prompt's own
        // label carries none, which is a `const`
        // table cannot name a letter of a caller-supplied string.
        let d = InputDialog::new(
            DialogId::Mkdir,
            "Create directory",
            "New directory name:",
            "",
        );
        let mut got: Vec<char> = underlined(&d, 60, 12)
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec!['n', 'o']);
    }
}
