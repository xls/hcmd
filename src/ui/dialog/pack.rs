//! the `Alt+F5`.
//!
//! > `Alt+F5` packs the selection: a dialog for target name, format,
//! > compression level, and "move to archive" (pack then delete sources).
//!
//! Four questions, four controls, and the answer is a [`PackAnswer`] rather
//! than an operation: the dialog neither creates the archive nor starts the
//! job, for the same reason no other dialog touches the filesystem.
//! [`crate::app::App::take_pending_pack`] is where
//! the event loop picks the work up.
//!
//! ```text
//!     ╭ Pack 3 file(s) ───────────────────────────────────────╮
//!     │Pack 3 file(s) into                                    │
//!     │/srv/media/photos.tar.gz                               │
//!     │Format:  < .tar.gz >        Compression:  < 6 >        │
//!     │[ ] Move to archive (delete the originals afterwards)  │
//!     │            [ OK ]   [ Cancel ]                        │
//!     ╰───────────────────────────────────────────────────────╯
//! ```
//!
//! **The name and the format are kept in step.** Choosing `.7z` rewrites the
//! extension in the field, and typing an extension the field recognises moves
//! the format selector - because two controls that disagree about the same
//! thing produce an archive the user did not ask for, and the one the user
//! last touched is the one that means it.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::checkbox;
use super::field::Field;
use super::row;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    PackAnswer, Piece, draw_mnemonic, draw_mnemonic_buttons, draw_mnemonic_pieces, draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::vfs::archive::format::{CompressionLevel, FormatId};

/// The formats `Alt+F5` offers, in [`FormatId::ALL`]'s order.
///
/// Derived, not listed: a format is offered here exactly when a new archive
/// of it can be created, and each format already answers that through
/// [`ArchiveFormat::can_create`](crate::vfs::archive::format::ArchiveFormat::can_create).
/// A second hand-kept list would be one that could drift - offer a `.rar` that
/// fails at `OK`, or forget a format added later - so the dialog reads the
/// capability instead of restating it.
///
/// `can_create`, and not `WriteModel::writable`: the write model answers for
/// members of an archive that already exists, and the two coincide today only
/// because every member-writable format happens to be a container. A `.gz`
/// that learned to rewrite its one member would become writable without ever
/// becoming something a selection fits into, and a filter on `writable()`
/// would offer it here and fail at the job.
///
/// `.rar` is absent and never will be: "RAR compression
/// is patent-encumbered and `unrar` cannot write", so nothing can create one
/// and the filter drops it. Offering a choice that fails at `OK` would be the
/// opposite of the "refused up front".
fn packable() -> Vec<FormatId> {
    FormatId::ALL
        .iter()
        .copied()
        .filter(|f| f.backend().can_create())
        .collect()
}

/// One focusable control, in `Tab` order.
///
/// An enum rather than the six `usize` constants it replaces, because
/// the design puts the floor at five controls: with six,
/// [`PackDialog::accel`] is an exhaustive `match` and a control added later
/// cannot be forgotten there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The archive name.
    Name,
    /// The format stepper.
    Format,
    /// The compression-level stepper.
    Level,
    /// "Move to archive".
    Move,
    /// Create it.
    Ok,
    /// Give up.
    Cancel,
}

/// Every control, in `Tab` order. The ring's length is this.
const CONTROLS: &[Control] = &[
    Control::Name,
    Control::Format,
    Control::Level,
    Control::Move,
    Control::Ok,
    Control::Cancel,
];

/// The "move to archive" checkbox's label, and the string the design's
/// underline is searched in.
const MOVE_LABEL: &str = "Move to archive (delete the originals afterwards)";

/// the `Alt` mnemonics for this dialog.
///
/// `o` and `n` are the program-wide `OK` and `Cancel`. `Alt+C` is
/// `Compression` here rather than `Close`, because this dialog has no `Close`:
/// the reservation is a floor on what a letter may mean, not a requirement
/// that every dialog spend it.
///
/// `i` is the `i` of `into`, which the word-start rule picks over the `i` of
/// `file(s)` earlier in the same line.
pub const MNEMONICS: &[(Control, char)] = &[
    (Control::Name, 'i'),
    (Control::Format, 'f'),
    (Control::Level, 'c'),
    (Control::Move, 'm'),
    (Control::Ok, 'o'),
    (Control::Cancel, 'n'),
];

/// The `Alt+F5` dialog.
#[derive(Debug)]
pub struct PackDialog {
    count: usize,
    field: Field,
    format: usize,
    level: u8,
    move_sources: bool,
    error: Option<String>,
    ring: FocusRing,
}

impl PackDialog {
    /// A dialog packing `count` items into `target`, opening on the format
    /// `target`'s own extension names.
    pub fn new(count: usize, target: impl Into<String>) -> Self {
        let target: String = target.into();
        let format = format_of(&target).unwrap_or(0);
        Self {
            count,
            field: Field::with_text(target),
            format,
            level: CompressionLevel::DEFAULT.get(),
            move_sources: false,
            error: None,
            ring: FocusRing::new(CONTROLS.len()),
        }
    }

    /// Which control has focus.
    pub fn focused(&self) -> Control {
        CONTROLS
            .get(self.ring.index())
            .copied()
            .unwrap_or(Control::Name)
    }

    /// The format stepper's own label, which is the string its `f` is
    /// underlined in.
    fn format_label(&self) -> String {
        format!("Format: < {} >", self.format().extension())
    }

    /// The compression stepper's own label.
    fn level_label(&self) -> String {
        format!("Compression: < {} >", self.level)
    }

    /// The target as it stands.
    pub fn target(&self) -> &str {
        self.field.text()
    }

    /// The format currently chosen.
    pub fn format(&self) -> FormatId {
        packable()
            .get(self.format)
            .copied()
            .unwrap_or(FormatId::Zip)
    }

    /// The compression level currently chosen.
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Whether "move to archive" is ticked.
    pub fn moves(&self) -> bool {
        self.move_sources
    }

    /// The refusal currently shown, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Step the format selector and rewrite the field's extension to match.
    fn step_format(&mut self, forward: bool) {
        let n = packable().len();
        self.format = if forward {
            (self.format + 1) % n
        } else {
            (self.format + n - 1) % n
        };
        let retyped = with_extension(self.field.text(), self.format());
        self.field.set_text(retyped);
        self.error = None;
    }

    /// Step the compression level, clamping rather than wrapping: `0` and `9`
    /// are the ends of a scale, not a ring, and wrapping from "store" to
    /// "maximum" on one keypress is a surprise.
    fn step_level(&mut self, forward: bool) {
        self.level = if forward {
            self.level
                .saturating_add(1)
                .min(CompressionLevel::MAX.get())
        } else {
            self.level.saturating_sub(1)
        };
    }

    fn accept(&mut self) -> DialogOutcome {
        let target = self.field.text().trim().to_string();
        if target.is_empty() {
            self.error = Some("an archive needs a name".to_string());
            return DialogOutcome::Consumed;
        }
        if target.ends_with('/') {
            self.error = Some("that names a directory, not an archive".to_string());
            return DialogOutcome::Consumed;
        }
        DialogOutcome::Accept(DialogResult::Pack(Box::new(PackAnswer {
            target,
            format: self.format(),
            level: self.level,
            move_sources: self.move_sources,
        })))
    }
}

/// Which [`packable`] index `name`'s extension names, if any.
fn format_of(name: &str) -> Option<usize> {
    let found = FormatId::from_name(name)?;
    packable().iter().position(|f| *f == found)
}

/// `name` with whatever archive extension it has replaced by `format`'s.
///
/// A name with no recognised extension keeps every character it has and gains
/// one: `photos` becomes `photos.tar.gz`, not `photos.gz`, and a `report.2026`
/// does not lose its `.2026`.
fn with_extension(name: &str, format: FormatId) -> String {
    // The suffix that is really there, not this format's canonical one:
    // `photos.tgz` is a `.tar.gz` spelled four characters shorter, and taking
    // seven characters off it would eat the name.
    let stem = match FormatId::suffix_of(name) {
        Some((suffix, _)) => name
            .get(..name.len().saturating_sub(suffix.len()))
            .unwrap_or(name),
        None => name,
    };
    format!("{stem}{}", format.extension())
}

impl Accelerated for PackDialog {
    type Control = Control;

    fn mnemonics(&self) -> &'static [(Control, char)] {
        MNEMONICS
    }

    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            // The two steppers are focus-only: the design gives no meaning to
            // "accelerate a three-way control", and both candidate meanings
            // turn something off.
            Control::Name | Control::Format | Control::Level => Accel::Focus,
            Control::Move => Accel::Check,
            Control::Ok | Control::Cancel => Accel::Press,
        }
    }

    fn focus_control(&mut self, control: Control) {
        if let Some(at) = CONTROLS.iter().position(|c| *c == control) {
            self.ring.set(at);
        }
    }

    /// **on**, never a toggle. `Space` is how it comes off, and
    /// it is one keystroke away with the focus now on the box.
    fn switch_on(&mut self, control: Control) {
        match control {
            Control::Move => self.move_sources = true,
            Control::Name | Control::Format | Control::Level | Control::Ok | Control::Cancel => {}
        }
    }

    fn press(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Ok => self.accept(),
            Control::Cancel => DialogOutcome::Cancel,
            Control::Name | Control::Format | Control::Level | Control::Move => {
                DialogOutcome::Consumed
            }
        }
    }
}

impl Dialog for PackDialog {
    fn id(&self) -> DialogId {
        DialogId::Pack
    }

    fn title(&self) -> String {
        format!("Pack {} file(s)", self.count)
    }

    fn size_hint(&self) -> (u16, u16) {
        // Label, field, format+level, the checkbox, an error line, buttons,
        // two borders.
        (64, 9)
    }

    /// All six letters.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action` or reaches a
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
            // One route to each button: `Enter` presses whichever has focus,
            // exactly as `Alt`+letter does.
            return match self.focused() {
                Control::Cancel => self.press(Control::Cancel),
                Control::Name | Control::Format | Control::Level | Control::Move | Control::Ok => {
                    self.press(Control::Ok)
                }
            };
        }
        if self.focused() == Control::Name {
            let before = self.field.text().to_string();
            if self.field.handle(key) {
                if before != self.field.text() {
                    self.error = None;
                    // The field is the control the user just touched, so it is
                    // the one that means it: an extension typed here moves the
                    // selector rather than being overwritten by it.
                    if let Some(index) = format_of(self.field.text()) {
                        self.format = index;
                    }
                }
                return DialogOutcome::Consumed;
            }
        }
        if self.focused() == Control::Move && key.press.code == KeyCode::Char(' ') {
            self.move_sources = !self.move_sources;
            return DialogOutcome::Consumed;
        }
        match key.press.code {
            KeyCode::Left if self.focused() == Control::Format => {
                self.step_format(false);
                DialogOutcome::Consumed
            }
            KeyCode::Right if self.focused() == Control::Format => {
                self.step_format(true);
                DialogOutcome::Consumed
            }
            KeyCode::Left if self.focused() == Control::Level => {
                self.step_level(false);
                DialogOutcome::Consumed
            }
            KeyCode::Right if self.focused() == Control::Level => {
                self.step_level(true);
                DialogOutcome::Consumed
            }
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

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focused();
        if let Some(rect) = row(area, 0) {
            draw_mnemonic(
                f,
                rect,
                &format!("Pack {} file(s) into", self.count),
                'i',
                style.body(),
                style.ascii,
            );
        }
        if let Some(rect) = row(area, 1) {
            self.field.render(f, rect, style);
        }
        if let Some(rect) = row(area, 2) {
            // Two pieces and not one string: a mnemonic is scoped to its own
            // control's label, and as one string
            // the `c` of `Compression` would be found in `Format`'s half first.
            // Each piece also carries its own focus highlight, which these two
            // steppers did not have before.
            let pieces = [
                Piece::new(
                    self.format_label(),
                    Some('f'),
                    style.button(focused == Control::Format),
                    focused == Control::Format,
                ),
                Piece::new(
                    self.level_label(),
                    Some('c'),
                    style.button(focused == Control::Level),
                    focused == Control::Level,
                ),
            ];
            draw_mnemonic_pieces(f, rect, &pieces, style.body());
        }
        if let Some(rect) = row(area, 3) {
            let text = checkbox(MOVE_LABEL, self.move_sources, style.ascii);
            draw_mnemonic(
                f,
                rect,
                &text,
                'm',
                style.focus_label(focused == Control::Move),
                style.ascii,
            );
        }
        if let Some(rect) = row(area, 4)
            && let Some(error) = self.error.as_deref()
        {
            draw_text(f, rect, error, style.button(true), style.ascii);
        }
        let last = area.height.saturating_sub(1);
        if let Some(rect) = row(area, last.max(5)) {
            let index = match focused {
                Control::Ok => 0,
                Control::Cancel => 1,
                // Focus is on a field or a stepper, so neither button is
                // highlighted.
                Control::Name | Control::Format | Control::Level | Control::Move => usize::MAX,
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
        if self.focused() != Control::Name {
            return None;
        }
        self.field.cursor(row(area, 1)?)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyModifiers, KeyPress};

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn typed(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    /// `Alt`+letter, the keystroke the design is about.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    /// Every character drawn with [`ratatui::style::Modifier::UNDERLINED`],
    /// folded to lower case. the underline, read off the buffer
    /// rather than off the table, so a declared letter with no paint behind it
    /// fails the test that uses this.
    fn underlined(d: &PackDialog, w: u16, h: u16) -> Vec<char> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(
            &crate::config::Theme::blue(),
            crate::config::ColorDepth::TrueColor,
            false,
        );
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
                if cell.modifier.contains(ratatui::style::Modifier::UNDERLINED) {
                    out.extend(cell.symbol().chars());
                }
            }
        }
        out.iter().map(|c| c.to_ascii_lowercase()).collect()
    }

    #[test]
    fn it_opens_on_the_format_the_name_already_names() {
        // the design asks for a name *and* a format, and a dialog that
        // opened on `.zip` over a field reading `photos.tar.gz` would be
        // showing two answers to one question.
        let dialog = PackDialog::new(3, "/srv/photos.tar.gz");
        assert_eq!(dialog.format(), FormatId::TarGz);
        assert_eq!(dialog.level(), CompressionLevel::DEFAULT.get());
        assert!(!dialog.moves());
    }

    #[test]
    fn choosing_a_format_rewrites_the_extension_rather_than_appending_one() {
        let mut dialog = PackDialog::new(1, "/srv/photos.tar.gz");
        dialog.focus_control(Control::Format);
        dialog.handle_key(&key(KeyCode::Right));
        assert_eq!(dialog.format(), FormatId::TarBz2);
        assert_eq!(dialog.target(), "/srv/photos.tar.bz2");

        // And back again, without accumulating extensions.
        dialog.handle_key(&key(KeyCode::Left));
        assert_eq!(dialog.target(), "/srv/photos.tar.gz");
    }

    #[test]
    fn a_name_with_no_archive_extension_gains_one_and_keeps_the_rest() {
        let mut dialog = PackDialog::new(1, "/srv/report.2026");
        dialog.focus_control(Control::Format);
        dialog.handle_key(&key(KeyCode::Right));
        assert!(
            dialog.target().starts_with("/srv/report.2026"),
            "{}",
            dialog.target()
        );
    }

    #[test]
    fn typing_an_extension_moves_the_selector() {
        // The control the user last touched is the one that means it.
        let mut dialog = PackDialog::new(1, "/srv/photos.zip");
        assert_eq!(dialog.format(), FormatId::Zip);
        dialog.focus_control(Control::Name);
        for _ in 0..4 {
            dialog.handle_key(&key(KeyCode::Backspace));
        }
        for c in ".7z".chars() {
            dialog.handle_key(&typed(c));
        }
        assert_eq!(dialog.target(), "/srv/photos.7z");
        assert_eq!(dialog.format(), FormatId::SevenZ);
    }

    #[test]
    fn the_level_clamps_at_both_ends_rather_than_wrapping() {
        let mut dialog = PackDialog::new(1, "/srv/a.zip");
        dialog.focus_control(Control::Level);
        for _ in 0..20 {
            dialog.handle_key(&key(KeyCode::Right));
        }
        assert_eq!(dialog.level(), CompressionLevel::MAX.get());
        for _ in 0..20 {
            dialog.handle_key(&key(KeyCode::Left));
        }
        assert_eq!(dialog.level(), CompressionLevel::STORE.get());
    }

    #[test]
    fn space_ticks_move_to_archive_and_ok_hands_back_every_answer() {
        let mut dialog = PackDialog::new(2, "/srv/a.zip");
        dialog.focus_control(Control::Move);
        dialog.handle_key(&typed(' '));
        assert!(dialog.moves());
        dialog.focus_control(Control::Ok);
        match dialog.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Pack(answer)) => {
                assert_eq!(answer.target, "/srv/a.zip");
                assert_eq!(answer.format, FormatId::Zip);
                assert!(answer.move_sources);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_name_is_refused_in_the_dialog_rather_than_started() {
        let mut dialog = PackDialog::new(1, "");
        dialog.focus_control(Control::Ok);
        assert!(matches!(
            dialog.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));
        assert!(dialog.error().is_some());
    }

    #[test]
    fn rar_is_not_offered_because_it_cannot_be_written() {
        // Offering a choice that fails at `OK` is the opposite
        // of the "refused up front", and `.rar` is read only.
        assert!(!packable().contains(&FormatId::Rar));
    }

    #[test]
    fn the_offer_is_the_seven_container_formats_in_table_order() {
        // The concrete list, spelled out: an assertion that recomputes the
        // implementation's own filter cannot fail, and what this test guards
        // is what the user is offered. A format gaining or losing
        // `can_create` shows up here as a readable diff.
        assert_eq!(
            packable(),
            [
                FormatId::Zip,
                FormatId::Tar,
                FormatId::TarGz,
                FormatId::TarBz2,
                FormatId::TarXz,
                FormatId::TarZst,
                FormatId::SevenZ,
            ]
        );
    }

    #[test]
    fn every_control_is_reachable_by_its_alt_letter() {
        // control by control. The two steppers are focus-only: the design
        // gives no meaning to accelerating a three-way control, and both
        // candidate meanings turn something off.
        let mut d = PackDialog::new(3, "/srv/photos.tar.gz");
        let opening = d.format();
        for (letter, want) in [
            ('i', Control::Name),
            ('f', Control::Format),
            ('c', Control::Level),
            ('m', Control::Move),
        ] {
            d.handle_key(&alt(letter));
            assert_eq!(d.focused(), want, "Alt+{letter}");
        }
        assert_eq!(d.format(), opening, "a stepper was focused, not stepped");
        assert_eq!(
            d.level(),
            CompressionLevel::DEFAULT.get(),
            "and neither was the level"
        );

        let mut d = PackDialog::new(3, "/srv/photos.tar.gz");
        match d.handle_key(&alt('o')) {
            DialogOutcome::Accept(DialogResult::Pack(answer)) => {
                assert_eq!(answer.target, "/srv/photos.tar.gz");
            }
            other => panic!("Alt+O pressed OK, got {other:?}"),
        }

        let mut d = PackDialog::new(3, "/srv/photos.tar.gz");
        assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));
    }

    #[test]
    fn a_mnemonic_never_turns_the_checkbox_off() {
        // "a key that enabled on the way in and disabled on the
        // way back would make a repeated keystroke destructive, and the user is
        // reaching for it because they want to type there." `Alt+M` only ever
        // ticks; `Space` is how it comes off, one keystroke away with the focus
        // now on the box.
        let mut d = PackDialog::new(3, "/srv/photos.zip");
        assert!(!d.moves());
        d.handle_key(&alt('m'));
        assert!(d.moves(), "Alt+M ticked it");
        d.handle_key(&alt('m'));
        assert!(d.moves(), "Alt+M again left it ticked");
        d.handle_key(&typed(' '));
        assert!(!d.moves(), "and Space is the toggle");
    }

    #[test]
    fn a_mnemonic_never_types_into_the_name_field() {
        // the design I8. The mnemonic check runs before the field
        // ever sees the key, and `DialogKey::text` is `None` under `ALT`.
        let mut d = PackDialog::new(1, "/srv/photos.zip");
        for letter in ['i', 'f', 'c', 'm', 'z'] {
            d.handle_key(&alt(letter));
            assert_eq!(d.target(), "/srv/photos.zip", "Alt+{letter}");
        }
    }

    #[test]
    fn mnemonics_are_unique_within_this_dialog() {
        // a duplicate is a bug rather than a first-one-wins rule,
        // "because the second control becomes unreachable silently".
        let mut seen: Vec<char> = Vec::new();
        for (control, letter) in MNEMONICS {
            assert!(letter.is_ascii_lowercase(), "{control:?}: stored folded");
            assert!(!seen.contains(letter), "{control:?}: Alt+{letter} is taken");
            seen.push(*letter);
            assert!(CONTROLS.contains(control), "{control:?} is not in the ring");
        }
        assert_eq!(seen.len(), CONTROLS.len(), "every control has a letter");
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // And the word-start rule of the design is what puts
        // the `i` on `into` rather than on the `i` of `file(s)` earlier in the
        // same line.
        let d = PackDialog::new(3, "/srv/photos.tar.gz");
        let mut want = d.mnemonic_letters();
        let mut got = underlined(&d, 90, 20);
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want, "underlines on screen");
    }
}
