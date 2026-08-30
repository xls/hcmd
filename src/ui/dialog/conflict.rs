//! The conflict dialog.
//!
//! > **Conflict policy** on an existing destination: overwrite / skip / rename
//! > / append, each with an "all" variant, plus "overwrite if newer" and
//! > "overwrite if different size". Decisions apply for the remainder of the
//! > batch.
//!
//! ```text
//! ┌ File exists ───────────────────────────────────────────┐
//! │ /srv/media/report.txt                                  │
//! │ existing            1,234,567       2026-08-01 12:00   │
//! │ new file            2,345,678       2026-08-27 09:14   │
//! │ Rename to: report (2).txt                              │
//! │ [x] Apply to all remaining conflicts                   │
//! │ [ Overwrite ] [ Skip ] [ Rename ] [ Append ]           │
//! │ [ If newer ] [ If different size ] [ Cancel ]          │
//! └────────────────────────────────────────────────────────┘
//! ```
//!
//! # Both files' size and mtime, always
//!
//! An "overwrite?" that does not say what is being overwritten is a coin toss.
//! Sizes are spelled the way the `size` column spells them and
//! the timestamps the way the `date` column does (`panel.date_format`), so the
//! numbers in the dialog and the numbers in the panel behind it agree.
//!
//! # The "all" variants are a checkbox, not six more buttons
//!
//! the choice and its scope are orthogonal, so the
//! ten buttons the design describes are six buttons and one checkbox. An
//! "all" *rename* auto-generates names - one typed name cannot serve a batch -
//! which is why ticking the box empties the rename field.
//!
//! # `Esc` answers `Cancel`
//!
//! The worker is **parked** inside [`crate::ops::JobContext::ask`] until this
//! dialog answers. Closing without answering would leave it there, so `Esc`
//! sends [`Decision::Cancel`] rather than returning [`DialogOutcome::Cancel`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::time::SystemTime;

use super::field::Field;
use super::{bytes_text, checkbox, row};
use crate::config::PanelConfig;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    draw_mnemonic, draw_mnemonic_buttons, draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::ops::{ConflictChoice, ConflictRequest, Decision, JobId};
use crate::ui::text;
use crate::vfs::Entry;

/// One focusable control.
///
/// It wraps the *choice* rather than a ring index, because the ring is built
/// from a runtime `Vec<ConflictChoice>`: index 0 is `Overwrite` on a file
/// conflict and `Skip` on a directory one, so a letter tied to an index would
/// name a different button depending on what collided.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// One of the six choices, as offered by this conflict.
    Choice(ConflictChoice),
    /// The `Rename to:` field.
    RenameField,
    /// `[x] Apply to all remaining conflicts`.
    ApplyToAll,
    /// `Cancel`.
    Cancel,
}

/// the `Alt` mnemonics for this dialog.
///
/// `o`, `s`, `r`, `p` and `a` are **the same letters this dialog already
/// answers to as bare keys**, so `Alt+O` and `O` do the same thing and there is
/// one set to learn. `i` and `d` are new reach: `If newer` and `If different
/// size` share an initial and could not have bare-letter accelerators,
/// which is exactly what the design buys that a
/// bare letter cannot.
///
/// [`ConflictChoice::Append`] keeps `p` rather than `a` because `a` is
/// `Apply to all`, which is the existing bare-letter assignment and the more
/// frequently wanted control. `n` is the program-wide `Cancel`.
pub const MNEMONICS: &[(Control, char)] = &[
    (Control::Choice(ConflictChoice::Overwrite), 'o'),
    (Control::Choice(ConflictChoice::Skip), 's'),
    (Control::Choice(ConflictChoice::Rename), 'r'),
    (Control::Choice(ConflictChoice::Append), 'p'),
    (Control::Choice(ConflictChoice::OverwriteIfNewer), 'i'),
    (
        Control::Choice(ConflictChoice::OverwriteIfDifferentSize),
        'd',
    ),
    (Control::RenameField, 't'),
    (Control::ApplyToAll, 'a'),
    (Control::Cancel, 'n'),
];

/// The `Apply to all` checkbox's label, and the string the underline
/// is searched in.
const ALL_LABEL: &str = "Apply to all remaining conflicts";

/// Which row of the interior a piece of the dialog occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The destination that is in the way.
    Path,
    /// The existing file's size and mtime.
    Existing,
    /// The incoming file's size and mtime.
    Incoming,
    /// The name [`ConflictChoice::Rename`] will use.
    Rename,
    /// "Apply to all remaining conflicts".
    All,
    /// Overwrite / Skip / Rename / Append.
    Buttons,
    /// If newer / If different size / Cancel.
    MoreButtons,
}

/// What goes first when the interior is short.
///
/// The rename field goes before either descriptive row, because it only matters
/// to one of six choices; the two comparison rows go last, because they are the
/// entire reason this dialog is not a yes/no box.
const DROP_ORDER: [Slot; 4] = [Slot::Rename, Slot::All, Slot::Incoming, Slot::Existing];

/// the conflict dialog.
#[derive(Debug)]
pub struct ConflictDialog {
    /// **Which job asked.** the design lets a backgrounded job sit parked on
    /// a conflict while a second job's dialog is the one on screen, so the
    /// answer has to name the job it was given for - routing it to whichever
    /// job happened to be parked first answers a question the user never saw.
    job: JobId,
    request: Box<ConflictRequest>,
    /// The choices offered, in button order. [`ConflictChoice::Append`] is
    /// absent for a directory conflict.
    choices: Vec<ConflictChoice>,
    rename: Field,
    apply_to_all: bool,
    /// Pre-formatted so `render` needs no configuration.
    path_text: String,
    existing_text: String,
    incoming_text: String,
    ring: FocusRing,
}

impl ConflictDialog {
    /// A dialog for one conflict.
    ///
    /// `suggested` is the free name [`crate::ops::copy::free_name`] would pick,
    /// offered in the rename field so the common case is `Rename` and `Enter`.
    pub fn new(
        job: JobId,
        request: Box<ConflictRequest>,
        suggested: impl Into<String>,
        cfg: &PanelConfig,
    ) -> Self {
        // A directory in the way of a file offers neither of the two choices
        // that would have to remove it: `Overwrite` on it is refused by
        // [`crate::ops::conflict::Plan::Refuse`] - a file-shaped answer is not
        // consent to `remove_dir_all` a tree - and there is nothing to append
        // to. `Rename`, `Skip` and `Cancel` are the ones that can happen.
        let dest_dir_only = request.dest_is_dir && !request.both_dirs;
        let choices: Vec<ConflictChoice> = ConflictChoice::ALL
            .iter()
            .copied()
            .filter(|c| !(request.both_dirs && *c == ConflictChoice::Append))
            .filter(|c| {
                !(dest_dir_only
                    && matches!(
                        c,
                        ConflictChoice::Overwrite
                            | ConflictChoice::Append
                            | ConflictChoice::OverwriteIfNewer
                            | ConflictChoice::OverwriteIfDifferentSize
                    ))
            })
            .collect();
        let path_text = request.dest.to_string();
        let existing_text = if dest_dir_only {
            // Naming it a directory is the whole difference between this and a
            // file-versus-file collision: a size and an mtime alone read as a
            // file, and the answer given to a file is not an answer about a
            // tree.
            format!(
                "a directory   {}",
                describe(request.dest_size, request.dest_mtime, cfg)
            )
        } else {
            describe(request.dest_size, request.dest_mtime, cfg)
        };
        let incoming_text = describe(request.source_size, request.source_mtime, cfg);
        // One control per choice, then the rename field (files only), the
        // checkbox and Cancel.
        let extra = if request.both_dirs { 2 } else { 3 };
        let count = choices.len().saturating_add(extra);
        let mut ring = FocusRing::new(count);
        // Open on `Skip`, not on `Overwrite`. the design makes
        // a confirmation open on the safe button, for the reason that applies
        // twice over here: the `Enter` the user was already pressing must not
        // be the one that destroys a file, and skipping is the only choice
        // nothing is lost to.
        if let Some(safe) = choices.iter().position(|c| *c == ConflictChoice::Skip) {
            ring.set(safe);
        }
        Self {
            job,
            request,
            choices,
            rename: Field::with_text(suggested),
            apply_to_all: false,
            path_text,
            existing_text,
            incoming_text,
            ring,
        }
    }

    /// The conflict being resolved.
    pub const fn request(&self) -> &ConflictRequest {
        &self.request
    }

    /// The job this question belongs to.
    pub const fn job_id(&self) -> JobId {
        self.job
    }

    /// Whether the answer will be installed as the standing policy
    /// ("Decisions apply for the remainder of the batch").
    pub const fn apply_to_all(&self) -> bool {
        self.apply_to_all
    }

    /// The name [`ConflictChoice::Rename`] would use.
    pub fn rename_to(&self) -> &str {
        self.rename.text()
    }

    /// The choices this conflict offers.
    pub fn choices(&self) -> &[ConflictChoice] {
        &self.choices
    }

    /// Index of the rename field in the focus ring, or [`usize::MAX`] when
    /// there is no rename field - a directory conflict has nothing to rename,
    /// and a control in the ring that is never drawn is a focus black hole.
    fn rename_index(&self) -> usize {
        if self.request.both_dirs {
            usize::MAX
        } else {
            self.choices.len()
        }
    }

    /// Index of the "apply to all" checkbox.
    fn all_index(&self) -> usize {
        self.choices
            .len()
            .saturating_add(usize::from(!self.request.both_dirs))
    }

    /// Index of `Cancel`.
    fn cancel_index(&self) -> usize {
        self.all_index().saturating_add(1)
    }

    /// Answer with `choice`.
    fn choose(&self, choice: ConflictChoice) -> DialogOutcome {
        let rename_to = if choice == ConflictChoice::Rename && !self.apply_to_all {
            let name = self.rename.text().trim();
            // An empty field means "you pick": the worker generates a free
            // name, which is also what an "all" rename does.
            (!name.is_empty()).then(|| name.to_string())
        } else {
            None
        };
        DialogOutcome::Accept(DialogResult::Conflict(Box::new(Decision::Conflict {
            choice,
            rename_to,
            apply_to_all: self.apply_to_all,
        })))
    }

    /// Abandon the job. `Esc` and the `Cancel` button both land here - the
    /// worker is parked on this question and something has to answer it.
    fn abandon() -> DialogOutcome {
        DialogOutcome::Accept(DialogResult::Conflict(Box::new(Decision::Cancel)))
    }

    /// Tick `Apply to all`, and empty the rename field because one typed name
    /// cannot serve a batch.
    fn apply_to_all_on(&mut self) {
        self.apply_to_all = true;
        self.rename.set_text("");
    }

    /// A choice's bare-key accelerator, so `o`, `s`, `r` and `p` work without
    /// `Tab` and without `Alt`.
    ///
    /// **The letter is [`MNEMONICS`]'s and not a second list.** the design's
    /// `Alt`+letter and this dialog's older bare letters are two routes to one
    /// button, so `Alt+O` and `O` cannot come apart.
    ///
    ///
    /// `If newer` and `If different size` keep no bare letter: they share an
    /// initial, and a bare `i` that pressed one of them would be the wrong
    /// button half the time. Reaching them is exactly what `Alt+I` and `Alt+D`
    /// are for.
    fn accelerator(&self, choice: ConflictChoice) -> Option<char> {
        match choice {
            ConflictChoice::Overwrite
            | ConflictChoice::Skip
            | ConflictChoice::Rename
            | ConflictChoice::Append => self.mnemonic_of(Control::Choice(choice)),
            ConflictChoice::OverwriteIfNewer | ConflictChoice::OverwriteIfDifferentSize => None,
        }
    }

    /// Which slots this interior has room for.
    fn rows(&self, area: Rect) -> Vec<(Slot, Rect)> {
        let mut wanted = vec![Slot::Path, Slot::Existing, Slot::Incoming];
        if !self.request.both_dirs {
            wanted.push(Slot::Rename);
        }
        wanted.push(Slot::All);
        wanted.push(Slot::Buttons);
        wanted.push(Slot::MoreButtons);

        let height = usize::from(area.height);
        for slot in DROP_ORDER {
            if wanted.len() <= height {
                break;
            }
            wanted.retain(|s| *s != slot);
        }
        wanted.truncate(height);

        wanted
            .into_iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let index = u16::try_from(i).unwrap_or(u16::MAX);
                row(area, index).map(|rect| (slot, rect))
            })
            .collect()
    }

    /// The rename field's rectangle inside its row: the label, then the field.
    fn rename_field(rect: Rect) -> Rect {
        let label = u16::try_from(text::width(RENAME_LABEL)).unwrap_or(0);
        if rect.width <= label {
            return Rect::new(rect.x, rect.y, 0, 0);
        }
        Rect::new(
            rect.x.saturating_add(label),
            rect.y,
            rect.width.saturating_sub(label),
            1,
        )
    }
}

/// The rename row's label.
const RENAME_LABEL: &str = "Rename to: ";

/// One file's size and mtime, laid out so the two rows line up.
fn describe(size: u64, mtime: Option<SystemTime>, cfg: &PanelConfig) -> String {
    let mut entry = Entry::file("");
    entry.size = size;
    entry.mtime = mtime;
    let date = crate::panel::format::date_text(&entry, cfg);
    let size = bytes_text(size, cfg);
    if date.is_empty() {
        size
    } else {
        format!("{size}   {date}")
    }
}

impl Accelerated for ConflictDialog {
    type Control = Control;

    fn mnemonics(&self) -> &'static [(Control, char)] {
        MNEMONICS
    }

    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            // A directory conflict offers neither `Overwrite`, `Append`,
            // `If newer`, `If different size` nor the rename field. Those
            // letters are swallowed and do nothing, because a dialog consumes
            // all input.
            Control::Choice(choice) => {
                if self.choices.contains(&choice) {
                    Accel::Press
                } else {
                    Accel::Absent
                }
            }
            Control::RenameField => {
                if self.request.both_dirs {
                    Accel::Absent
                } else {
                    Accel::Focus
                }
            }
            Control::ApplyToAll => Accel::Check,
            Control::Cancel => Accel::Press,
        }
    }

    fn focus_control(&mut self, control: Control) {
        let at = match control {
            Control::Choice(choice) => self.choices.iter().position(|c| *c == choice),
            Control::RenameField => {
                let index = self.rename_index();
                (index != usize::MAX).then_some(index)
            }
            Control::ApplyToAll => Some(self.all_index()),
            Control::Cancel => Some(self.cancel_index()),
        };
        if let Some(at) = at {
            self.ring.set(at);
        }
    }

    /// **on**, never a toggle. The bare `a` this dialog already
    /// answers to still toggles; `Alt+A` only ever ticks.
    fn switch_on(&mut self, control: Control) {
        match control {
            Control::ApplyToAll => self.apply_to_all_on(),
            Control::Choice(_) | Control::RenameField | Control::Cancel => {}
        }
    }

    fn press(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Choice(choice) => self.choose(choice),
            Control::Cancel => Self::abandon(),
            Control::RenameField | Control::ApplyToAll => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for ConflictDialog {
    fn job(&self) -> Option<JobId> {
        Some(self.job)
    }

    fn id(&self) -> DialogId {
        DialogId::Conflict
    }

    fn title(&self) -> String {
        if self.request.both_dirs {
            "Directory exists".to_string()
        } else if self.request.dest_is_dir {
            "A directory is in the way".to_string()
        } else {
            "File exists".to_string()
        }
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = text::width(&self.path_text)
            .max(text::width(&self.existing_text).saturating_add(12))
            .max(text::width(&self.incoming_text).saturating_add(12))
            .max(50);
        let w = u16::try_from(widest.saturating_add(4)).unwrap_or(u16::MAX);
        let rows = if self.request.both_dirs { 6 } else { 7 };
        (w.clamp(46, 76), rows + 2)
    }

    /// The letters this conflict actually offers.
    ///
    ///
    /// Per-instance, which is why [`Dialog::mnemonic_letters`] returns a `Vec`:
    /// a directory conflict offers four fewer choices and no rename field, and
    /// the five letters that name them are absent rather than merely inert.
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
        // rename field can see the key. The bare
        // letters below are unchanged, including their "not inside the rename
        // field" guard: `Alt+O` and `O` reach the same button by two routes.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return Self::abandon();
        }

        let index = self.ring.index();
        let on_rename_field = index == self.rename_index();

        if key.is_accept() {
            if index == self.cancel_index() {
                return self.press(Control::Cancel);
            }
            if let Some(choice) = self.choices.get(index).copied() {
                return self.press(Control::Choice(choice));
            }
            if on_rename_field {
                // The field belongs to one choice, so `Enter` in it is that
                // choice: type a name, press Enter.
                return self.press(Control::Choice(ConflictChoice::Rename));
            }
            // From the checkbox, which belongs to no choice in particular, the
            // safe one: skipping loses nothing.
            return self.press(Control::Choice(ConflictChoice::Skip));
        }

        // The checkbox toggles with Space wherever it has focus, and with `a`
        // from anywhere that is not a text field.
        let toggles_all = (index == self.all_index() && key.text() == Some(' '))
            || (!on_rename_field && key.text() == Some('a'));
        if toggles_all {
            if self.apply_to_all {
                self.apply_to_all = false;
            } else {
                self.apply_to_all_on();
            }
            return DialogOutcome::Consumed;
        }

        if on_rename_field && self.rename.handle(key) {
            return DialogOutcome::Consumed;
        }

        // An initial letter chooses - but only outside the rename field, where
        // the same letter is text.
        let accelerator = key
            .text()
            .filter(|_| !on_rename_field)
            .map(|c| c.to_ascii_lowercase());
        if let Some(choice) = accelerator.and_then(|c| {
            self.choices
                .iter()
                .copied()
                .find(|choice| self.accelerator(*choice) == Some(c))
        }) {
            return self.press(Control::Choice(choice));
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
        let body: Style = style.body();
        let index = self.ring.index();
        // The two button rows are one list split in two, so the focus index has
        // to be translated into each half.
        let split = self.choices.len().min(4);
        for (slot, rect) in self.rows(area) {
            match slot {
                Slot::Path => draw_text(
                    f,
                    rect,
                    &super::crop_left(&self.path_text, usize::from(rect.width), style.ascii),
                    body,
                    style.ascii,
                ),
                Slot::Existing => draw_text(
                    f,
                    rect,
                    &format!("existing   {}", self.existing_text),
                    body,
                    style.ascii,
                ),
                Slot::Incoming => draw_text(
                    f,
                    rect,
                    &format!("new file   {}", self.incoming_text),
                    body,
                    style.ascii,
                ),
                Slot::Rename => {
                    // The `t` of the trailing `to`, which the word-start rule
                    // of the design picks over the `t` inside
                    // `Rename` - whose own `r` belongs to the `Rename` button.
                    draw_mnemonic(
                        f,
                        rect,
                        RENAME_LABEL,
                        't',
                        style.button(index == self.rename_index()),
                        style.ascii,
                    );
                    let field = Self::rename_field(rect);
                    if field.width > 0 {
                        self.rename.render(f, field, style);
                    }
                }
                Slot::All => draw_mnemonic(
                    f,
                    rect,
                    &checkbox(ALL_LABEL, self.apply_to_all, style.ascii),
                    'a',
                    style.button(index == self.all_index()),
                    style.ascii,
                ),
                Slot::Buttons => {
                    let labels: Vec<(&str, Option<char>)> = self
                        .choices
                        .iter()
                        .take(split)
                        .map(|c| (c.label(), self.mnemonic_of(Control::Choice(*c))))
                        .collect();
                    draw_mnemonic_buttons(f, rect, &labels, index, style);
                }
                Slot::MoreButtons => {
                    let mut labels: Vec<(&str, Option<char>)> = self
                        .choices
                        .iter()
                        .skip(split)
                        .map(|c| (c.label(), self.mnemonic_of(Control::Choice(*c))))
                        .collect();
                    labels.push(("Cancel", Some('n')));
                    let focus = index.checked_sub(split).map_or(usize::MAX, |i| {
                        // The rename field and the checkbox sit between the two
                        // halves of the choice list, so only Cancel and the
                        // trailing choices map onto this row.
                        let choices_here = self.choices.len().saturating_sub(split);
                        if i < choices_here {
                            i
                        } else if index == self.cancel_index() {
                            choices_here
                        } else {
                            usize::MAX
                        }
                    });
                    draw_mnemonic_buttons(f, rect, &labels, focus, style);
                }
            }
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if self.ring.index() != self.rename_index() {
            return None;
        }
        let rect = self
            .rows(area)
            .into_iter()
            .find_map(|(s, r)| (s == Slot::Rename).then_some(r))?;
        let field = Self::rename_field(rect);
        if field.width == 0 {
            return None;
        }
        self.rename.cursor(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::KeyPress;
    use crate::vfs::VfsPath;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::time::Duration;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn typed(d: &mut ConflictDialog, text: &str) {
        for c in text.chars() {
            d.handle_key(&key(KeyCode::Char(c)));
        }
    }

    fn request(both_dirs: bool) -> Box<ConflictRequest> {
        let base = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(1_774_000_000))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        Box::new(ConflictRequest {
            source: VfsPath::local("/tmp/report.txt"),
            dest: VfsPath::local("/srv/media/report.txt"),
            source_size: 2_345_678,
            dest_size: 1_234_567,
            source_mtime: Some(base),
            dest_mtime: base.checked_sub(Duration::from_secs(86_400)),
            both_dirs,
            dest_is_dir: both_dirs,
        })
    }

    fn dialog() -> ConflictDialog {
        ConflictDialog::new(
            JobId(1),
            request(false),
            "report (2).txt",
            &PanelConfig::default(),
        )
    }

    /// `Alt`+letter, the keystroke the design is about.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(
            KeyCode::Char(c),
            crate::input::KeyModifiers::ALT,
        ))
    }

    /// Every character drawn with [`ratatui::style::Modifier::UNDERLINED`],
    /// folded to lower case. the underline, read off the buffer
    /// rather than off the table.
    fn underlined(d: &ConflictDialog, w: u16, h: u16) -> Vec<char> {
        let buffer = render(d, w, h, false);
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

    fn render(d: &ConflictDialog, w: u16, h: u16, ascii: bool) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &st);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The dialog's own interior, without the framework's border.
    ///
    /// The frame is `crate::dialog::draw`'s and is tested there and in
    /// `ui::tests::every_v02_dialog_renders_inside_the_minimum_terminal`,
    /// which draws the whole box over the real panels at 60x15 in both glyph
    /// sets. This renders what *this* dialog is responsible for, so an ASCII
    /// failure below is the dialog's own and not the border's.
    fn render_inner(d: &impl Dialog, w: u16, h: u16, ascii: bool) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                let area = f.area();
                d.render(f, area, &st);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn dump(buf: &Buffer) -> String {
        let area = buf.area();
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            out.push('\n');
        }
        out
    }

    fn decision(outcome: DialogOutcome) -> Decision {
        match outcome {
            DialogOutcome::Accept(DialogResult::Conflict(decision)) => *decision,
            other => panic!("expected a decision, got {other:?}"),
        }
    }

    #[test]
    fn every_choice_the_spec_lists_is_offered() {
        // overwrite / skip / rename / append, plus if-newer and
        // if-different-size.
        let d = dialog();
        assert_eq!(d.choices(), ConflictChoice::ALL);
    }

    #[test]
    fn a_directory_conflict_never_offers_append() {
        // appending cannot mean anything for a directory.
        let d = ConflictDialog::new(JobId(1), request(true), "x", &PanelConfig::default());
        assert!(!d.choices().contains(&ConflictChoice::Append));
        assert_eq!(d.choices().len(), ConflictChoice::ALL.len() - 1);
        assert_eq!(d.title(), "Directory exists");
    }

    #[test]
    fn both_files_size_and_mtime_are_shown_so_the_choice_is_informed() {
        let out = dump(&render(&dialog(), 80, 24, false));
        assert!(out.contains("1,234,567"), "the existing size:\n{out}");
        assert!(out.contains("2,345,678"), "the incoming size:\n{out}");
        assert!(out.contains("existing"), "{out}");
        assert!(out.contains("new file"), "{out}");
        // Two different dates, so the two rows cannot be the same row twice.
        let dates: Vec<&str> = out.lines().filter(|l| l.contains(':')).collect();
        assert!(dates.len() >= 2, "{out}");
    }

    #[test]
    fn esc_answers_cancel_rather_than_leaving_the_worker_parked() {
        let mut d = dialog();
        assert_eq!(decision(d.handle_key(&key(KeyCode::Esc))), Decision::Cancel);
    }

    #[test]
    fn an_initial_letter_chooses_without_tabbing() {
        for (letter, expected) in [
            ('o', ConflictChoice::Overwrite),
            ('s', ConflictChoice::Skip),
            ('r', ConflictChoice::Rename),
            ('p', ConflictChoice::Append),
        ] {
            let mut d = dialog();
            match decision(d.handle_key(&key(KeyCode::Char(letter)))) {
                Decision::Conflict { choice, .. } => assert_eq!(choice, expected, "{letter}"),
                other => panic!("{letter}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_all_variant_is_a_checkbox_and_it_empties_the_rename_field() {
        // the choice and its scope are orthogonal, and an "all" rename
        // auto-generates names.
        let mut d = dialog();
        assert!(!d.apply_to_all());
        assert_eq!(d.rename_to(), "report (2).txt");
        d.handle_key(&key(KeyCode::Char('a')));
        assert!(d.apply_to_all());
        assert_eq!(d.rename_to(), "");

        match decision(d.handle_key(&key(KeyCode::Char('o')))) {
            Decision::Conflict {
                choice,
                apply_to_all,
                rename_to,
            } => {
                assert_eq!(choice, ConflictChoice::Overwrite);
                assert!(apply_to_all);
                assert_eq!(rename_to, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_typed_rename_travels_with_the_decision() {
        let mut d = dialog();
        while d.ring.index() != d.rename_index() {
            d.handle_key(&key(KeyCode::Tab));
        }
        for _ in 0..40 {
            d.handle_key(&key(KeyCode::Backspace));
        }
        typed(&mut d, "report.v2.txt");
        match decision(d.handle_key(&key(KeyCode::Enter))) {
            Decision::Conflict {
                choice, rename_to, ..
            } => {
                assert_eq!(choice, ConflictChoice::Rename);
                assert_eq!(rename_to.as_deref(), Some("report.v2.txt"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_rename_asks_the_worker_to_pick() {
        let mut d = dialog();
        while d.ring.index() != d.rename_index() {
            d.handle_key(&key(KeyCode::Tab));
        }
        for _ in 0..40 {
            d.handle_key(&key(KeyCode::Backspace));
        }
        match decision(d.handle_key(&key(KeyCode::Enter))) {
            Decision::Conflict { rename_to, .. } => assert_eq!(rename_to, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn typing_in_the_rename_field_never_triggers_an_accelerator() {
        let mut d = dialog();
        while d.ring.index() != d.rename_index() {
            d.handle_key(&key(KeyCode::Tab));
        }
        for _ in 0..40 {
            d.handle_key(&key(KeyCode::Backspace));
        }
        typed(&mut d, "sorpa");
        assert_eq!(d.rename_to(), "sorpa", "the letters typed, not chose");
        assert!(!d.apply_to_all(), "and `a` did not toggle the checkbox");
    }

    #[test]
    fn it_renders_at_every_size_the_spec_names() {
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                let out = dump(&render(&dialog(), w, h, ascii));
                assert!(out.contains("Overwrite"), "{w}x{h} ascii={ascii}:\n{out}");
                assert!(out.contains("Skip"), "{w}x{h}:\n{out}");
            }
        }
    }

    #[test]
    fn every_glyph_it_draws_has_an_ascii_form() {
        // `ui.ascii_borders = true` falls back to `+-|`.
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (30, 6)] {
            let inner = dump(&render_inner(&dialog(), w, h, true));
            assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
        }
    }

    #[test]
    fn no_row_ever_leaves_the_interior_and_none_is_zero_sized() {
        let d = dialog();
        for h in 0u16..12 {
            for w in [0u16, 1, 12, 40, 76] {
                let area = Rect::new(1, 1, w, h);
                for (_, rect) in d.rows(area) {
                    assert!(rect.width > 0 && rect.height > 0, "{w}x{h}: {rect:?}");
                    assert!(rect.bottom() <= area.bottom(), "{w}x{h}: {rect:?}");
                }
            }
        }
    }

    #[test]
    fn every_control_is_reachable_by_tab_and_the_ring_wraps() {
        let mut d = dialog();
        let count = ConflictChoice::ALL.len() + 3;
        assert_eq!(d.ring.count(), count);
        let mut seen = vec![d.ring.index()];
        for _ in 1..count {
            d.handle_key(&key(KeyCode::Tab));
            seen.push(d.ring.index());
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "some control is unreachable: {seen:?}");
        let before = d.ring.index();
        d.handle_key(&key(KeyCode::Tab));
        assert_ne!(d.ring.index(), before, "it wraps rather than sticking");
    }

    #[test]
    fn it_opens_on_the_choice_that_destroys_nothing() {
        // Contract the reasoning: the `Enter` the user was already pressing
        // must not be the one that overwrites a file.
        let mut d = dialog();
        assert_eq!(
            d.choices().get(d.ring.index()).copied(),
            Some(ConflictChoice::Skip)
        );
        match decision(d.handle_key(&key(KeyCode::Enter))) {
            Decision::Conflict { choice, .. } => assert_eq!(choice, ConflictChoice::Skip),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_directory_conflict_has_no_rename_control_in_its_ring() {
        let d = ConflictDialog::new(JobId(1), request(true), "x", &PanelConfig::default());
        assert_eq!(d.ring.count(), d.choices().len() + 2);
        assert_eq!(d.rename_index(), usize::MAX);
        assert_eq!(d.cancel_index(), d.ring.count() - 1);
    }

    #[test]
    fn every_choice_is_reachable_by_its_alt_letter() {
        // the design. `o`, `s`, `r` and `p` are the letters
        // this dialog already answered to as bare keys, so `Alt+O` and `O` do
        // the same thing and there is one set to learn.
        for (letter, want) in [
            ('o', ConflictChoice::Overwrite),
            ('s', ConflictChoice::Skip),
            ('r', ConflictChoice::Rename),
            ('p', ConflictChoice::Append),
            ('i', ConflictChoice::OverwriteIfNewer),
            ('d', ConflictChoice::OverwriteIfDifferentSize),
        ] {
            let mut d = dialog();
            match decision(d.handle_key(&alt(letter))) {
                Decision::Conflict { choice, .. } => assert_eq!(choice, want, "Alt+{letter}"),
                other => panic!("Alt+{letter}: {other:?}"),
            }
        }
        // And `Alt+N` is `Cancel`, as it is in every dialog that has one.
        let mut d = dialog();
        assert_eq!(decision(d.handle_key(&alt('n'))), Decision::Cancel);
    }

    #[test]
    fn the_two_choices_a_bare_letter_could_never_reach() {
        // `If newer` and `If different size` share an initial, so they have no
        // bare-letter accelerator - which is
        // exactly what the design buys that a bare letter cannot. The bare
        // keys are unchanged: `i` and `d` alone still do nothing.
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Char('i'))),
            DialogOutcome::Ignored
        ));
        assert!(matches!(
            d.handle_key(&key(KeyCode::Char('d'))),
            DialogOutcome::Ignored
        ));
    }

    #[test]
    fn alt_t_focuses_the_rename_field_and_types_nothing_into_it() {
        // the design on a plain field: the caret moves in and nothing else
        // changes - not the text, not the selection.
        let mut d = dialog();
        assert!(matches!(d.handle_key(&alt('t')), DialogOutcome::Consumed));
        assert_eq!(d.rename_to(), "report (2).txt");
        // And now that the field has focus, a bare letter is text again: the
        // "not inside the rename field" guard is unchanged.
        typed(&mut d, "x");
        assert!(d.rename_to().contains('x'));
    }

    #[test]
    fn alt_a_ticks_apply_to_all_and_never_unticks_it() {
        // the "an accelerator never turns anything off", against the
        // bare `a` this dialog has always had, which still toggles.
        let mut d = dialog();
        assert!(!d.apply_to_all());
        d.handle_key(&alt('a'));
        assert!(d.apply_to_all(), "Alt+A ticked it");
        d.handle_key(&alt('a'));
        assert!(d.apply_to_all(), "Alt+A again left it ticked");
        assert_eq!(d.rename_to(), "", "and one name cannot serve a batch");
        d.handle_key(&key(KeyCode::Char('a')));
        assert!(!d.apply_to_all(), "the bare letter is still the toggle");
    }

    /// A directory standing where a file is being written: the design's
    /// awkward case, and the one that offers the fewest choices.
    fn dir_in_the_way() -> Box<ConflictRequest> {
        let mut req = request(false);
        req.dest_is_dir = true;
        req
    }

    #[test]
    fn a_conflict_swallows_the_letters_it_does_not_offer() {
        // a letter that names a control
        // this instance does not draw is **consumed and nothing happens**. It
        // is not passed on, because a dialog consumes all input,
        // and it does not conjure the missing button.
        //
        // A directory in the way of a file offers neither of the two choices
        // that would have to remove it, nor either conditional overwrite.
        let mut d = ConflictDialog::new(
            JobId(1),
            dir_in_the_way(),
            "report (2).txt",
            &PanelConfig::default(),
        );
        for letter in ['o', 'p', 'i', 'd'] {
            assert!(
                matches!(d.handle_key(&alt(letter)), DialogOutcome::Consumed),
                "Alt+{letter} on a directory in the way"
            );
        }
        assert_eq!(d.rename_to(), "report (2).txt", "and nothing changed");
        let mut letters = d.mnemonic_letters();
        letters.sort_unstable();
        assert_eq!(letters, vec!['a', 'n', 'r', 's', 't'], "nor advertised");

        // A directory over a directory has no `Append` and no rename field.
        let mut d = ConflictDialog::new(
            JobId(1),
            request(true),
            "media (2)",
            &PanelConfig::default(),
        );
        for letter in ['p', 't'] {
            assert!(
                matches!(d.handle_key(&alt(letter)), DialogOutcome::Consumed),
                "Alt+{letter} on a directory-over-directory conflict"
            );
        }
        let mut letters = d.mnemonic_letters();
        letters.sort_unstable();
        assert_eq!(letters, vec!['a', 'd', 'i', 'n', 'o', 'r', 's']);

        // And a choice it does offer still answers.
        let mut d = ConflictDialog::new(
            JobId(1),
            request(true),
            "media (2)",
            &PanelConfig::default(),
        );
        match decision(d.handle_key(&alt('s'))) {
            Decision::Conflict { choice, .. } => assert_eq!(choice, ConflictChoice::Skip),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mnemonics_are_unique_within_this_dialog() {
        // a duplicate is a bug rather than a first-one-wins rule.
        let mut seen: Vec<char> = Vec::new();
        for (control, letter) in MNEMONICS {
            assert!(letter.is_ascii_lowercase(), "{control:?}: stored folded");
            assert!(!seen.contains(letter), "{control:?}: Alt+{letter} is taken");
            seen.push(*letter);
        }
        assert_eq!(seen.len(), 9, "six choices, the field, the box and Cancel");
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // All three shapes this dialog takes, because each draws a different
        // set of buttons (the design I3).
        for (name, req) in [
            ("file over file", request(false)),
            ("directory over directory", request(true)),
            ("directory in the way", dir_in_the_way()),
        ] {
            let d = ConflictDialog::new(JobId(1), req, "report (2).txt", &PanelConfig::default());
            let mut want = d.mnemonic_letters();
            let mut got = underlined(&d, 100, 24);
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "{name}: underlines on screen");
        }
    }
}
