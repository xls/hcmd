//! Selection by mask - the `+` and `-` prompts.
//!
//! ```text
//!        ┌──────────── Select by mask ─────────────┐
//!        │ Mark files matching:                    │
//!        │ *.rs                                    │
//!        │ Match:  (•) Wildcard  ( ) Regex         │
//!        │            [ OK ]   [ Cancel ]          │
//!        └─────────────────────────────────────────┘
//! ```
//!
//! Two halves live here, because they are the two halves of one feature:
//!
//! * [`MaskDialog`] - the prompt itself, "opens a mask prompt, default `*`",
//!   with the mode switch the design asks for ("can be switched to regex in
//!   the prompt").
//! * [`apply`] - what the answer does to a [`Tab`]'s marks.
//!
//! The matcher is not here: it is [`crate::ops::mask`], because the copy/move
//! dialog's "Only files of this type" is the same wildcard
//! language and there must be exactly one implementation of it.
//!
//! # The regex switch works, since v0.6
//!
//! the design offers regex in this prompt. A regex engine is a dependency,
//! and v0.2 had none, so the control was drawn and refused with the milestone
//! that would bring it. the search
//! brings `grep-regex`, which is that engine, so the switch now switches and
//! [`apply`] compiles the mask in whichever language the prompt was left in
//! ([`crate::ops::mask::compile`]). A silently-wildcard "Regex" radio button
//! would have been the one outcome worse than not offering it, which is why
//! the refusal came first and the feature second.
//!
//!
//! # Why a dialog lives under `panel/`
//!
//! Because marking is panel state and this dialog exists only to change it;
//! [`apply`] is the substance and the prompt is its front end. Everything the
//! framework needs is in [`crate::dialog`], and this file only implements the
//! [`Dialog`] trait - the same way [`crate::dialog::InputDialog`] does, which
//! is what `F7` uses.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::Tab;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    Piece, draw_mnemonic, draw_mnemonic_buttons, draw_mnemonic_pieces, draw_text, letter_of,
};
use crate::input::{Action, CommandLine, DialogId, KeyCode};
use crate::ops::mask::{MaskMode, compile};

/// One focusable control, in `Tab` order.
///
/// An enum rather than the five `usize` constants it replaces, because
/// the design puts the floor at five controls: at five,
/// [`MaskDialog::accel`] is an exhaustive `match` and a control added later
/// cannot be forgotten there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The mask field.
    Field,
    /// The `Wildcard` / `Regex` switch.
    Mode,
    /// The `Exclude directories` checkbox.
    Exclude,
    /// The `OK` button.
    Ok,
    /// The `Cancel` button.
    Cancel,
}

/// Every control, in `Tab` order. The ring's length is this.
const CONTROLS: &[Control] = &[
    Control::Field,
    Control::Mode,
    Control::Exclude,
    Control::Ok,
    Control::Cancel,
];

/// The mode switch's own label, and the string the underline is
/// searched in (a mnemonic is scoped to its own
/// piece, never to the whole composed row).
const MODE_LABEL: &str = "Match:";

/// The `Exclude directories` checkbox's label.
const EXCLUDE_LABEL: &str = "Exclude directories";

/// the `Alt` mnemonics for this dialog.
///
/// `o` and `n` are the program-wide `OK` and `Cancel`. The field has no
/// letter: its label is caller-supplied (`Mark files matching:` or `Unmark
/// files matching:`) and a `const` table cannot name a letter of a string it
/// has never seen. The dialog opens with the field focused, so nothing is out
/// of reach and `Tab` is the way back to it.
///
/// `Wildcard` and `Regex` are the radio's two values and are not separately
/// accelerated: the design gives no meaning to "accelerate a three-way
/// control" and selecting one option turns the other off.
pub const MNEMONICS: &[(Control, char)] = &[
    (Control::Mode, 'm'),
    (Control::Exclude, 'e'),
    (Control::Ok, 'o'),
    (Control::Cancel, 'n'),
];

/// Separates the `Exclude directories` flag from the mask in the prompt's
/// answer.
///
/// [`crate::dialog::DialogResult::Text`] carries one string and
/// the design fixes the variants, so the checkbox travels in
/// front of the mask. A tab is the separator because `Tab` moves between
/// controls and can therefore never be typed into the field - there is no mask
/// this can misread.
const ANSWER_SEPARATOR: char = '\t';

/// Build the prompt's answer: the checkbox, then the mask.
pub fn encode_answer(mask: &str, exclude_dirs: bool) -> String {
    format!("{}{ANSWER_SEPARATOR}{mask}", u8::from(exclude_dirs))
}

/// Read one back. A string with no separator is a bare mask with the checkbox
/// off, which is what every other route into [`apply`] means.
pub fn decode_answer(answer: &str) -> (String, bool) {
    match answer.split_once(ANSWER_SEPARATOR) {
        Some((flag, mask)) => (mask.to_string(), flag == "1"),
        None => (answer.to_string(), false),
    }
}

/// What one `+` or `-` did.
///
/// `matched` and `changed` differ whenever a mask catches something that was
/// already in the wanted state - `+ *.rs` twice matches the same files and
/// changes nothing the second time - which is the difference between "nothing
/// matched" and "nothing left to mark", and the status line says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaskOutcome {
    /// How many entries the mask matched. `..` is never one of them.
    pub matched: usize,
    /// How many marks actually changed.
    pub changed: usize,
}

impl MaskOutcome {
    /// The status-line sentence for this outcome.
    pub fn message(&self, mask: &str, mark: bool) -> String {
        let verb = if mark { "marked" } else { "unmarked" };
        if self.matched == 0 {
            return format!("{mask}: nothing matched");
        }
        if self.changed == 0 {
            return format!(
                "{mask}: {} already {verb}",
                plural(self.matched, "entry", "entries")
            );
        }
        format!(
            "{mask}: {} {verb}",
            plural(self.changed, "entry", "entries")
        )
    }
}

/// `1 entry` / `2 entries`, written once.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Mark (or unmark) every entry in `tab` whose **name** matches `mask`.
///
/// * `..` is never marked, in either direction, the way it is never counted.
///
/// * `exclude_dirs` is the checkbox: "a mask is nearly always
///   about files - `*.jpg` is meant to catch photographs, not a directory
///   someone named `holiday.jpg` - but not always, and which one you meant is
///   not recoverable from the pattern". Default **off**, so `*` still means
///   everything. Marking a directory here never walks it - only `Space` and
///   `Ctrl+L` size anything.
/// * Matching is case-insensitive and understands lists and a `|` exclusion,
///   because [`crate::ops::mask::matches`] does; a regex mask is
///   case-insensitive and unanchored for the same reason.
///
///
/// Returns `Err` with the reason when the mask cannot be compiled - a bad
/// regular expression, or a mode this build cannot honour. Refusing beats
/// matching something other than what was typed.
///
/// The mask is compiled **once** and matched per row, rather than parsed per
/// row: a regex over a directory of ten thousand entries is one build and ten
/// thousand searches.
pub fn apply(
    tab: &mut Tab,
    mask: &str,
    mode: MaskMode,
    mark: bool,
    exclude_dirs: bool,
) -> Result<MaskOutcome, String> {
    if let Some(why) = mode.unavailable() {
        return Err(why.to_string());
    }
    let compiled = compile(mask, mode).map_err(|e| e.to_string())?;
    let mut out = MaskOutcome::default();
    // Split the borrow: the marks are mutated while the entries are read, and
    // they are two fields of the same `Tab`.
    let Tab { entries, marks, .. } = tab;
    for entry in entries.iter() {
        if entry.is_parent || (exclude_dirs && entry.is_dir()) || !compiled.matches(&entry.name) {
            continue;
        }
        out.matched = out.matched.saturating_add(1);
        // Keyed on `Entry::mark_key`, like every other mark operation: on a
        // virtual listing two rows can share a name, and a mask that matched
        // one of them must not mark the other.
        let key = entry.mark_key();
        let changed = if mark {
            marks.insert(key.into_owned())
        } else {
            marks.remove(key.as_ref())
        };
        if changed {
            out.changed = out.changed.saturating_add(1);
        }
    }
    Ok(out)
}

/// The `+` / `-` mask prompt.
///
/// A [`CommandLine`] field, a mode switch, `OK` and `Cancel`. The field is a
/// `CommandLine` for the reason the design gives: it already
/// holds the invariant that the caret is a *character* index and can never
/// slice a `String` at a non-boundary, plus word kill, line kill and a
/// display-width caret column that a CJK filename does not break.
pub struct MaskDialog {
    id: DialogId,
    title: String,
    label: String,
    line: CommandLine,
    ring: FocusRing,
    mode: MaskMode,
    /// the `Exclude directories`. Sticky for the session, which
    /// is why it arrives from, and goes back to, [`crate::app::App`].
    exclude_dirs: bool,
    /// Set when the user asked for a mode this build cannot honour, so the
    /// refusal is a fact the dialog can be asked about and not only pixels.
    notice: Option<&'static str>,
    /// The remembered mask has not been touched yet, so the first printable key
    /// replaces it. Stands in for a text selection, which
    /// [`CommandLine`] does not have.
    fresh: bool,
}

impl MaskDialog {
    /// A prompt for one side of the pair.
    ///
    /// `initial` is the mask offered as the default: `*` the first time, and
    /// afterwards "the last mask … remembered per session"
    /// ([`History::last`]). The caret starts at its end, so
    /// `-` `Enter` repeats the last mask with two keys.
    pub fn new(id: DialogId, initial: impl Into<String>) -> Self {
        let (title, label) = match id {
            DialogId::UnselectMask => ("Unselect by mask", "Unmark files matching:"),
            // `SelectMask`, and anything else that reuses this prompt.
            _ => ("Select by mask", "Mark files matching:"),
        };
        let mut line = CommandLine::new();
        line.set_text(initial);
        line.move_end();
        Self {
            id,
            title: title.to_string(),
            label: label.to_string(),
            line,
            ring: FocusRing::new(CONTROLS.len()),
            mode: MaskMode::default(),
            exclude_dirs: false,
            notice: None,
            fresh: true,
        }
    }

    /// Open with the session's `Exclude directories` setting (
    /// "tick it once and it stays ticked").
    #[must_use]
    pub const fn excluding_dirs(mut self, exclude: bool) -> Self {
        self.exclude_dirs = exclude;
        self
    }

    /// Whether directories are excluded from the match.
    pub const fn exclude_dirs(&self) -> bool {
        self.exclude_dirs
    }

    /// The mask as typed.
    pub fn text(&self) -> &str {
        self.line.text()
    }

    /// The caret, as a character index.
    pub fn caret(&self) -> usize {
        self.line.caret()
    }

    /// Which control has focus.
    pub fn focused(&self) -> Control {
        CONTROLS
            .get(self.ring.index())
            .copied()
            .unwrap_or(Control::Field)
    }

    /// The selected mode. Never [`MaskMode::Regex`] in this build.
    pub const fn mode(&self) -> MaskMode {
        self.mode
    }

    /// The refusal shown under the mode switch, once one has been earned.
    pub const fn notice(&self) -> Option<&'static str> {
        self.notice
    }

    /// True when the mask may be accepted. An empty mask is refused: `*` is
    /// one keystroke away and is what "everything" is spelled as, so an empty
    /// field is a slip rather than an instruction.
    pub fn is_acceptable(&self) -> bool {
        !self.line.text().trim().is_empty()
    }

    /// What `OK` does, whether it was reached by `Enter`, by `Tab` and
    /// `Space`, or by the `Alt+O`.
    ///
    /// One body and not three: [`Accelerated::press`] is the only route to a
    /// button, so a third route cannot come to mean something different from
    /// the first two.
    fn accept(&self) -> DialogOutcome {
        if !self.is_acceptable() {
            return DialogOutcome::Consumed;
        }
        DialogOutcome::Accept(DialogResult::Text(encode_answer(
            self.line.text(),
            self.exclude_dirs,
        )))
    }

    /// `Space` / `Left` / `Right` on the mode switch.
    ///
    /// Switching *to* an unavailable mode does not switch: it records the
    /// reason and leaves the mode alone.
    fn toggle_mode(&mut self) {
        let wanted = match self.mode {
            MaskMode::Wildcard => MaskMode::Regex,
            MaskMode::Regex => MaskMode::Wildcard,
        };
        match wanted.unavailable() {
            Some(why) => self.notice = Some(why),
            None => {
                self.mode = wanted;
                self.notice = None;
            }
        }
    }

    /// The mode row, one piece per label, so the unavailable option can be
    /// dimmed and so `Match:` carries its own `Alt+M` underline.
    ///
    /// Three pieces and not one string: the letter is searched in
    /// the control's own label, never in the whole composed row.
    /// Searched as one string, the `m` of
    /// `Match:` would still win here, but the rule is the rule everywhere and
    /// a row that followed it only by luck is a row that stops following it
    /// when a word is added.
    fn mode_pieces(&self, style: &DialogStyle) -> Vec<Piece> {
        let on = if style.ascii { "(*)" } else { "(\u{2022})" };
        let off = "( )";
        let focused = self.focused() == Control::Mode;
        let wildcard = self.mode == MaskMode::Wildcard;
        let selected = |is: bool| {
            if is {
                style.button(focused)
            } else {
                style.body()
            }
        };
        vec![
            Piece::new(
                MODE_LABEL,
                letter_of(MNEMONICS, Control::Mode),
                style.body(),
                false,
            ),
            Piece::new(
                format!("{} Wildcard", if wildcard { on } else { off }),
                // The radio's two values are not separately accelerated:
                // selecting one turns the other
                // off, and the accelerator never turns anything off.
                None,
                selected(wildcard),
                // One control drawn as a heading and two values. The piece a
                // narrow row has to keep is the value that is *on*, because
                // that is the one that says what the control is set to.
                focused && wildcard,
            ),
            Piece::new(
                format!("{} Regex", if wildcard { off } else { on }),
                None,
                // Drawn, and visibly not the selected one.
                selected(!wildcard).add_modifier(Modifier::DIM),
                focused && !wildcard,
            ),
        ]
    }
}

impl Accelerated for MaskDialog {
    type Control = Control;

    fn mnemonics(&self) -> &'static [(Control, char)] {
        MNEMONICS
    }

    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            // The mode switch is a radio and is focus-only.
            // `Field` has no letter and so is
            // never reached through here; it answers `Focus` because that is
            // what it would be if it ever gained one.
            Control::Field | Control::Mode => Accel::Focus,
            Control::Exclude => Accel::Check,
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
            Control::Exclude => self.exclude_dirs = true,
            Control::Field | Control::Mode | Control::Ok | Control::Cancel => {}
        }
    }

    fn press(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Ok => self.accept(),
            Control::Cancel => DialogOutcome::Cancel,
            Control::Field | Control::Mode | Control::Exclude => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for MaskDialog {
    fn id(&self) -> DialogId {
        self.id
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = crate::ui::text::width(&self.label)
            .max(crate::ui::text::width(&self.title))
            .max(crate::ui::text::width(self.line.text()))
            .max(MaskMode::Regex.unavailable().map_or(0, |w| w.len()));
        let w = u16::try_from(widest.saturating_add(8)).unwrap_or(u16::MAX);
        // Label, field, mode, checkbox, notice, buttons, two borders.
        (w.clamp(44, 76), 8)
    }

    /// Four letters. The field has none: its label is caller-supplied and a
    /// `const` table cannot name a letter of a string it has never seen.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action` or reaches the
        // field, so a global `Alt` binding of
        // the design cannot pre-empt a mnemonic.
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
            return match self.focused() {
                Control::Cancel => self.press(Control::Cancel),
                Control::Field | Control::Mode | Control::Exclude | Control::Ok => {
                    self.press(Control::Ok)
                }
            };
        }

        if self.focused() == Control::Mode {
            return match key.press.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                    self.toggle_mode();
                    DialogOutcome::Consumed
                }
                KeyCode::Up => {
                    self.focus_control(Control::Field);
                    DialogOutcome::Consumed
                }
                KeyCode::Down => {
                    self.focus_control(Control::Exclude);
                    DialogOutcome::Consumed
                }
                _ => DialogOutcome::Ignored,
            };
        }

        // the checkbox.
        if self.focused() == Control::Exclude {
            return match key.press.code {
                KeyCode::Char(' ') => {
                    self.exclude_dirs = !self.exclude_dirs;
                    DialogOutcome::Consumed
                }
                KeyCode::Up => {
                    self.focus_control(Control::Mode);
                    DialogOutcome::Consumed
                }
                KeyCode::Down => {
                    self.focus_control(Control::Ok);
                    DialogOutcome::Consumed
                }
                _ => DialogOutcome::Ignored,
            };
        }

        // On a button, Left/Right walk the buttons rather than editing.
        if self.focused() != Control::Field {
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
                    self.focus_control(Control::Exclude);
                    DialogOutcome::Consumed
                }
                _ => DialogOutcome::Ignored,
            };
        }

        // A binding resolved in the `dialog` context wins over the raw key
        // (the design steps 1-3), so a rebound `kill_line` still works.
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
            KeyCode::Down => self.focus_control(Control::Mode),
            KeyCode::Insert => self.line.overwrite = !self.line.overwrite,
            _ => match key.text() {
                Some(c) => {
                    // The remembered mask arrives **preselected**, the same rule
                    // the target field follows: the first thing typed replaces it
                    // rather than appending to it, so a new mask is typed straight
                    // in and the old one is repeated with `Enter` alone. Appending
                    // turned `*` plus `*.jpg` into `**.jpg`.
                    //
                    if std::mem::take(&mut self.fresh) {
                        self.line.set_text("");
                    }
                    self.line.insert_char(c);
                }
                None => return DialogOutcome::Ignored,
            },
        }
        // Any deliberate edit means the user is working with what is there, so
        // the preselection is spent.
        self.fresh = false;
        DialogOutcome::Consumed
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let row = |n: u16| Rect::new(area.x, area.y.saturating_add(n), area.width, 1);
        draw_text(f, row(0), &self.label, style.body(), style.ascii);

        if area.height >= 2 {
            let field = row(1);
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

        // The mode switch and its refusal are the first things given up on a
        // short terminal; the field and the buttons are not.
        if area.height >= 4 {
            draw_mnemonic_pieces(f, row(2), &self.mode_pieces(style), style.body());
        }
        if area.height >= 5 {
            // The box is a mark and the label is what follows it, which is
            // where the `e` is underlined: `split_mnemonic` is
            // what tells the two apart, so a ticked box cannot take the
            // underline off a label.
            let label = crate::ui::dialog::checkbox(EXCLUDE_LABEL, self.exclude_dirs, style.ascii);
            let button = style.button(self.focused() == Control::Exclude);
            match letter_of(MNEMONICS, Control::Exclude) {
                Some(letter) => draw_mnemonic(f, row(3), &label, letter, button, style.ascii),
                None => draw_text(f, row(3), &label, button, style.ascii),
            }
        }
        if area.height >= 6
            && let Some(why) = MaskMode::Regex.unavailable()
        {
            let mut text = style.body();
            if self.notice.is_some() {
                // The user just asked for it; say so louder than a footnote.
                text = text.add_modifier(Modifier::BOLD);
            } else {
                text = text.add_modifier(Modifier::DIM);
            }
            draw_text(f, row(4), why, text, style.ascii);
        }

        if area.height >= 3 {
            let buttons = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
            let focus = match self.focused() {
                Control::Ok => 0,
                Control::Cancel => 1,
                // Not on a button at all, which no index is.
                Control::Field | Control::Mode | Control::Exclude => usize::MAX,
            };
            draw_mnemonic_buttons(
                f,
                buttons,
                &[
                    ("OK", letter_of(MNEMONICS, Control::Ok)),
                    ("Cancel", letter_of(MNEMONICS, Control::Cancel)),
                ],
                focus,
                style,
            );
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if self.focused() != Control::Field || area.width == 0 || area.height < 2 {
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

impl std::fmt::Debug for MaskDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaskDialog")
            .field("id", &self.id)
            .field("text", &self.line.text())
            .field("mode", &self.mode)
            .field("focused", &self.ring.index())
            .finish()
    }
}

/// The three answers a mask prompt needs in order to reopen where it was left.
///
///
/// The mask last typed, the masks offered in the drop-down, and whether a mask
/// is meant to apply to directories. All three are sticky for the session and
/// none of them is configuration: they are what the user last said, offered
/// back so that "mark these, then copy those" is two keystrokes rather than
/// two retypings.
///
/// The last mask and the exclude-directories flag are shared between `+` and
/// `-` deliberately, exactly as the mask is: the two keys ask the same
/// question with opposite answers.
///
/// Named [`History`] to match [`crate::search::saved::History`], which is the
/// same idea for the same reason one module over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// The last wildcard mask offered by `+` / `-`.
    pub last: String,
    /// This session's "Only files of this type" masks, newest first, behind
    /// the copy dialog's `+ F8`.
    pub offered: Vec<String>,
    /// The `Exclude directories` checkbox on the `+` / `-` prompt.
    /// Default off.
    pub exclude_dirs: bool,
}

impl Default for History {
    /// `*`, nothing offered yet, directories included.
    fn default() -> Self {
        Self {
            last: "*".to_string(),
            offered: Vec::new(),
            exclude_dirs: false,
        }
    }
}

impl History {
    /// Put a mask at the head of the drop-down, if it is worth offering and is
    /// not already there.
    ///
    /// An empty mask is not a mask, and a duplicate would push the list's
    /// oldest entry out to say nothing new.
    pub fn offer(&mut self, mask: &str) {
        if mask.is_empty() || self.offered.iter().any(|m| m == mask) {
            return;
        }
        self.offered.insert(0, mask.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::{Config, Keymap, Theme};
    use crate::input::{KeyModifiers, KeyPress};
    use crate::ops::{SizeCache, TreeStats};
    use crate::panel::format::status_text;
    use crate::vfs::{Entry, VfsPath};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn typed(d: &mut MaskDialog, text: &str) {
        for c in text.chars() {
            d.handle_key(&key(KeyCode::Char(c)));
        }
    }

    /// `Alt`+letter, the keystroke the design is about.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    /// Every character drawn with [`Modifier::UNDERLINED`], folded to lower
    /// case. the underline, read off the buffer rather than off the
    /// table, so a declared letter with no paint behind it fails.
    fn underlined(d: &MaskDialog, w: u16, h: u16) -> Vec<char> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), crate::config::ColorDepth::TrueColor, false);
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
                    out.extend(cell.symbol().chars().map(|c| c.to_ascii_lowercase()));
                }
            }
        }
        out
    }

    #[test]
    fn every_letter_names_a_control_this_dialog_can_reach() {
        // control by control. The mode switch is a radio and is
        // focus-only: the design gives no meaning to accelerating a
        // three-way control, and both candidate meanings turn the other
        // option off.
        let mut d = MaskDialog::new(DialogId::SelectMask, "*.rs");
        let opening = d.mode();
        d.handle_key(&alt('m'));
        assert_eq!(d.focused(), Control::Mode, "Alt+M");
        assert_eq!(d.mode(), opening, "the radio was focused, not stepped");
        d.handle_key(&alt('e'));
        assert_eq!(d.focused(), Control::Exclude, "Alt+E");

        let mut d = MaskDialog::new(DialogId::SelectMask, "*.rs");
        match d.handle_key(&alt('o')) {
            DialogOutcome::Accept(DialogResult::Text(answer)) => {
                assert_eq!(decode_answer(&answer), ("*.rs".to_string(), false));
            }
            other => panic!("Alt+O pressed OK, got {other:?}"),
        }

        let mut d = MaskDialog::new(DialogId::SelectMask, "*.rs");
        assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));

        // Every letter is in the ring, and `Field` is the one control this
        // dialog deliberately leaves without one.
        for (control, letter) in MNEMONICS {
            assert!(letter.is_ascii_lowercase(), "{control:?}: stored folded");
            assert!(CONTROLS.contains(control), "{control:?} is not in the ring");
        }
        assert_eq!(
            MNEMONICS.len(),
            CONTROLS.len().saturating_sub(1),
            "four letters for five controls: the field's label is the caller's"
        );
    }

    #[test]
    fn a_mnemonic_never_turns_the_checkbox_off() {
        // "a key that enabled on the way in and disabled on the
        // way back would make a repeated keystroke destructive."
        // the design I6.
        let mut d = MaskDialog::new(DialogId::SelectMask, "*.rs");
        assert!(!d.exclude_dirs());
        d.handle_key(&alt('e'));
        assert!(d.exclude_dirs(), "Alt+E ticked it");
        d.handle_key(&alt('e'));
        assert!(d.exclude_dirs(), "Alt+E again left it ticked");
        assert_eq!(d.focused(), Control::Exclude, "and the focus is on it");
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(!d.exclude_dirs(), "and Space is the toggle");
    }

    #[test]
    fn a_mnemonic_never_types_into_the_mask() {
        // the design I8. The mnemonic check runs before the field
        // ever sees the key, and `DialogKey::text` is `None` under `ALT`, so
        // neither the mask nor the caret moves - including for the letters
        // this dialog does not claim.
        let mut d = MaskDialog::new(DialogId::SelectMask, "*.rs");
        let caret = d.caret();
        for letter in ['m', 'e', 'o', 'n', 'q', 'z'] {
            d.handle_key(&alt(letter));
            assert_eq!(d.text(), "*.rs", "Alt+{letter} typed into the mask");
        }
        d.handle_key(&alt('m'));
        assert_eq!(d.caret(), caret, "and the caret never moved");
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // the design I3: the set of underlined cells is exactly the set of
        // declared letters, read off the buffer and not off the table.
        // `Match:` is its own piece, so its `m` cannot land in the mask above
        // it or in `Mark files matching:`.
        let d = MaskDialog::new(DialogId::SelectMask, "*.rs");
        let mut want = d.mnemonic_letters();
        let mut got = underlined(&d, 80, 12);
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want, "underlines on screen");
    }

    fn tab() -> Tab {
        let mut tab = Tab::new(VfsPath::local("/root"));
        tab.entries = vec![
            Entry::parent_entry(),
            Entry::dir("src"),
            Entry::dir("target"),
            Entry::file("main.rs"),
            Entry::file("lib.rs"),
            Entry::file("Cargo.toml"),
            Entry::file("Makefile"),
            Entry::file("notes.bak"),
        ];
        tab
    }

    fn marks(tab: &Tab) -> Vec<String> {
        let mut names: Vec<String> = tab.marks.iter().cloned().collect();
        names.sort();
        names
    }

    // ------------------------------------------------------- the matcher ----

    #[test]
    fn the_matcher_answers_a_table_of_patterns_and_names() {
        // the own examples first, then the edges that decide
        // whether a mask does what the person typing it meant.
        let table: &[(&str, &str, bool)] = &[
            ("*.rs", "main.rs", true),
            ("*.rs", "main.rs.bak", false),
            ("img_*.jpg", "img_001.jpg", true),
            ("img_*.jpg", "IMG_001.JPG", true),
            ("img_*.jpg", "photo.jpg", false),
            // `?` is exactly one character, never zero and never two.
            ("?.txt", "a.txt", true),
            ("?.txt", ".txt", false),
            ("?.txt", "ab.txt", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("?", "\u{e9}", true),
            // A name with a dot and no extension, and one with neither.
            ("*.", "notes.", true),
            ("*.*", "Makefile", true),
            ("*", "Makefile", true),
            ("Makefile", "makefile", true),
            ("*.*", ".bashrc", true),
            ("*.gz", "a.tar.gz", true),
            ("*.tar", "a.tar.gz", false),
            // An empty mask matches everything rather than nothing: it is what
            // an untouched field means, and `is_match_all` says so.
            ("", "anything at all", true),
            ("   ", "anything at all", true),
            // A list, and an exclusion.
            ("*.rs *.toml", "Cargo.toml", true),
            ("*.rs;*.toml", "main.rs", true),
            ("*.rs,*.toml", "notes.bak", false),
            ("*|*.bak", "notes.txt", true),
            ("*|*.bak", "notes.bak", false),
        ];
        for (mask, name, want) in table {
            assert_eq!(
                crate::ops::mask::matches(mask, name),
                *want,
                "mask {mask:?} against {name:?}"
            );
        }
    }

    // ---------------------------------------------------------- `+` / `-` ---

    #[test]
    fn plus_marks_what_matches_and_minus_takes_it_back() {
        let mut t = tab();
        let out = apply(&mut t, "*.rs", MaskMode::Wildcard, true, false).expect("wildcard");
        assert_eq!((out.matched, out.changed), (2, 2));
        assert_eq!(marks(&t), vec!["lib.rs", "main.rs"]);

        // A second `+` with the same mask matches the same files and changes
        // nothing, which the status line distinguishes from "nothing matched".
        let out = apply(&mut t, "*.rs", MaskMode::Wildcard, true, false).expect("wildcard");
        assert_eq!((out.matched, out.changed), (2, 0));
        assert!(out.message("*.rs", true).contains("already marked"));

        let out = apply(&mut t, "main.*", MaskMode::Wildcard, false, false).expect("wildcard");
        assert_eq!((out.matched, out.changed), (1, 1));
        assert_eq!(marks(&t), vec!["lib.rs"]);

        let out = apply(&mut t, "*.zip", MaskMode::Wildcard, true, false).expect("wildcard");
        assert_eq!(out.matched, 0);
        assert_eq!(out.message("*.zip", true), "*.zip: nothing matched");
    }

    #[test]
    fn dot_dot_is_never_marked_by_a_mask() {
        // "`..` is never counted, in either form" - and it is
        // never marked either, whatever the mask.
        let mut t = tab();
        let out = apply(&mut t, "*", MaskMode::Wildcard, true, false).expect("wildcard");
        assert_eq!(out.matched, 7, "everything except `..`");
        assert!(!t.marks.contains(".."));
    }

    #[test]
    fn a_directory_is_matched_by_name_like_anything_else() {
        let mut t = tab();
        apply(&mut t, "src", MaskMode::Wildcard, true, false).expect("wildcard");
        assert_eq!(marks(&t), vec!["src"]);
        // And a mask over an extension does not sweep directories up with it.
        let mut t = tab();
        apply(&mut t, "*.rs", MaskMode::Wildcard, true, false).expect("wildcard");
        assert!(!t.marks.contains("src"));
    }

    #[test]
    fn a_regex_mask_marks_what_the_pattern_matches() {
        // v0.2 refused this mode for want of an engine; the design's
        // `grep-regex` is that engine.
        let mut t = tab();
        let out = apply(&mut t, "^main\\.rs$", MaskMode::Regex, true, false).expect("regex");
        assert_eq!((out.matched, out.changed), (1, 1));
        assert_eq!(marks(&t), vec!["main.rs"]);

        // And a mask that is a glob rather than a regex is refused with its
        // reason instead of matching something else.
        let mut t = tab();
        let err = apply(&mut t, "*.rs", MaskMode::Regex, true, false).expect_err("not a regex");
        assert!(err.contains("regular expression"), "{err}");
        assert!(t.marks.is_empty(), "and nothing was marked meanwhile");
    }

    // ---------------------------------------------------------- the prompt --

    #[test]
    fn the_prompt_offers_the_remembered_mask_with_the_caret_at_its_end() {
        // "the last mask is remembered per session and offered as
        // the default on the next open" - so `-` `Enter` repeats it.
        let mut d = MaskDialog::new(DialogId::UnselectMask, "*.bak");
        assert_eq!(d.text(), "*.bak");
        assert_eq!(d.caret(), 5);
        assert_eq!(d.title(), "Unselect by mask");
        match d.handle_key(&key(KeyCode::Enter)) {
            // The answer carries the checkbox in front of the
            // mask; `decode_answer` is the other half of it.
            DialogOutcome::Accept(DialogResult::Text(answer)) => {
                assert_eq!(decode_answer(&answer), ("*.bak".to_string(), false));
            }
            other => panic!("expected the mask back, got {other:?}"),
        }
    }

    #[test]
    fn typing_over_the_default_then_enter_is_the_whole_interaction() {
        let mut d = MaskDialog::new(DialogId::SelectMask, "*");
        d.handle_key(&key(KeyCode::Backspace));
        typed(&mut d, "img_*.jpg");
        assert_eq!(d.text(), "img_*.jpg");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Accept(DialogResult::Text(_))
        ));
    }

    #[test]
    fn an_empty_mask_is_refused_and_esc_cancels() {
        let mut d = MaskDialog::new(DialogId::SelectMask, "");
        assert!(!d.is_acceptable());
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn the_regex_switch_switches() {
        // the design offers regex here. v0.2 drew the control and refused it
        // for want of an engine; the design brings one, so the mode now
        // changes and there is no notice to show.
        let mut d = MaskDialog::new(DialogId::SelectMask, "*");
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), Control::Mode);
        assert!(d.notice().is_none());

        d.handle_key(&key(KeyCode::Char(' ')));
        assert_eq!(d.mode(), MaskMode::Regex, "the mode changed");
        assert!(d.notice().is_none(), "{:?}", d.notice());

        // And back again, because it is a switch and not a latch.
        d.handle_key(&key(KeyCode::Char(' ')));
        assert_eq!(d.mode(), MaskMode::Wildcard);

        // The switch never eats the mask: the field is untouched.
        assert_eq!(d.text(), "*");
    }

    #[test]
    fn the_exclude_directories_checkbox_is_offered_and_sticky() {
        // default off, toggled with `Space`, and it travels back on
        // the answer so the session can remember it.
        let mut d = MaskDialog::new(DialogId::SelectMask, "*.jpg");
        assert!(
            !d.exclude_dirs(),
            "default off, so `*` still means everything"
        );

        // It opens ticked when the session says so.
        let mut sticky = MaskDialog::new(DialogId::SelectMask, "*.jpg").excluding_dirs(true);
        assert!(sticky.exclude_dirs());
        match sticky.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Text(answer)) => {
                assert_eq!(decode_answer(&answer), ("*.jpg".to_string(), true));
            }
            other => panic!("expected the mask back, got {other:?}"),
        }

        // `Space` on the field types a space; only the checkbox toggles it.
        // It replaces rather than appends, because the remembered mask arrives
        // preselected and a space is text like any other.
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(!d.exclude_dirs());
        assert_eq!(d.text(), " ");
        // A second one appends: the preselection is spent.
        d.handle_key(&key(KeyCode::Char(' ')));
        assert_eq!(d.text(), "  ");
        for _ in 0..2 {
            d.handle_key(&key(KeyCode::Tab));
        }
        assert_eq!(d.focused(), Control::Exclude);
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(d.exclude_dirs());
    }

    #[test]
    fn a_directory_named_like_the_mask_is_left_alone_when_the_box_is_ticked() {
        // the own example: "`*.jpg` is meant to catch
        // photographs, not a directory someone named `holiday.jpg`".
        let mut t = Tab::new(VfsPath::local("/root"));
        t.entries = vec![
            Entry::parent_entry(),
            Entry::dir("holiday.jpg"),
            Entry::file("beach.jpg"),
        ];
        let out = apply(&mut t, "*.jpg", MaskMode::Wildcard, true, true).expect("wildcard");
        assert_eq!(out.matched, 1);
        assert_eq!(t.marks.len(), 1);
        assert!(t.marks.contains("beach.jpg"));

        // Unticked, the unsurprising behaviour is unchanged.
        let mut t = Tab::new(VfsPath::local("/root"));
        t.entries = vec![Entry::dir("holiday.jpg"), Entry::file("beach.jpg")];
        apply(&mut t, "*.jpg", MaskMode::Wildcard, true, false).expect("wildcard");
        assert_eq!(t.marks.len(), 2);
    }

    #[test]
    fn tab_walks_field_mode_ok_cancel_and_enter_on_cancel_cancels() {
        let mut d = MaskDialog::new(DialogId::SelectMask, "*");
        assert_eq!(d.focused(), Control::Field);
        for want in [
            Control::Mode,
            Control::Exclude,
            Control::Ok,
            Control::Cancel,
            Control::Field,
        ] {
            d.handle_key(&key(KeyCode::Tab));
            assert_eq!(d.focused(), want);
        }
        d.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(d.focused(), Control::Cancel, "Shift+Tab goes the other way");

        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn a_modified_key_never_types_into_the_mask() {
        let mut d = MaskDialog::new(DialogId::SelectMask, "");
        d.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(d.text(), "", "Ctrl+K is not the letter k");
    }

    /// Every cell of a rendered frame, row by row.
    fn dump(buf: &ratatui::buffer::Buffer) -> String {
        let area = *buf.area();
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_prompt_draws_inside_whatever_rectangle_it_is_given() {
        // a dialog is clamped to the terminal, never
        // the reverse. the design calls 60x15 usable; this also checks the
        // sizes below it, where rows are dropped rather than drawn outside.
        //
        // Drawing outside the rectangle panics `TestBackend`, so reaching the
        // assertions is the clamping half. The assertions are what is left on
        // screen after the clamp: down to 24x5 the dialog is still a dialog -
        // it says what it is, shows the mask being edited and keeps the
        // buttons that answer it - and below the minimum it draws nothing at
        // all rather than a frame with nothing inside it.
        let style = DialogStyle::new(&Theme::blue(), crate::config::ColorDepth::TrueColor, false);
        let d = MaskDialog::new(DialogId::SelectMask, "*.rs");
        for (w, h, fits) in [
            (60u16, 15u16, true),
            (40, 8, true),
            (24, 5, true),
            (14, 4, false),
            (12, 3, false),
        ] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|f| {
                    crate::dialog::draw(f, &d, f.area(), &style);
                })
                .expect("draw");
            let out = dump(terminal.backend().buffer());
            if fits {
                assert!(out.contains("Select by mask"), "{w}x{h}:\n{out}");
                assert!(out.contains("*.rs"), "{w}x{h}: no mask on screen:\n{out}");
                assert!(
                    out.contains("[ OK ]") && out.contains("[ Cancel ]"),
                    "{w}x{h}: no buttons to answer with:\n{out}"
                );
            } else {
                assert!(
                    out.trim().is_empty(),
                    "{w}x{h} is below the minimum and should draw nothing:\n{out}"
                );
            }
        }
    }

    // ------------------------------------------------------------- the ≥ ----

    #[test]
    fn a_marked_directory_shows_a_bound_until_it_is_sized() {
        // `+ *` marks the directories without walking them, so
        // the selection's size is a lower bound and says so.
        let cfg = Config::default().panel;
        let mut t = tab();
        for e in &mut t.entries {
            if !e.is_dir() {
                e.size = 1024;
            }
        }
        apply(&mut t, "*.rs", MaskMode::Wildcard, true, false).expect("wildcard");
        apply(&mut t, "src", MaskMode::Wildcard, true, false).expect("wildcard");

        let mut sizes = SizeCache::new();
        let bounded = status_text(&t, &cfg, false, &sizes);
        assert!(bounded.contains('\u{2265}'), "{bounded}");
        assert!(
            status_text(&t, &cfg, true, &sizes).contains(">="),
            "and `>=` under ui.ascii_borders"
        );

        // `Space` or `Ctrl+L` puts the walk's result in the cache, and the
        // bound resolves into a number.
        sizes.insert(
            VfsPath::local("/root/src"),
            TreeStats {
                bytes: 4096,
                files: 3,
                dirs: 1,
            },
        );
        let exact = status_text(&t, &cfg, false, &sizes);
        assert!(!exact.contains('\u{2265}'), "{exact}");
        assert!(exact.contains("6 k"), "2 KiB of files plus 4 KiB: {exact}");
    }

    #[test]
    fn a_re_read_invalidates_the_cached_size() {
        // "cached for the session against the path and
        // invalidated when the panel re-reads it".
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let root = VfsPath::local("/root");
        app.left.active_tab_mut().path = root.clone();
        app.jobs.sizes.insert(root.join("src"), TreeStats::ZERO);
        app.jobs
            .sizes
            .insert(VfsPath::local("/elsewhere"), TreeStats::ZERO);
        assert!(app.jobs.sizes.contains(&root.join("src")));

        app.request_read(crate::panel::Side::Left, 0, root.clone());
        assert!(
            !app.jobs.sizes.contains(&root.join("src")),
            "the subtree under a re-read path cannot be trusted"
        );
        assert!(
            app.jobs.sizes.contains(&VfsPath::local("/elsewhere")),
            "and nothing else was dropped"
        );
    }
}
