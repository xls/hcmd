//! the `execute = "ask"` prompt.
//!
//! > **`ask`** (default) prompts before running. The dialog names the file, its
//! > size, and what it actually is - ELF binary, shell script, Python script,
//! > detected with `infer` from the content rather than guessed from the name -
//! > and offers **Execute / Open with... / View (F3) / Cancel**.
//!
//! ```text
//!            ┌──────────────── Execute? ────────────────┐
//!            │ deploy.sh                                │
//!            │ Size: 1.2 kB                             │
//!            │ Type: shell script                       │
//!            │                                          │
//!            │ [ Execute ] [ Open with... ] [ View (F3) │
//!            └──────────────────────────────────────────┘
//! ```
//!
//! **Why the default button is `Cancel`.** the argument for
//! prompting at all is that "`Enter` is the key people press to navigate, and
//! the cost of an accidental execution is unbounded". A prompt that opened on
//! `Execute` would turn the second `Enter` of a fast `Enter Enter` into exactly
//! the accident the prompt exists to prevent, so this dialog opens on the
//! button that does nothing - the same rule `crate::dialog::ConfirmDialog`
//! keeps for a delete.
//!
//! The four choices travel out through [`crate::dialog::DialogResult::Text`],
//! carrying an [`ExecuteChoice`], exactly as [`super::JobAction`] already does:
//! `DialogResult` is fixed by the design and four buttons do not
//! earn a variant of their own.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::bytes_text;
use crate::config::PanelConfig;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle,
    draw_mnemonic_buttons, draw_text,
};
use crate::input::{DialogId, KeyCode};

/// The `Execute` button, and the ring index of it.
const EXECUTE: usize = 0;
/// The `Open with...` button (the chooser).
const OPEN_WITH: usize = 1;
/// The `View (F3)` button, which opens the internal viewer instead.
const VIEW: usize = 2;
/// The `Cancel` button, which is where the dialog opens.
const CANCEL: usize = 3;

/// the `Alt` mnemonics for this dialog.
///
/// `o` is `Open with...` and not an `OK` button, because the four
/// buttons are `Execute`, `Open with...`, `View (F3)` and `Cancel` and there is
/// no `OK` among them; that is the listed exception in
/// the design. `n` is `Cancel`, which is its program-wide
/// meaning.
pub const MNEMONICS: &[(usize, char)] =
    &[(EXECUTE, 'e'), (OPEN_WITH, 'o'), (VIEW, 'v'), (CANCEL, 'n')];

/// Which of the four the user pressed.
///
/// Travels in [`crate::dialog::DialogResult::Text`] through
/// [`ExecuteChoice::encode`], the way [`super::JobAction`] already does, so no
/// `DialogResult` variant is added for four buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecuteChoice {
    /// Run it (the `execute_in` decides where).
    Execute,
    /// Open the chooser instead.
    OpenWith,
    /// Open the internal viewer instead.
    View,
    /// Do nothing.
    Cancel,
}

impl ExecuteChoice {
    /// The wire form, deliberately boring so a test can assert on it.
    pub const fn encode(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::OpenWith => "open_with",
            Self::View => "view",
            Self::Cancel => "cancel",
        }
    }

    /// Read one back.
    ///
    /// `None` for anything that is not one of these - which is every other
    /// dialog's `Text` answer, so an arm that calls this can share a match with
    /// them.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "execute" => Some(Self::Execute),
            "open_with" => Some(Self::OpenWith),
            "view" => Some(Self::View),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }

    /// The choice a ring index names.
    const fn from_control(control: usize) -> Self {
        match control {
            EXECUTE => Self::Execute,
            OPEN_WITH => Self::OpenWith,
            VIEW => Self::View,
            // `Cancel` and anything out of range: the safe answer is the one
            // that does nothing.
            _ => Self::Cancel,
        }
    }
}

impl std::fmt::Display for ExecuteChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.encode())
    }
}

/// the `execute = "ask"` prompt.
#[derive(Debug)]
pub struct ExecuteDialog {
    name: String,
    size: String,
    kind: String,
    ring: crate::dialog::FocusRing,
}

impl ExecuteDialog {
    /// `kind` comes from [`crate::ops::open::kind_of`], never from the name
    /// (invariant I16).
    ///
    /// `cfg` is the panel's, so the size is spelled the way the same file's
    /// size column spells it - one `panel.size_style` for the whole program.
    pub fn new(name: String, size: u64, kind: String, cfg: &PanelConfig) -> Self {
        let mut ring = crate::dialog::FocusRing::new(MNEMONICS.len());
        // See the module documentation: the prompt opens on the button that
        // does nothing.
        ring.set(CANCEL);
        Self {
            name,
            size: bytes_text(size, cfg),
            kind,
            ring,
        }
    }

    /// The file this is asking about.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the content says it is.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Which button has focus, as a choice rather than an index.
    pub const fn focused(&self) -> ExecuteChoice {
        ExecuteChoice::from_control(self.ring.index())
    }

    /// The three lines above the buttons.
    fn lines(&self) -> [String; 3] {
        [
            self.name.clone(),
            format!("Size: {}", self.size),
            format!("Type: {}", self.kind),
        ]
    }

    /// The buttons, in ring order, with their letters.
    fn buttons(&self) -> [(&'static str, Option<char>); 4] {
        [
            ("Execute", Some('e')),
            ("Open with...", Some('o')),
            ("View (F3)", Some('v')),
            ("Cancel", Some('n')),
        ]
    }
}

impl Accelerated for ExecuteDialog {
    /// Ring indices rather than an enum: four controls is under
    /// the five-control floor, and the ring already
    /// indexes them.
    type Control = usize;

    fn mnemonics(&self) -> &'static [(usize, char)] {
        MNEMONICS
    }

    /// Every control here is a button, so every letter presses one.
    /// Nothing in this dialog is a checkbox, so nothing can be turned
    /// off by a mnemonic - the rule is satisfied trivially.
    fn accel(&self, _control: usize) -> Accel<usize> {
        Accel::Press
    }

    fn focus_control(&mut self, control: usize) {
        self.ring.set(control);
    }

    /// Nothing here is a checkbox.
    fn switch_on(&mut self, _control: usize) {}

    fn press(&mut self, control: usize) -> DialogOutcome {
        let choice = ExecuteChoice::from_control(control);
        DialogOutcome::Accept(DialogResult::Text(choice.encode().to_string()))
    }
}

impl Dialog for ExecuteDialog {
    fn id(&self) -> DialogId {
        DialogId::Execute
    }

    fn title(&self) -> String {
        "Execute?".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        // The buttons are the floor: `[ Execute ]  [ Open with... ]
        // [ View (F3) ]  [ Cancel ]` is 55 columns, and a narrower dialog drops
        // the last of them rather than wrapping (`draw_mnemonic_buttons`), so
        // the hint asks for room the name can also live in.
        let widest = self
            .lines()
            .iter()
            .map(|line| crate::ui::text::width(line))
            .max()
            .unwrap_or(0)
            .saturating_add(4);
        let want = u16::try_from(widest).unwrap_or(u16::MAX).max(58);
        (want.min(96), 7)
    }

    /// Four buttons and four letters.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action`.
        //
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return DialogOutcome::Accept(DialogResult::Text(
                ExecuteChoice::Cancel.encode().to_string(),
            ));
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
            // The button says `View (F3)`, so `F3` presses it. A label that
            // names a key and does not answer to it is a label that lies.
            KeyCode::F(3) => self.press(VIEW),
            _ if key.is_accept() => self.press(self.ring.index()),
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let buttons = self.buttons();
        // the design makes 60x15 a supported size, and all four of the design
        // 7.6.1's buttons are 56 columns on one row - two more than a 60-column
        // terminal leaves inside the border. `draw_mnemonic_buttons` drops what
        // does not fit, and the button it would drop is `Cancel`, so below that
        // width they go on two rows rather than off the edge. Shortening a
        // label instead would cost `View (F3)`'s key or `Open with...`'s
        // ellipsis, and the design names both.
        let one_row = crate::dialog::mnemonic_buttons_width(&buttons);
        let rows: u16 = if one_row <= usize::from(area.width) {
            1
        } else {
            2
        };
        let body = style.body();
        let text_rows = area.height.saturating_sub(rows);
        for (offset, line) in self.lines().iter().enumerate() {
            let index = u16::try_from(offset).unwrap_or(u16::MAX);
            if index >= text_rows {
                // The button rows are never overwritten, however short the
                // dialog was clamped to: a prompt with no way to answer it is
                // worse than one that does not name the file.
                break;
            }
            let Some(rect) = super::row(area, index) else {
                break;
            };
            draw_text(f, rect, line, body, style.ascii);
        }
        let focus = self.ring.index();
        if rows == 1 {
            let row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
            draw_mnemonic_buttons(f, row, &buttons, focus, style);
            return;
        }
        let (first, second) = buttons.split_at(2);
        let top = Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 1);
        let bottom = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        // `usize::MAX` is "no button on this row has focus", which is what
        // keeps exactly one of the four highlighted across the two rows.
        draw_mnemonic_buttons(f, top, first, focus, style);
        draw_mnemonic_buttons(f, bottom, second, focus.wrapping_sub(2), style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::{KeyModifiers, KeyPress};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    fn dialog() -> ExecuteDialog {
        ExecuteDialog::new(
            "deploy.sh".to_string(),
            1234,
            "shell script".to_string(),
            &PanelConfig::default(),
        )
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    fn answer(outcome: &DialogOutcome) -> Option<ExecuteChoice> {
        match outcome {
            DialogOutcome::Accept(DialogResult::Text(text)) => ExecuteChoice::parse(text),
            _ => None,
        }
    }

    fn screen(d: &ExecuteDialog, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &style);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).map_or(" ", ratatui::buffer::Cell::symbol))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_choice_survives_a_round_trip_through_text() {
        for choice in [
            ExecuteChoice::Execute,
            ExecuteChoice::OpenWith,
            ExecuteChoice::View,
            ExecuteChoice::Cancel,
        ] {
            assert_eq!(ExecuteChoice::parse(choice.encode()), Some(choice));
        }
        assert_eq!(ExecuteChoice::parse("background 3"), None);
    }

    #[test]
    fn the_prompt_names_the_file_its_size_and_what_it_actually_is() {
        // the own sentence, asserted off the drawn buffer.
        let out = screen(&dialog(), 80, 24);
        assert!(out.contains("deploy.sh"), "{out}");
        assert!(out.contains("shell script"), "{out}");
        assert!(out.contains("Size:"), "{out}");
        for label in ["Execute", "Open with", "View (F3)", "Cancel"] {
            assert!(out.contains(label), "{label} missing:\n{out}");
        }
    }

    #[test]
    fn it_opens_on_cancel_so_a_second_enter_runs_nothing() {
        // "`Enter` is the key people press to navigate, and the
        // cost of an accidental execution is unbounded."
        let mut d = dialog();
        assert_eq!(d.focused(), ExecuteChoice::Cancel);
        assert_eq!(
            answer(&d.handle_key(&key(KeyCode::Enter))),
            Some(ExecuteChoice::Cancel)
        );
    }

    #[test]
    fn esc_cancels_whichever_button_has_focus() {
        let mut d = dialog();
        d.focus_control(EXECUTE);
        assert_eq!(
            answer(&d.handle_key(&key(KeyCode::Esc))),
            Some(ExecuteChoice::Cancel)
        );
    }

    #[test]
    fn the_arrows_walk_the_four_buttons_and_enter_presses_one() {
        let mut d = dialog();
        d.focus_control(EXECUTE);
        assert_eq!(d.focused(), ExecuteChoice::Execute);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Right)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.focused(), ExecuteChoice::OpenWith);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Left)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.focused(), ExecuteChoice::Execute);
        assert_eq!(
            answer(&d.handle_key(&key(KeyCode::Enter))),
            Some(ExecuteChoice::Execute)
        );
        // `Tab` walks the same ring, and wraps.
        let mut d = dialog();
        d.focus_control(EXECUTE);
        for _ in 0..4 {
            assert!(matches!(
                d.handle_key(&key(KeyCode::Tab)),
                DialogOutcome::Consumed
            ));
        }
        assert_eq!(d.focused(), ExecuteChoice::Execute);
    }

    #[test]
    fn f3_presses_the_button_that_names_it() {
        let mut d = dialog();
        assert_eq!(
            answer(&d.handle_key(&key(KeyCode::F(3)))),
            Some(ExecuteChoice::View)
        );
    }

    #[test]
    fn every_button_answers_to_its_alt_letter() {
        // and the focus moves to the
        // button before it is pressed.
        for (letter, want) in [
            ('e', ExecuteChoice::Execute),
            ('o', ExecuteChoice::OpenWith),
            ('v', ExecuteChoice::View),
            ('n', ExecuteChoice::Cancel),
        ] {
            let mut d = dialog();
            assert_eq!(answer(&d.handle_key(&alt(letter))), Some(want));
            assert_eq!(d.focused(), want, "focus moved first");
        }
        let mut d = dialog();
        assert_eq!(d.mnemonic_letters(), vec!['e', 'o', 'v', 'n']);
        assert!(matches!(d.handle_key(&alt('z')), DialogOutcome::Ignored));
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // read off the buffer, not off the
        // table.
        let d = dialog();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, &d, f.area(), &style);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let mut got: Vec<char> = Vec::new();
        for y in 0..24 {
            for x in 0..80 {
                let Some(cell) = buf.cell((x, y)) else {
                    continue;
                };
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    got.extend(cell.symbol().chars().map(|c| c.to_ascii_lowercase()));
                }
            }
        }
        let mut want = d.mnemonic_letters();
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want, "underlines on screen");
    }

    #[test]
    fn it_draws_at_every_size_the_spec_declares_usable() {
        // invariant I20: 60x15 is a supported size, and both border
        // modes have to render without a zero-dimension rectangle.
        let d = dialog();
        for ascii in [false, true] {
            let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
            for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
                let backend = TestBackend::new(w, h);
                let mut terminal = Terminal::new(backend).expect("test terminal");
                terminal
                    .draw(|f| {
                        crate::dialog::draw(f, &d, f.area(), &style);
                    })
                    .expect("draw");
                let buf = terminal.backend().buffer().clone();
                let out: String = (0..h)
                    .map(|y| {
                        (0..w)
                            .map(|x| buf.cell((x, y)).map_or(" ", ratatui::buffer::Cell::symbol))
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(out.contains("Cancel"), "{w}x{h} ascii={ascii}:\n{out}");
            }
        }
    }
}
