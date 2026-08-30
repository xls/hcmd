//! `F2`, pure rename.
//!
//! ```text
//!            ┌──────────────── Rename ─────────────────┐
//!            │ New name:                               │
//!            │ ┌─────────────────────────────────────┐ │
//!            │ │ archive.tar.gz                      │ │
//!            │ └─────────────────────────────────────┘ │
//!            │            [ OK ]   [ Cancel ]          │
//!            └─────────────────────────────────────────┘
//! ```
//!
//! the design states four things and this file is those four things:
//!
//! * **Only the filename.** No path, no mask, no target directory - "It cannot
//!   move anything, which is the point: there is no way to fat-finger a path
//!   and discover the file somewhere else." A separator in the field is
//!   therefore refused rather than obeyed.
//! * **The stem is preselected and the extension is not**, so typing replaces
//!   the name and keeps `.tar.gz` intact. "Extension" is the `ext` column's
//!   definition - one definition of the word in the program -
//!   which is [`crate::vfs::Entry::split_name`], so `archive.tar.gz` preselects
//!   `archive.tar`. `Ctrl+A` selects the lot.
//! * **`Enter` renames, `Esc` cancels.**
//! * **A name that already exists is refused in the dialog**, before anything
//!   happens, "rather than opening the conflict flow - at this size a conflict
//!   is a typo, not a decision". The sibling names are handed in at
//!   construction: a dialog reads no filesystem.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::field::Field;
use super::row;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    draw_mnemonic, draw_mnemonic_buttons, draw_text,
};
use crate::input::{Action, DialogId, KeyCode};
use crate::ui::text;

/// The field, then the two buttons.
const FIELD: usize = 0;
/// `OK`.
const OK: usize = 1;
/// `Cancel`.
const CANCEL: usize = 2;

/// The field's label, and the string the underline is searched in.
const FIELD_LABEL: &str = "New name:";

/// the `Alt` mnemonics for this dialog.
///
/// `n` is reserved program-wide for `Cancel`, so the field takes the first
/// free letter of its own label - the `e` of `N`**`e`**`w`. The field opens
/// focused with the stem preselected, so its mnemonic is only ever a way back
/// from the buttons.
pub const MNEMONICS: &[(usize, char)] = &[(FIELD, 'e'), (OK, 'o'), (CANCEL, 'n')];

/// the rename dialog.
#[derive(Debug)]
pub struct RenameDialog {
    /// The name as it is now, so an unchanged answer is a no-op rather than a
    /// job that fails with "the source and the destination are the same file".
    original: String,
    field: Field,
    /// Everything else in the directory, for the in-dialog refusal.
    siblings: Vec<String>,
    /// Why the last `Enter` did nothing, shown under the field.
    error: Option<String>,
    ring: FocusRing,
}

impl RenameDialog {
    /// A dialog for `name`, with the other names in its directory.
    ///
    /// `is_dir` decides where the preselection ends: a directory has no
    /// extension in the `ext` column's sense, so the whole name is selected.
    pub fn new(name: impl Into<String>, is_dir: bool, siblings: Vec<String>) -> Self {
        let original: String = name.into();
        let mut field = Field::with_text(original.clone());
        let stem = stem_len(&original, is_dir);
        if stem > 0 {
            field.select(0, stem);
        }
        Self {
            original,
            field,
            siblings,
            error: None,
            ring: FocusRing::new(3),
        }
    }

    /// What is in the field.
    pub fn name(&self) -> &str {
        self.field.text()
    }

    /// The preselected range, for a test that asserts the design's
    /// "the stem is preselected and the extension is not".
    pub fn selected(&self) -> String {
        self.field.selected_text()
    }

    /// The refusal currently shown, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Refuse in the dialog, or hand back the new name.
    fn accept(&mut self) -> DialogOutcome {
        let name = self.field.text().trim().to_string();
        if name.is_empty() {
            self.error = Some("a name cannot be empty".to_string());
            return DialogOutcome::Consumed;
        }
        if name == self.original {
            // Nothing to do, and nothing to report: this is `Esc` with extra
            // steps.
            return DialogOutcome::Cancel;
        }
        if name.contains('/') {
            // The whole point of `F2` over `F6`.
            self.error = Some("F2 renames only; use F6 to move".to_string());
            return DialogOutcome::Consumed;
        }
        if name == "." || name == ".." {
            self.error = Some("that name is not a file".to_string());
            return DialogOutcome::Consumed;
        }
        if self.siblings.contains(&name) {
            self.error = Some(format!("{name} already exists"));
            return DialogOutcome::Consumed;
        }
        DialogOutcome::Accept(DialogResult::Text(name))
    }
}

/// How much of `name` the stem is.
fn stem_len(name: &str, is_dir: bool) -> usize {
    if is_dir {
        return name.chars().count();
    }
    // The `ext` column's rule: the part after the **last** dot, and a leading
    // dot is not a separator, so `.bashrc` is all stem.
    match name.rfind('.') {
        Some(0) | None => name.chars().count(),
        Some(byte) => name.get(..byte).unwrap_or(name).chars().count(),
    }
}

impl Accelerated for RenameDialog {
    /// Ring indices rather than an enum: three controls is under
    /// the five-control floor.
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

    /// Nothing here is a checkbox.
    fn switch_on(&mut self, _control: usize) {}

    fn press(&mut self, control: usize) -> DialogOutcome {
        match control {
            // The same in-dialog refusals `Enter` gets: a
            // name that exists, a name with a separator in it, an empty name.
            OK => self.accept(),
            CANCEL => DialogOutcome::Cancel,
            _ => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for RenameDialog {
    fn id(&self) -> DialogId {
        DialogId::Rename
    }

    fn title(&self) -> String {
        "Rename".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = text::width(self.field.text())
            .max(self.error.as_deref().map(text::width).unwrap_or(0))
            .max(24);
        let w = u16::try_from(widest.saturating_add(8)).unwrap_or(u16::MAX);
        // Label, field, error, buttons, two borders.
        (w.clamp(36, 76), 7)
    }

    /// `Alt+E`, `Alt+O` and `Alt+N`.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action` - `Ctrl+A`'s
        // `SelectAll` included - or reaches the field.
        //
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
            if self.ring.is(CANCEL) {
                return DialogOutcome::Cancel;
            }
            return self.accept();
        }
        if key.action == Some(Action::SelectAll) {
            // `Ctrl+A` selects the lot, "for the rarer case where the extension
            // is the thing being changed".
            let all = self.field.text().chars().count();
            self.field.select(0, all);
            return DialogOutcome::Consumed;
        }
        if self.ring.is(FIELD) {
            // A key that changes the text has answered the refusal on screen.
            let before = self.field.text().to_string();
            if self.field.handle(key) {
                if before != self.field.text() {
                    self.error = None;
                }
                return DialogOutcome::Consumed;
            }
        }
        match key.press.code {
            KeyCode::Left | KeyCode::Up => {
                self.ring.prev();
                DialogOutcome::Consumed
            }
            KeyCode::Right | KeyCode::Down => {
                self.ring.next();
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if let Some(rect) = row(area, 0) {
            draw_mnemonic(f, rect, FIELD_LABEL, 'e', style.body(), style.ascii);
        }
        if let Some(rect) = row(area, 1) {
            self.field.render(f, rect, style);
        }
        if let Some(rect) = row(area, 2)
            && let Some(error) = self.error.as_deref()
        {
            draw_text(f, rect, error, style.button(true), style.ascii);
        }
        // The buttons take the last row there is, so a 60x15 terminal still has
        // a way to say yes.
        let last = area.height.saturating_sub(1);
        if let Some(rect) = row(area, last.max(3)) {
            let focused = match self.ring.index() {
                OK => 0,
                CANCEL => 1,
                _ => usize::MAX,
            };
            draw_mnemonic_buttons(
                f,
                rect,
                &[("OK", Some('o')), ("Cancel", Some('n'))],
                focused,
                style,
            );
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if !self.ring.is(FIELD) {
            return None;
        }
        self.field.cursor(row(area, 1)?)
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
    fn underlined(d: &RenameDialog, w: u16, h: u16) -> Vec<char> {
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

    fn typed(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn the_stem_is_preselected_and_the_extension_is_not() {
        // and the definition of "extension": the part after the last
        // dot.
        let dialog = RenameDialog::new("archive.tar.gz", false, Vec::new());
        assert_eq!(dialog.selected(), "archive.tar");

        // A dotfile has no stem to speak of; a directory has no extension.
        assert_eq!(
            RenameDialog::new(".bashrc", false, Vec::new()).selected(),
            ".bashrc"
        );
        assert_eq!(
            RenameDialog::new("photos.d", true, Vec::new()).selected(),
            "photos.d"
        );
    }

    #[test]
    fn typing_replaces_the_stem_and_keeps_the_extension() {
        let mut dialog = RenameDialog::new("archive.tar.gz", false, Vec::new());
        for c in "backup".chars() {
            dialog.handle_key(&typed(c));
        }
        assert_eq!(dialog.name(), "backup.gz");
    }

    #[test]
    fn an_existing_name_is_refused_in_the_dialog() {
        // "A name that already exists is refused in the
        // dialog, before anything happens, rather than opening the conflict
        // flow." So `Enter` does not close it.
        let mut dialog = RenameDialog::new("a.txt", false, vec!["b.txt".to_string()]);
        for c in "b".chars() {
            dialog.handle_key(&typed(c));
        }
        assert_eq!(dialog.name(), "b.txt");
        let outcome = dialog.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, DialogOutcome::Consumed), "{outcome:?}");
        assert_eq!(dialog.error(), Some("b.txt already exists"));

        // And editing clears the refusal.
        dialog.handle_key(&key(KeyCode::Backspace));
        assert_eq!(dialog.error(), None);
    }

    #[test]
    fn a_path_is_refused_because_f2_cannot_move_anything() {
        let mut dialog = RenameDialog::new("a.txt", false, Vec::new());
        dialog.handle_key(&typed('x'));
        for c in "/b.txt".chars() {
            dialog.handle_key(&typed(c));
        }
        let outcome = dialog.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, DialogOutcome::Consumed), "{outcome:?}");
        assert!(
            dialog.error().unwrap_or_default().contains("F6"),
            "{:?}",
            dialog.error()
        );
    }

    #[test]
    fn an_unchanged_name_is_a_cancel_rather_than_a_job() {
        let mut dialog = RenameDialog::new("a.txt", false, Vec::new());
        let outcome = dialog.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, DialogOutcome::Cancel), "{outcome:?}");
    }

    #[test]
    fn a_new_name_is_the_answer() {
        let mut dialog = RenameDialog::new("a.txt", false, vec!["b.txt".to_string()]);
        dialog.handle_key(&typed('c'));
        let outcome = dialog.handle_key(&key(KeyCode::Enter));
        match outcome {
            DialogOutcome::Accept(DialogResult::Text(name)) => assert_eq!(name, "c.txt"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_control_is_reachable_by_its_alt_letter() {
        // control by control: the
        // field is focused and left alone, and each button is focused and
        // pressed.
        let mut d = RenameDialog::new("a.txt", false, Vec::new());
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.ring.index(), OK);
        d.handle_key(&alt('e'));
        assert_eq!(d.ring.index(), FIELD, "Alt+E is the way back to the field");
        assert_eq!(d.name(), "a.txt", "and it changed nothing");

        let mut d = RenameDialog::new("a.txt", false, Vec::new());
        d.handle_key(&typed('b'));
        match d.handle_key(&alt('o')) {
            DialogOutcome::Accept(DialogResult::Text(name)) => assert_eq!(name, "b.txt"),
            other => panic!("Alt+O pressed OK, got {other:?}"),
        }

        let mut d = RenameDialog::new("a.txt", false, Vec::new());
        d.handle_key(&typed('b'));
        assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));
    }

    #[test]
    fn alt_o_gets_the_same_in_dialog_refusal_enter_gets() {
        // One definition of what `OK` does, whichever route pressed it.
        //
        let mut d = RenameDialog::new("a.txt", false, vec!["b.txt".to_string()]);
        d.handle_key(&typed('b'));
        assert!(matches!(d.handle_key(&alt('o')), DialogOutcome::Consumed));
        assert_eq!(d.error(), Some("b.txt already exists"));
    }

    #[test]
    fn a_mnemonic_never_types_and_never_edits() {
        // the design I8: `DialogKey::text` is `None` under `ALT`,
        // and the mnemonic check runs before the field ever sees the key.
        let mut d = RenameDialog::new("archive.tar.gz", false, Vec::new());
        let before = d.name().to_string();
        for letter in ['e', 'z', 'q'] {
            d.handle_key(&alt(letter));
            assert_eq!(d.name(), before, "Alt+{letter} typed something");
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
        let d = RenameDialog::new("a.txt", false, Vec::new());
        assert_eq!(d.mnemonic_letters(), vec!['e', 'o', 'n']);
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // the underline, read off the buffer rather than off the
        // table, so a declared letter with no paint behind it fails here.
        let d = RenameDialog::new("archive.tar.gz", false, Vec::new());
        let mut got: Vec<char> = underlined(&d, 60, 14)
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec!['e', 'n', 'o']);
    }
}
