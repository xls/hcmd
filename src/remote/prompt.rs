//! the password and passphrase prompt.
//!
//! > **Key file**, path per host, with a passphrase prompt if the key is
//! > encrypted. [...] **Password**, prompted at connect time and held only for
//! > the session.
//!
//! ```text
//!     +- Password for thorin@nas.local:2222 -----------------+
//!     |Password:                                             |
//!     |********                                              |
//!     |[ ] Save in the system keyring                        |
//!     |              [ OK ]   [ Cancel ]                     |
//!     +------------------------------------------------------+
//! ```
//!
//! # Why this is not [`crate::dialog::InputDialog`]
//!
//! Three differences, none of them cosmetic:
//!
//! * its buffer is a [`Secret`], which redacts its own `Debug` and overwrites
//!   itself on drop, where `InputDialog`'s is a `String`;
//! * **nothing it holds is ever drawn.** The field shows one `*` per
//!   character and no character of the buffer reaches the screen buffer, which
//!   is what the design tests;
//! * its answer is [`DialogResult::Secret`] and never
//!   [`DialogResult::Text`]. A secret travelling in `Text` would be one
//!   `Debug` away from a log line, because `DialogResult` derives `Debug`.
//!
//! # It does not retain
//!
//! Accepting **moves** the buffer into the answer and leaves an empty one
//! behind; cancelling drops it. Either way the dialog owns no secret once it
//! is finished with, and [`Secret`]'s `Drop` overwrites the bytes.
//!
//! # The keyring checkbox
//!
//! Offered only where the host's `auth` is `keyring` **and** a store reports
//! itself available. Where the
//! host asked for one and there is none, the dialog carries
//! [`crate::remote::keyring::unavailable_message`] on its own row instead,
//! which is the "say so in the dialog and fall back to prompting
//! every time - do not silently write the password to disk".
//!
//! A **passphrase** never offers it: the opt-in is about the
//! password for a host, and offering to store a key's passphrase would be
//! inventing a policy the design does not have.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    SecretAnswer, draw_mnemonic, draw_mnemonic_buttons, draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::remote::auth::SecretKind;
use crate::remote::secret::Secret;
use crate::ui::dialog::{checkbox, row};

/// One focusable control of the prompt, in `Tab` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptControl {
    /// The masked field. It has focus when the dialog opens.
    Field,
    /// "Save in the system keyring".
    Remember,
    /// Send it.
    Ok,
    /// Give up, and connect nothing.
    Cancel,
}

/// the letters.
///
/// **The field has no letter**, which is the rule
/// for `InputDialog`'s field: it has focus when the dialog opens and there is
/// nowhere else to be.
pub const PROMPT_MNEMONICS: &[(PromptControl, char)] = &[
    (PromptControl::Remember, 's'),
    (PromptControl::Ok, 'o'),
    (PromptControl::Cancel, 'n'),
];

/// The checkbox's label, and the string the underline is searched
/// in.
const REMEMBER_LABEL: &str = "Save in the system keyring";

/// The controls when the keyring checkbox is offered.
const CONTROLS_WITH_REMEMBER: &[PromptControl] = &[
    PromptControl::Field,
    PromptControl::Remember,
    PromptControl::Ok,
    PromptControl::Cancel,
];

/// The controls when it is not. A control that is not on the screen is not in
/// the `Tab` ring either, so `Tab` never lands on nothing.
const CONTROLS: &[PromptControl] = &[
    PromptControl::Field,
    PromptControl::Ok,
    PromptControl::Cancel,
];

/// The narrowest this dialog asks to be.
const MIN_WIDTH: u16 = 46;

/// the password and passphrase prompt.
pub struct SecretDialog {
    kind: SecretKind,
    /// The buffer. Never drawn, never `Display`ed, never in `Debug`.
    secret: Secret,
    /// How many characters are in it.
    ///
    /// Kept beside the buffer rather than measured from it: [`Secret`]'s
    /// surface is bytes and a row of `*` is
    /// characters, and counting characters would mean exposing the bytes to
    /// count them - a fifth call site of `Secret::expose` for a cosmetic
    /// reason (S5). `push` and `pop` report whether they did anything, which
    /// is exactly what keeps this in step.
    typed: usize,
    offer_keyring: bool,
    remember: bool,
    /// A row of explanation under the field: the "say so in the
    /// dialog" when a host asked for the keyring and there is none.
    note: Option<String>,
    controls: &'static [PromptControl],
    ring: FocusRing,
}

impl SecretDialog {
    /// A prompt for `kind`.
    ///
    /// `offer_keyring` is true only when the host opted in **and** a keyring is
    /// available, which is what makes the checkbox [`Accel::Absent`]
    /// everywhere else.
    pub fn new(kind: SecretKind, offer_keyring: bool) -> Self {
        // A passphrase is never storable, whatever the caller passes: the opt-in
        // is about a host's password, and this is the one place that can enforce
        // it for every caller.
        let offer_keyring = offer_keyring && matches!(kind, SecretKind::Password { .. });
        let controls = if offer_keyring {
            CONTROLS_WITH_REMEMBER
        } else {
            CONTROLS
        };
        Self {
            kind,
            secret: Secret::new(),
            typed: 0,
            offer_keyring,
            remember: false,
            note: None,
            controls,
            // Index 0 is the field: it has focus when the dialog opens.
            ring: FocusRing::new(controls.len()),
        }
    }

    /// Add the row the design asks for when there is no keyring to offer.
    ///
    /// The caller passes [`crate::remote::keyring::unavailable_message`]; it is
    /// not read here, because a dialog does not probe anything.
    ///
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Which question this is.
    pub const fn kind(&self) -> &SecretKind {
        &self.kind
    }

    /// Whether the keyring checkbox is on the screen at all.
    pub const fn offers_keyring(&self) -> bool {
        self.offer_keyring
    }

    /// Whether the checkbox is ticked.
    pub const fn remembers(&self) -> bool {
        self.remember
    }

    /// How many characters have been typed.
    ///
    /// The count and never the content: it is what the row of `*` is drawn
    /// from, and it is all a test can ask without holding a secret itself.
    pub const fn typed(&self) -> usize {
        self.typed
    }

    /// The note row, when there is one.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Which control has focus.
    pub fn focused(&self) -> PromptControl {
        self.controls
            .get(self.ring.index())
            .copied()
            .unwrap_or(PromptControl::Field)
    }

    /// The label above the field: `Password:` or `Passphrase:`.
    ///
    /// Short, because the border title already carries *which* password
    /// ("Password for thorin@nas.local:2222").
    const fn prompt_label(&self) -> &'static str {
        match self.kind {
            SecretKind::Passphrase { .. } => "Passphrase:",
            SecretKind::Password { .. } => "Password:",
        }
    }

    /// Which row of the interior each part is drawn on.
    ///
    /// One function, so [`Dialog::render`] and [`Dialog::cursor`] cannot
    /// disagree about where the field is.
    const fn field_row() -> u16 {
        1
    }

    /// The row the note is on, when there is one.
    fn note_row(&self) -> Option<u16> {
        self.note.as_ref().map(|_| Self::field_row() + 1)
    }

    /// The row the checkbox is on, when it is offered.
    fn remember_row(&self) -> Option<u16> {
        if !self.offer_keyring {
            return None;
        }
        Some(match self.note_row() {
            Some(note) => note.saturating_add(1),
            None => Self::field_row().saturating_add(1),
        })
    }

    /// Hand the secret over and keep none of it.
    fn accept(&mut self) -> DialogOutcome {
        // Swapped out and not cloned: the dialog is left holding an empty
        // buffer, and the one it had travels into the answer. A `swap` and not
        // a `take`, because [`Secret`] is not required to implement `Default`
        // and a second copy is the one thing this must not make.
        let mut secret = Secret::new();
        std::mem::swap(&mut secret, &mut self.secret);
        self.typed = 0;
        DialogOutcome::Accept(DialogResult::Secret(Box::new(SecretAnswer {
            secret,
            remember: self.remember && self.offer_keyring,
        })))
    }
}

impl Accelerated for SecretDialog {
    type Control = PromptControl;

    fn mnemonics(&self) -> &'static [(PromptControl, char)] {
        PROMPT_MNEMONICS
    }

    fn accel(&self, control: PromptControl) -> Accel<PromptControl> {
        match control {
            PromptControl::Field => Accel::Focus,
            PromptControl::Remember => {
                if self.offer_keyring {
                    Accel::Check
                } else {
                    Accel::Absent
                }
            }
            PromptControl::Ok | PromptControl::Cancel => Accel::Press,
        }
    }

    fn focus_control(&mut self, control: PromptControl) {
        if let Some(at) = self.controls.iter().position(|c| *c == control) {
            self.ring.set(at);
        }
    }

    /// **on**, never a toggle.
    fn switch_on(&mut self, control: PromptControl) {
        match control {
            PromptControl::Remember => self.remember = self.offer_keyring,
            PromptControl::Field | PromptControl::Ok | PromptControl::Cancel => {}
        }
    }

    fn press(&mut self, control: PromptControl) -> DialogOutcome {
        match control {
            PromptControl::Ok => self.accept(),
            PromptControl::Cancel => DialogOutcome::Cancel,
            PromptControl::Field | PromptControl::Remember => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for SecretDialog {
    fn id(&self) -> DialogId {
        DialogId::RemoteSecret
    }

    fn title(&self) -> String {
        self.kind.title()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = crate::ui::text::width(&self.title())
            .max(self.note.as_deref().map_or(0, crate::ui::text::width))
            .max(crate::ui::text::width(REMEMBER_LABEL).saturating_add(4));
        let w = u16::try_from(widest.saturating_add(6)).unwrap_or(u16::MAX);
        // Label, field, an optional note, an optional checkbox, the button
        // row, two borders.
        let rows = 3u16
            .saturating_add(u16::from(self.note.is_some()))
            .saturating_add(u16::from(self.offer_keyring));
        (w.max(MIN_WIDTH), rows.saturating_add(2))
    }

    /// Two letters, or three where the keyring checkbox is offered.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics()
            .iter()
            .filter(|(control, _)| match self.accel(*control) {
                Accel::Absent => false,
                Accel::Focus | Accel::Check | Accel::Gate(_) | Accel::Press => true,
            })
            .map(|(_, letter)| *letter)
            .collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action` and before the
        // field can see the key. It is also what
        // keeps `Alt`+letter out of the buffer, together with `DialogKey::text`
        // answering `None` whenever `Alt` is held.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            // The buffer goes with the dialog, and `Secret::drop` overwrites it.
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return match self.focused() {
                PromptControl::Cancel => DialogOutcome::Cancel,
                PromptControl::Field | PromptControl::Remember | PromptControl::Ok => self.accept(),
            };
        }
        if self.focused() == PromptControl::Remember && key.press.code == KeyCode::Char(' ') {
            self.remember = !self.remember;
            return DialogOutcome::Consumed;
        }
        if self.focused() == PromptControl::Field {
            match key.press.code {
                KeyCode::Backspace => {
                    if self.secret.pop() {
                        self.typed = self.typed.saturating_sub(1);
                    }
                    return DialogOutcome::Consumed;
                }
                // There is no caret to move and no selection to make: the
                // field is write-only, so every editing key that is not
                // backspace would need something drawn to act on.
                KeyCode::Delete | KeyCode::Home | KeyCode::End => {
                    return DialogOutcome::Consumed;
                }
                _ => {}
            }
            if let Some(ch) = key.text() {
                // `false` at `Secret::MAX`: the
                // buffer is refused rather than reallocated, so a paste
                // accident cannot leave a copy behind.
                if self.secret.push(ch) {
                    self.typed = self.typed.saturating_add(1);
                }
                return DialogOutcome::Consumed;
            }
        }
        match key.press.code {
            KeyCode::Up | KeyCode::Left => {
                self.ring.prev();
                DialogOutcome::Consumed
            }
            KeyCode::Down | KeyCode::Right => {
                self.ring.next();
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    /// Draws one `*` per character and **no character of the buffer**.
    ///
    ///
    /// A mark per character rather than a blank field: a field that shows
    /// nothing at all cannot tell the user that a keystroke registered, and
    /// the length is on the screen for as long as the dialog is and nowhere
    /// else. It is never in a log, a `Debug`, or the status line.
    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focused();
        if let Some(rect) = row(area, 0) {
            draw_text(f, rect, self.prompt_label(), style.body(), style.ascii);
        }
        if let Some(rect) = row(area, Self::field_row()) {
            let width = usize::from(rect.width);
            // Capped to the field, so a long passphrase cannot draw off the
            // edge and cannot say how much longer than the field it is.
            let marks = "*".repeat(self.typed.min(width));
            let padded = crate::ui::text::fit_left(
                &marks,
                width,
                crate::ui::text::Crop::End,
                crate::ui::dialog::ellipsis(style.ascii),
            );
            draw_text(f, rect, &padded, style.input(), style.ascii);
        }
        if let Some(rect) = self.note_row().and_then(|r| row(area, r))
            && let Some(note) = self.note.as_deref()
        {
            draw_text(f, rect, note, style.body(), style.ascii);
        }
        if let Some(rect) = self.remember_row().and_then(|r| row(area, r)) {
            let text = checkbox(REMEMBER_LABEL, self.remember, style.ascii);
            draw_mnemonic(
                f,
                rect,
                &text,
                's',
                style.button(focused == PromptControl::Remember),
                style.ascii,
            );
        }
        let last = area.height.saturating_sub(1);
        if let Some(rect) = row(area, last) {
            let index = match focused {
                PromptControl::Ok => 0,
                PromptControl::Cancel => 1,
                PromptControl::Field | PromptControl::Remember => usize::MAX,
            };
            draw_mnemonic_buttons(
                f,
                rect,
                &[("OK", Some('o')), ("Cancel", Some('n'))],
                index,
                style,
            );
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if self.focused() != PromptControl::Field {
            return None;
        }
        let rect = row(area, Self::field_row())?;
        if rect.width == 0 {
            return None;
        }
        let col = u16::try_from(self.typed).unwrap_or(u16::MAX);
        let x = rect
            .x
            .saturating_add(col)
            .min(rect.right().saturating_sub(1));
        Some((x, rect.y))
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// Manual, and it prints no secret and no length (the design
/// S1.
///
/// Deriving would already be safe, because [`Secret`]'s own `Debug` redacts.
/// It is written out anyway: this is the type whose whole purpose is to hold a
/// credential, and a derived impl would start printing whatever field a later
/// edit adds.
impl std::fmt::Debug for SecretDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretDialog")
            .field("kind", &self.kind)
            .field("offer_keyring", &self.offer_keyring)
            .field("remember", &self.remember)
            .field("empty", &self.secret.is_empty())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::{KeyModifiers, KeyPress};
    use ratatui::style::Modifier;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    fn typed(d: &mut SecretDialog, text: &str) {
        for ch in text.chars() {
            d.handle_key(&key(KeyCode::Char(ch)));
        }
    }

    fn password() -> SecretKind {
        SecretKind::Password {
            authority: "sftp://thorin@nas.local:2222".to_string(),
        }
    }

    fn passphrase() -> SecretKind {
        SecretKind::Passphrase {
            key: PathBuf::from("/home/thorin/.ssh/id_ed25519"),
        }
    }

    /// The whole screen as text.
    fn screen(d: &dyn Dialog, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &style);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                if let Some(cell) = buffer.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    /// Every underlined character, in screen order.
    fn underlined(d: &dyn Dialog, w: u16, h: u16) -> Vec<char> {
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
                    // Folded, because a table is written in lower case and a
                    // label is not: `Alt+Q` underlines the `Q` of `Quick`.
                    out.extend(cell.symbol().chars().map(|c| c.to_ascii_lowercase()));
                }
            }
        }
        out
    }

    #[test]
    fn no_character_of_the_buffer_reaches_the_screen() {
        // the whole point of the type.
        let mut d = SecretDialog::new(password(), true);
        typed(&mut d, "hunter2");
        assert_eq!(d.typed(), 7);
        let drawn = screen(&d, 60, 12);
        assert!(!drawn.contains("hunter2"), "{drawn}");
        assert!(drawn.contains("*******"), "one mark per character: {drawn}");
        assert!(!drawn.contains("********"), "and not one more: {drawn}");
        // The field's own row holds marks and nothing else, which is the
        // assertion that would fail if any character of the buffer were drawn.
        let field = drawn
            .lines()
            .find(|line| line.contains('*'))
            .expect("the field row");
        assert!(
            field
                .chars()
                .all(|c| c == '*' || c == ' ' || !c.is_ascii_alphanumeric()),
            "{field:?}"
        );
    }

    #[test]
    fn the_answer_is_a_secret_and_never_text() {
        // a secret travelling in
        // `DialogResult::Text` would be one `Debug` away from a log line.
        let mut d = SecretDialog::new(password(), true);
        typed(&mut d, "hunter2");
        let DialogOutcome::Accept(result) = d.handle_key(&key(KeyCode::Enter)) else {
            panic!("Enter answers");
        };
        let DialogResult::Secret(answer) = &result else {
            panic!("the answer is a Secret, got {result:?}");
        };
        assert_eq!(answer.secret.expose(), b"hunter2");
        assert!(!answer.remember, "the box was not ticked");
        assert!(
            !format!("{result:?}").contains("hunter2"),
            "and the answer's own Debug redacts"
        );
    }

    #[test]
    fn the_prompt_retains_nothing_once_it_has_answered() {
        let mut d = SecretDialog::new(password(), false);
        typed(&mut d, "hunter2");
        let _ = d.handle_key(&key(KeyCode::Enter));
        assert_eq!(d.typed(), 0);
        let drawn = screen(&d, 60, 12);
        assert!(!drawn.contains('*'), "{drawn}");
        assert!(!format!("{d:?}").contains("hunter2"));
    }

    #[test]
    fn the_debug_of_a_prompt_says_nothing_a_log_should_not_have() {
        // the design S1: not the bytes, and not the length either.
        let mut d = SecretDialog::new(password(), true);
        typed(&mut d, "hunter2");
        let printed = format!("{d:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(!printed.contains('7'), "not even the length: {printed}");
        assert!(printed.contains("empty: false"), "{printed}");
    }

    #[test]
    fn backspace_is_the_only_edit_and_it_keeps_the_count_honest() {
        let mut d = SecretDialog::new(password(), false);
        typed(&mut d, "abc");
        d.handle_key(&key(KeyCode::Backspace));
        assert_eq!(d.typed(), 2);
        // Nothing to move a caret over, so the caret keys do nothing at all.
        for code in [KeyCode::Home, KeyCode::End, KeyCode::Delete] {
            d.handle_key(&key(code));
        }
        assert_eq!(d.typed(), 2);
        let DialogOutcome::Accept(DialogResult::Secret(answer)) =
            d.handle_key(&key(KeyCode::Enter))
        else {
            panic!("Enter answers");
        };
        assert_eq!(answer.secret.expose(), b"ab");

        // Backspace on an empty buffer is not an underflow.
        let mut empty = SecretDialog::new(password(), false);
        empty.handle_key(&key(KeyCode::Backspace));
        assert_eq!(empty.typed(), 0);
    }

    #[test]
    fn a_passphrase_is_never_offered_the_keyring_checkbox() {
        // the opt-in is about a host's
        // password, and offering to store a key's passphrase would be inventing
        // a policy the design does not have. Enforced here rather than at the call
        // site, so every caller gets it.
        let d = SecretDialog::new(passphrase(), true);
        assert!(!d.offers_keyring());
        assert_eq!(d.mnemonic_letters(), vec!['o', 'n']);
        assert!(d.title().contains("id_ed25519"));
        let drawn = screen(&d, 60, 12);
        assert!(!drawn.contains("keyring"), "{drawn}");
        assert!(drawn.contains("Passphrase:"), "{drawn}");
    }

    #[test]
    fn the_checkbox_is_there_only_where_the_host_opted_in_and_a_store_exists() {
        // and the design.
        let with = SecretDialog::new(password(), true);
        assert_eq!(with.mnemonic_letters(), vec!['s', 'o', 'n']);
        let without = SecretDialog::new(password(), false);
        assert_eq!(without.mnemonic_letters(), vec!['o', 'n']);
        assert!(matches!(
            without.accel(PromptControl::Remember),
            Accel::Absent
        ));
        // And the letter is swallowed rather than doing something invisible.
        let mut without = without;
        assert!(matches!(
            without.handle_key(&alt('s')),
            DialogOutcome::Consumed
        ));
        assert!(!without.remembers());
    }

    #[test]
    fn alt_s_ticks_the_box_and_never_unticks_it() {
        // "An accelerator never turns anything off."
        let mut d = SecretDialog::new(password(), true);
        for _ in 0..3 {
            d.handle_key(&alt('s'));
            assert!(d.remembers());
            assert_eq!(d.focused(), PromptControl::Remember);
        }
        // `Space` is how it comes off, one keystroke away with the focus on it.
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(!d.remembers());
    }

    #[test]
    fn a_ticked_box_travels_with_the_answer_and_only_where_it_was_offered() {
        let mut d = SecretDialog::new(password(), true);
        d.handle_key(&alt('s'));
        typed(&mut d, "hunter2");
        let DialogOutcome::Accept(DialogResult::Secret(answer)) =
            d.handle_key(&key(KeyCode::Enter))
        else {
            panic!("Enter answers");
        };
        assert!(answer.remember);
    }

    #[test]
    fn a_missing_keyring_is_said_out_loud_on_its_own_row() {
        // "If no keyring is available, say so in the dialog and
        // fall back to prompting every time - do not silently write the
        // password to disk."
        let note = crate::remote::keyring::unavailable_message();
        let d = SecretDialog::new(password(), false).with_note(note.clone());
        assert_eq!(d.note(), Some(note.as_str()));
        let drawn = screen(&d, 80, 14);
        assert!(drawn.contains("keyring"), "{drawn}");
        assert!(!d.offers_keyring(), "and there is no box to tick");
    }

    #[test]
    fn esc_gives_up_and_hands_nothing_back() {
        let mut d = SecretDialog::new(password(), true);
        typed(&mut d, "hunter2");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn an_alt_key_never_types_into_the_buffer() {
        // Two things keep it out: `DialogKey::text` is `None` whenever `Alt`
        // is held, and mnemonics are handled before the field sees the key.
        let mut d = SecretDialog::new(password(), true);
        for letter in ['s', 'o', 'n', 'z'] {
            d.handle_key(&alt(letter));
        }
        assert_eq!(d.typed(), 0);
    }

    #[test]
    fn every_letter_the_table_names_is_underlined_on_the_screen() {
        let d = SecretDialog::new(password(), true);
        let mut drawn = underlined(&d, 70, 14);
        drawn.sort_unstable();
        assert_eq!(drawn, vec!['n', 'o', 's']);
    }

    #[test]
    fn the_prompt_lays_itself_out_against_the_rectangle_it_is_given() {
        // the 60x15 floor.
        let d = SecretDialog::new(password(), true)
            .with_note(crate::remote::keyring::unavailable_message());
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (20, 5), (4, 3)] {
            let drawn = screen(&d, w, h);
            for line in drawn.lines() {
                assert_eq!(crate::ui::text::width(line), usize::from(w), "{w}x{h}");
            }
        }
    }

    #[test]
    fn a_long_secret_does_not_draw_off_the_edge_of_the_field() {
        let mut d = SecretDialog::new(password(), false);
        typed(&mut d, &"x".repeat(400));
        assert_eq!(d.typed(), 400);
        let drawn = screen(&d, 60, 12);
        for line in drawn.lines() {
            assert_eq!(crate::ui::text::width(line), 60);
        }
    }
}
