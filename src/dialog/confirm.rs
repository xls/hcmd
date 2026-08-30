//! A yes/no confirmation (`DialogId::ConfirmQuit`, `DialogId::ConfirmDelete`).
//!
//! Three things in v0.2 need one, and the design is specific about each:
//!
//! * **`Shift+F8`** - "unlinks, with a confirmation naming the count".
//!   Naming the count is why the message is a list of lines
//!   rather than a fixed string.
//! * **`F8`** on a directory - "Directories are recursed with a single
//!   confirmation, not one per entry".
//! * **`ui.confirm_exit`** - the quit prompt.
//!
//! The **default button is the safe one**. A confirmation that opens with
//! `Yes` focused is a confirmation that gets dismissed by the `Enter` the user
//! was already pressing, which is exactly the accident it exists to prevent;
//! [`ConfirmDialog::defaulting_to_yes`] is there for the prompts where that is
//! genuinely wanted.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::{
    Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing, draw_mnemonic_buttons,
    draw_text,
};
use crate::input::{DialogId, KeyCode};

/// The affirmative button, and index 0 of the ring.
const YES: usize = 0;
/// The negative button, and index 1.
const NO: usize = 1;

/// The letter a button label offers, or `None` when it has no free letter.
///
/// The first ASCII letter at a word start that is not `taken`, and failing that
/// the first ASCII letter anywhere that is not `taken` - the same rule
/// [`super::split_mnemonic`] underlines by, so
/// the letter this picks is always the letter that gets the underline.
///
/// A word start is a character at index 0 or preceded by a character that is
/// not ASCII alphanumeric, exactly as in the design.
pub fn button_letter(label: &str, taken: Option<char>) -> Option<char> {
    let mut anywhere: Option<char> = None;
    let mut prev: Option<char> = None;
    for ch in label.chars() {
        let letter = ch.to_ascii_lowercase();
        let free = ch.is_ascii_alphabetic() && taken != Some(letter);
        if free {
            if anywhere.is_none() {
                anywhere = Some(letter);
            }
            if prev.is_none_or(|p| !p.is_ascii_alphanumeric()) {
                return Some(letter);
            }
        }
        prev = Some(ch);
    }
    anywhere
}

/// The negative button's letter, which prefers `n` (the
/// program-wide reservation).
///
/// `Cancel` and `No` are therefore both `Alt+N`, so a user who learns one
/// dialog's negative has learned all of them. Resolved before the affirmative
/// so the affirmative is the one that has to give way on a clash.
fn negative_letter(label: &str) -> Option<char> {
    if label.chars().any(|c| c.eq_ignore_ascii_case(&'n')) {
        return Some('n');
    }
    button_letter(label, None)
}

/// A modal yes/no question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmDialog {
    id: DialogId,
    title: String,
    lines: Vec<String>,
    yes: String,
    no: String,
    /// The affirmative's `Alt` mnemonic. Computed once, at
    /// construction, because the label is the caller's rather than a constant.
    yes_letter: Option<char>,
    /// The negative's, resolved first (see [`negative_letter`]).
    no_letter: Option<char>,
    ring: FocusRing,
}

impl ConfirmDialog {
    /// A confirmation with `Yes` / `No` buttons, focused on `No`.
    pub fn new(id: DialogId, title: impl Into<String>, lines: Vec<String>) -> Self {
        let mut ring = FocusRing::new(2);
        // Index 1 is `No`: the safe default (see the module docs).
        ring.set(NO);
        let no_letter = negative_letter("No");
        Self {
            id,
            title: title.into(),
            lines,
            yes: "Yes".to_string(),
            no: "No".to_string(),
            yes_letter: button_letter("Yes", no_letter),
            no_letter,
            ring,
        }
    }

    /// Rename the two buttons - `Delete` / `Cancel` reads better than
    /// `Yes` / `No` on a destructive prompt.
    #[must_use]
    pub fn with_buttons(mut self, yes: impl Into<String>, no: impl Into<String>) -> Self {
        self.yes = yes.into();
        self.no = no.into();
        // Both, not just the one that changed: the affirmative's letter is
        // resolved against the negative's, so renaming either can move it.
        self.no_letter = negative_letter(&self.no);
        self.yes_letter = button_letter(&self.yes, self.no_letter);
        self
    }

    /// Its two `Alt` mnemonics: the affirmative's, then the negative's.
    ///
    pub const fn mnemonics(&self) -> (Option<char>, Option<char>) {
        (self.yes_letter, self.no_letter)
    }

    /// Open with the affirmative focused. For prompts where the answer is
    /// nearly always yes and nothing is destroyed by it.
    #[must_use]
    pub const fn defaulting_to_yes(mut self) -> Self {
        self.ring.set(YES);
        self
    }

    /// Which button has focus: 0 is yes, 1 is no.
    pub const fn focused(&self) -> usize {
        self.ring.index()
    }
}

impl Dialog for ConfirmDialog {
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
        let w = u16::try_from(widest.saturating_add(6)).unwrap_or(u16::MAX);
        let rows = u16::try_from(self.lines.len()).unwrap_or(1);
        (w.max(30), rows.saturating_add(4))
    }

    /// Its two letters, and no more.
    ///
    /// Per-instance rather than a `const` table because both labels are the
    /// caller's: `Yes`/`No`, `Delete`/`Cancel`, `Quit`/`Cancel`.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.yes_letter.into_iter().chain(self.no_letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // the mnemonics come before anything that reads
        // `key.action`, so a global `Alt` binding of the design cannot
        // pre-empt one.
        //
        // Both controls are buttons, so both letters are
        // `crate::dialog::mnemonic::Accel::Press`: focus first, then press.
        if let Some(letter) = key.mnemonic() {
            if self.yes_letter == Some(letter) {
                self.ring.set(YES);
                return DialogOutcome::Accept(DialogResult::Confirm(true));
            }
            if self.no_letter == Some(letter) {
                self.ring.set(NO);
                return DialogOutcome::Accept(DialogResult::Confirm(false));
            }
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            // `Esc` is always no, whichever button has focus.
            return DialogOutcome::Accept(DialogResult::Confirm(false));
        }
        match key.press.code {
            KeyCode::Left => {
                self.ring.prev();
                DialogOutcome::Consumed
            }
            KeyCode::Right => {
                self.ring.next();
                DialogOutcome::Consumed
            }
            // `y` and `n` answer directly, which is what anyone who has used a
            // terminal expects and what makes the dialog a single keystroke.
            KeyCode::Char('y' | 'Y') => DialogOutcome::Accept(DialogResult::Confirm(true)),
            KeyCode::Char('n' | 'N') => DialogOutcome::Accept(DialogResult::Confirm(false)),
            _ if key.is_accept() => DialogOutcome::Accept(DialogResult::Confirm(self.ring.is(0))),
            _ => DialogOutcome::Ignored,
        }
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
        let row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        draw_mnemonic_buttons(
            f,
            row,
            &[
                (self.yes.as_str(), self.yes_letter),
                (self.no.as_str(), self.no_letter),
            ],
            self.ring.index(),
            style,
        );
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
    fn underlined(d: &ConfirmDialog, w: u16, h: u16) -> Vec<char> {
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

    fn confirm() -> ConfirmDialog {
        ConfirmDialog::new(
            DialogId::ConfirmDelete,
            "Delete",
            vec!["Delete 3 files permanently?".to_string()],
        )
    }

    #[test]
    fn the_safe_button_has_focus_when_it_opens() {
        let d = confirm();
        assert_eq!(d.focused(), 1, "No is focused");
        assert_eq!(confirm().defaulting_to_yes().focused(), 0);
    }

    #[test]
    fn enter_answers_with_the_focused_button() {
        let mut d = confirm();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Accept(DialogResult::Confirm(false))
        ));

        let mut d = confirm();
        d.handle_key(&key(KeyCode::Tab));
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Accept(DialogResult::Confirm(true))
        ));
    }

    #[test]
    fn esc_is_always_no() {
        let mut d = confirm().defaulting_to_yes();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Accept(DialogResult::Confirm(false))
        ));
    }

    #[test]
    fn y_and_n_answer_directly() {
        let mut d = confirm();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Char('y'))),
            DialogOutcome::Accept(DialogResult::Confirm(true))
        ));
        let mut d = confirm();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Char('n'))),
            DialogOutcome::Accept(DialogResult::Confirm(false))
        ));
    }

    #[test]
    fn arrows_and_shift_tab_move_between_the_buttons() {
        let mut d = confirm();
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(d.focused(), 0);
        d.handle_key(&key(KeyCode::Right));
        assert_eq!(d.focused(), 1);
        d.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(d.focused(), 0);
    }

    #[test]
    fn the_negative_is_alt_n_whatever_it_is_called() {
        // the design reserves `Alt+N` for the negative program-wide, so a
        // user who learns one confirmation has learned all of them.
        // tabulates the five pairs this dialog is built with.
        for (yes, no, want_yes) in [
            ("Yes", "No", 'y'),
            ("Delete", "Cancel", 'd'),
            ("Trash", "Cancel", 't'),
            ("Quit", "Cancel", 'q'),
            ("Rewrite", "Cancel", 'r'),
        ] {
            let d = confirm().with_buttons(yes, no);
            assert_eq!(d.mnemonics(), (Some(want_yes), Some('n')), "{yes}/{no}");
        }
    }

    #[test]
    fn the_two_letters_are_never_the_same() {
        // "Mnemonics are unique within a dialog. A duplicate is
        // a bug rather than a first-one-wins rule, because the second control
        // becomes unreachable silently." With two runtime labels the only way
        // to know is to resolve them against each other, which is what
        // `button_letter`'s `taken` argument is for.
        for (yes, no) in [
            ("Yes", "No"),
            ("Delete", "Cancel"),
            ("No", "Cancel"),
            ("Now", "No"),
            ("Rename", "Cancel"),
        ] {
            let (a, b) = confirm().with_buttons(yes, no).mnemonics();
            assert!(a.is_some() && b.is_some(), "{yes}/{no}: both got a letter");
            assert_ne!(a, b, "{yes}/{no}");
        }
        // A label with no letters in it gets none rather than a wrong one.
        assert_eq!(confirm().with_buttons("42", "!").mnemonics(), (None, None));
    }

    #[test]
    fn each_letter_presses_its_own_button() {
        // the design on a button: focus it and press it. Both of these close
        // the dialog with the answer that button carries, from whichever
        // button happened to have focus.
        let mut d = confirm().with_buttons("Delete", "Cancel");
        assert!(matches!(
            d.handle_key(&alt('d')),
            DialogOutcome::Accept(DialogResult::Confirm(true))
        ));
        assert_eq!(d.focused(), YES, "and focus moved there first");

        let mut d = confirm()
            .with_buttons("Delete", "Cancel")
            .defaulting_to_yes();
        assert!(matches!(
            d.handle_key(&alt('n')),
            DialogOutcome::Accept(DialogResult::Confirm(false))
        ));
        assert_eq!(d.focused(), NO);

        // Upper case is the same keystroke: `DialogKey::mnemonic` folds it.
        let mut d = confirm();
        assert!(matches!(
            d.handle_key(&alt('Y')),
            DialogOutcome::Accept(DialogResult::Confirm(true))
        ));
    }

    #[test]
    fn an_unclaimed_alt_letter_changes_nothing() {
        // A dialog consumes all input, so the key is swallowed
        // rather than answering the question by accident.
        let mut d = confirm().with_buttons("Delete", "Cancel");
        let before = d.focused();
        assert!(matches!(
            d.handle_key(&alt('z')),
            DialogOutcome::Ignored | DialogOutcome::Consumed
        ));
        assert_eq!(d.focused(), before);
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // "The letter is shown underlined in that control's
        // label, so the whole set is readable off the screen rather than
        // memorised." Read off the rendered buffer, so a stored letter with no
        // paint behind it fails here.
        for (yes, no) in [("Yes", "No"), ("Delete", "Cancel"), ("Quit", "Cancel")] {
            let d = confirm().with_buttons(yes, no);
            let (a, b) = d.mnemonics();
            let mut want: Vec<char> = a.into_iter().chain(b).collect();
            let mut got: Vec<char> = underlined(&d, 60, 12)
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "{yes}/{no}: underlines on screen");
        }
    }
}
