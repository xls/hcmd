//! The copy / move dialog.
//!
//! Modelled on the Total Commander screenshot the design is taken from:
//!
//! ```text
//! ┌ Rename/Move 3 file(s) ─────────────────────────────────┐
//! │ Rename/Move 3 file(s) to                               │
//! │ /srv/media/*.*                                  [+ F7] │
//! │ 17.75 G · 523 files · 95 folders                       │
//! │ Only files of this type:                               │
//! │ *.*                                             [+ F8] │
//! │ [x] Preserve attributes                     [ ] Verify │
//! │ [ OK ] [ F2 Queue ] [ Tree ] [ Cancel ] [ Options >> ] │
//! └────────────────────────────────────────────────────────┘
//! ```
//!
//! # The one detail that matters
//!
//! The target field's **mask portion is preselected** ([`Field::with_mask_selected`]),
//! so typing replaces it and leaves the path. that is what makes
//! "copy these as `*.bak`" a two-second operation, and it is the point of the
//! dialog. Everything else here is a form.
//!
//! # Selection statistics are computed before the dialog opens
//!
//! "Compute these before the dialog opens; they are the last
//! chance to notice a mistake." [`crate::ops::selection_stats`] reads the size
//! cache and is therefore instant; what is *not* instant is the tree walk that
//! fills the cache. So the dialog takes the statistics it can have now and a
//! `sizing` flag: while a [`crate::ops::JobKind::Size`] walk is in flight the
//! line carries a spinner and the figure carries the `≥`, and
//! [`CopyMoveDialog::set_stats`] replaces it when the walk lands. The dialog
//! never blocks, and it never shows a computed-looking total that is partial.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::time::Instant;

use super::field::Field;
use super::{bytes_text, checkbox, ellipsis, row, spinner};
use crate::config::{OpsConfig, PanelConfig};
use crate::dialog::{
    Accel, Accelerated, CopyMoveAnswer, Dialog, DialogKey, DialogOutcome, DialogResult,
    DialogStyle, FocusRing, InputDialog, MessageDialog, Piece, draw_mnemonic,
    draw_mnemonic_buttons, draw_mnemonic_pieces, draw_text,
};
use crate::input::{DialogId, KeyCode, Milestone};
use crate::ops::{ConflictChoice, JobKind, JobStatus, SelectionStats};
use crate::ui::text;
use crate::vfs::VfsPath;

/// One focusable control.
///
/// `pub` because it is [`Accelerated::Control`], and a private associated type
/// in a public trait's interface trips `private_interfaces` under `-D warnings`.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The target path and mask.
    Target,
    /// `+ F7`: create a directory for the target.
    NewDir,
    /// "Only files of this type".
    Mask,
    /// `+ F8`: this session's mask history.
    MaskHistory,
    /// "Preserve attributes".
    Preserve,
    /// "Verify".
    Verify,
    /// The conflict policy, behind `Options >>`.
    Conflict,
    /// Start now.
    Ok,
    /// `F2 Queue`: append to the background queue instead.
    Queue,
    /// Pick the target from a directory tree.
    Tree,
    /// Give up.
    Cancel,
    /// Show or hide the conflict policy.
    Options,
}

/// Which row of the interior a piece of the dialog occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// `Rename/Move 3 file(s) to`.
    Title,
    /// The target field and its `+ F7`.
    Target,
    /// `17.75 G · 523 files · 95 folders`.
    Stats,
    /// `Only files of this type:`.
    MaskLabel,
    /// The second mask and its `+ F8`.
    Mask,
    /// The two checkboxes.
    Checks,
    /// The conflict policy.
    Conflict,
    /// The button row.
    Buttons,
}

/// What is given up first when the interior is too short (the
/// dialog lays itself out against the rectangle it is given, and 60x15 is a
/// supported size, not a degraded one).
///
/// The target field and the buttons are never in this list: a dialog that
/// cannot show where it is copying to, or how to say yes, is not a dialog. The
/// title line goes early because the border already carries the operation and
/// the count; the statistics go late because the design calls them the last
/// chance to notice a mistake.
const DROP_ORDER: [Slot; 6] = [
    Slot::MaskLabel,
    Slot::Title,
    Slot::Checks,
    Slot::Conflict,
    Slot::Mask,
    Slot::Stats,
];

/// The second mask's label, and the string the underline is
/// searched in.
const MASK_LABEL: &str = "Only files of this type:";

/// the `Alt` mnemonics for this dialog.
///
/// `o` and `n` are the program-wide `OK` and `Cancel`.
///
/// Two of them are worth their paragraph:
///
/// * **`Queue` is `u` and not `q`.** `Alt+Q` is the `quit`; a
///   mnemonic that shadowed it would start a background job on a keystroke half
///   the users of this program press to leave (the rule 4, and its only
///   application). `F2` already presses the button from anywhere in the dialog
///   and is printed in its label, so nothing is lost.
/// * **`Options` is `s` and deliberately shadows the `search`**,
///   which is what a dialog mnemonic does: expanding a row of the
///   dialog you are looking at starts nothing and destroys nothing. `s` is also
///   the only letter present in all four label tiers ([`CopyMoveDialog::button_labels`]).
///
/// [`Control::NewDir`] and [`Control::MaskHistory`] have no letter: their
/// labels are the glyphs `[+F7]` and `[+F8]`, which carry nothing to underline,
/// and `F7` and `F8` already reach them from anywhere in the dialog.
pub const MNEMONICS: &[(Control, char)] = &[
    (Control::Target, 't'),
    (Control::Mask, 'f'),
    (Control::Preserve, 'p'),
    (Control::Verify, 'v'),
    (Control::Conflict, 'c'),
    (Control::Ok, 'o'),
    (Control::Queue, 'u'),
    (Control::Tree, 'r'),
    (Control::Cancel, 'n'),
    (Control::Options, 's'),
];

/// The letter each tier of the button row underlines, in the row's own order.
///
/// Beside [`MNEMONICS`] rather than derived from it so the button row's five
/// cells and the table cannot drift; a test asserts they agree.
const BUTTON_MNEMONICS: [Option<char>; 5] = [Some('o'), Some('u'), Some('r'), Some('n'), Some('s')];

/// How wide the interior has to be before the `+ F7` / `+ F8` side buttons are
/// worth the columns they cost.
const SIDE_BUTTON_FLOOR: u16 = 24;

/// The width of `[+F7]` plus the space in front of it.
const SIDE_BUTTON_WIDTH: u16 = 6;

/// How many columns a row of buttons occupies once `[ ` … ` ]` and the two
/// spaces between each pair are counted - the same arithmetic
/// [`crate::dialog::draw_buttons`] does when it decides what to drop.
fn row_width(labels: &[&str]) -> usize {
    let mut used = 0usize;
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            used = used.saturating_add(2);
        }
        used = used.saturating_add(text::width(label)).saturating_add(4);
    }
    // `draw_buttons` needs one spare column beyond the last button.
    used.saturating_add(1)
}

/// the copy / move dialog.
pub struct CopyMoveDialog {
    kind: JobKind,
    count: usize,
    target: Field,
    mask: Field,
    /// This session's file masks, newest first, for `+ F8`.
    history: Vec<String>,
    /// Where `+ F8` is in that list.
    history_pos: usize,
    preserve_attrs: bool,
    verify: bool,
    /// `None` is "ask on the first conflict".
    conflict: Option<ConflictChoice>,
    options_open: bool,
    stats: SelectionStats,
    /// The statistics line's size, already formatted through `panel.human_sizes`
    /// and `panel.thousands_separator` so it matches the panel exactly.
    size_text: String,
    /// A [`crate::ops::JobKind::Size`] walk is still running.
    sizing: bool,
    /// The sources these statistics describe, so a walk that lands while the
    /// dialog is open can be folded in. Empty when nobody called
    /// [`CopyMoveDialog::watch`], which is what the older tests do.
    sources: Vec<VfsPath>,
    /// Sized roots already folded in, so one arriving twice cannot count twice.
    folded: Vec<VfsPath>,
    /// How the byte count is spelled, kept so [`CopyMoveDialog::job_update`]
    /// can re-format one without being handed the configuration again.
    size_cfg: PanelConfig,
    /// When the dialog opened, for the spinner. Wall-clock rather than a frame
    /// counter so the spinner turns at the same speed however often the UI
    /// happens to repaint.
    opened: Instant,
    ring: FocusRing,
}

impl CopyMoveDialog {
    /// A dialog for `count` selected entries, targeting `target`.
    ///
    /// `target` is the other panel's path plus a file mask - `/srv/media/*.*` -
    /// and the mask half arrives preselected. `stats` comes
    /// from [`crate::ops::selection_stats`] and `cfg` only formats its byte
    /// count; the dialog holds no configuration of its own.
    pub fn new(
        kind: JobKind,
        count: usize,
        target: impl Into<String>,
        stats: SelectionStats,
        cfg: &PanelConfig,
    ) -> Self {
        let mut dialog = Self {
            kind,
            count,
            target: Field::with_mask_selected(target),
            mask: Field::with_text("*.*"),
            history: Vec::new(),
            history_pos: 0,
            preserve_attrs: true,
            verify: false,
            conflict: None,
            options_open: false,
            stats,
            size_text: bytes_text(stats.bytes, cfg),
            sizing: false,
            sources: Vec::new(),
            folded: Vec::new(),
            size_cfg: cfg.clone(),
            opened: Instant::now(),
            ring: FocusRing::new(0),
        };
        dialog.rebuild_ring(Control::Target);
        dialog
    }

    /// Take the checkbox defaults from `[ops]`.
    #[must_use]
    pub fn with_config(mut self, ops: &OpsConfig) -> Self {
        self.preserve_attrs = ops.preserve_attrs;
        self
    }

    /// Offer this session's file masks behind `+ F8`.
    #[must_use]
    pub fn with_history(mut self, masks: Vec<String>) -> Self {
        self.history = masks;
        self.history_pos = 0;
        self
    }

    /// Say that a size walk is still running, so the statistics line spins
    /// instead of pretending to be final.
    pub const fn set_sizing(&mut self, sizing: bool) {
        self.sizing = sizing;
    }

    /// Replace the statistics when a walk lands.
    pub fn set_stats(&mut self, stats: SelectionStats, cfg: &PanelConfig) {
        self.stats = stats;
        self.size_text = bytes_text(stats.bytes, cfg);
    }

    /// The sources these statistics describe.
    ///
    /// Naming them is what lets [`Dialog::job_update`] finish the sentence the
    /// spinner starts: a `Ctrl+L` walk that lands while the dialog is open
    /// resolves the `≥` here as well as on the panel status line, instead of
    /// leaving the dialog showing a lower bound the program already knows to be
    /// wrong - on the one line the design calls "the last chance to notice a
    /// mistake".
    pub fn watch(&mut self, sources: Vec<VfsPath>) {
        self.sources = sources;
    }

    /// Point the target field somewhere else - the `+ F7`, once the
    /// directory it created is known.
    pub fn set_target(&mut self, target: impl Into<String>) {
        self.target = Field::with_mask_selected(target);
    }

    /// The statistics currently shown.
    pub const fn stats(&self) -> SelectionStats {
        self.stats
    }

    /// The target field's contents: a path and a mask.
    pub fn target(&self) -> &str {
        self.target.text()
    }

    /// "Only files of this type".
    pub fn file_mask(&self) -> &str {
        self.mask.text()
    }

    /// The "Preserve attributes" checkbox.
    pub const fn preserve_attrs(&self) -> bool {
        self.preserve_attrs
    }

    /// The "Verify" checkbox.
    pub const fn verify(&self) -> bool {
        self.verify
    }

    /// The conflict policy chosen under `Options >>`, `None` for "ask".
    pub const fn conflict(&self) -> Option<ConflictChoice> {
        self.conflict
    }

    /// Whether `Options >>` is expanded.
    pub const fn options_open(&self) -> bool {
        self.options_open
    }

    /// the title line: the operation and the count.
    pub fn title_line(&self) -> String {
        format!("{} to", self.heading())
    }

    /// The operation and the count, without the trailing preposition - the
    /// border's title.
    fn heading(&self) -> String {
        let verb = match self.kind {
            JobKind::Copy => "Copy",
            JobKind::Move => "Rename/Move",
            other => other.title(),
        };
        // `file(s)`, literally, both here and at a count of one: the design
        // spells it that way and the reference screenshot reads
        // `Rename/Move 1 file(s) to`. Pluralising it properly would be a nicer
        // sentence and a different dialog.
        format!("{verb} {} file(s)", self.count)
    }

    /// the statistics line, spinner included while a walk runs.
    pub fn stats_line(&self, ascii: bool) -> String {
        let body = self.stats.describe(&self.size_text, ascii);
        if self.sizing {
            format!("{} {body}", spinner(self.opened.elapsed(), ascii))
        } else {
            body
        }
    }

    /// The controls in `Tab` order. `Options >>` adds one; nothing else moves,
    /// so expanding it cannot make the focus jump somewhere else.
    fn controls(&self) -> Vec<Control> {
        let mut out = vec![
            Control::Target,
            Control::NewDir,
            Control::Mask,
            Control::MaskHistory,
            Control::Preserve,
            Control::Verify,
        ];
        if self.options_open {
            out.push(Control::Conflict);
        }
        out.extend([
            Control::Ok,
            Control::Queue,
            Control::Tree,
            Control::Cancel,
            Control::Options,
        ]);
        out
    }

    /// Which control has focus.
    fn focused(&self) -> Control {
        self.controls()
            .get(self.ring.index())
            .copied()
            .unwrap_or(Control::Target)
    }

    /// Rebuild the ring after `Options >>` changed the control count, keeping
    /// the same control focused.
    fn rebuild_ring(&mut self, keep: Control) {
        let controls = self.controls();
        self.ring = FocusRing::new(controls.len());
        if let Some(index) = controls.iter().position(|c| *c == keep) {
            self.ring.set(index);
        }
    }

    /// The answer this dialog carries.
    fn answer(&self, queue: bool) -> DialogOutcome {
        DialogOutcome::Accept(DialogResult::CopyMove(Box::new(CopyMoveAnswer {
            target: self.target.text().to_string(),
            file_mask: self.mask.text().to_string(),
            preserve_attrs: self.preserve_attrs,
            verify: self.verify,
            queue,
            conflict: self.conflict,
        })))
    }

    /// `OK` and `F2 Queue` both refuse an empty target: an empty destination is
    /// a mistake, and refusing it here beats failing the job.
    fn accept(&self, queue: bool) -> DialogOutcome {
        if self.target.is_empty() {
            return DialogOutcome::Consumed;
        }
        self.answer(queue)
    }

    /// `+ F7`: create a directory **for the target**.
    ///
    /// Its own [`DialogId`], not `F7`'s: the directory belongs under the target
    /// field's path, which with `F5` is the *other* panel, and the answer also
    /// has to come back and point the field at what it made.
    fn new_dir(&self) -> DialogOutcome {
        DialogOutcome::Push(Box::new(InputDialog::new(
            DialogId::MkdirForTarget,
            "Create directory",
            "New directory name:",
            "",
        )))
    }

    /// `+ F8`: cycle this session's file masks into the field.
    fn recall_mask(&mut self) -> DialogOutcome {
        if self.history.is_empty() {
            return DialogOutcome::Push(Box::new(MessageDialog::line(
                "File masks",
                "no file mask has been used in this session yet",
            )));
        }
        let index = self.history_pos.rem_euclid(self.history.len());
        if let Some(mask) = self.history.get(index) {
            let mask = mask.clone();
            self.mask.set_text(mask);
        }
        self.history_pos = index.saturating_add(1);
        DialogOutcome::Consumed
    }

    /// `Tree`: pick the target from a directory tree. The
    /// directory tree itself is the v0.7 - "the rest of TC" - so the
    /// button says which milestone brings it rather than doing nothing.
    fn tree(&self) -> DialogOutcome {
        DialogOutcome::Push(Box::new(MessageDialog::line(
            "Tree",
            format!(
                "target directory tree: not implemented until {}",
                Milestone::V07
            ),
        )))
    }

    /// Step the conflict policy. `None` - "ask on the first conflict" - is the
    /// first entry, so the default is one step from anywhere.
    fn cycle_conflict(&mut self, forward: bool) {
        let all = ConflictChoice::ALL;
        let len = all.len().saturating_add(1);
        let current = self
            .conflict
            .and_then(|c| all.iter().position(|x| *x == c))
            .map_or(0, |i| i.saturating_add(1));
        let next = if forward {
            current.saturating_add(1).rem_euclid(len)
        } else {
            current
                .saturating_add(len)
                .saturating_sub(1)
                .rem_euclid(len)
        };
        self.conflict = if next == 0 {
            None
        } else {
            all.get(next.saturating_sub(1)).copied()
        };
    }

    /// The conflict policy's label.
    fn conflict_label(&self) -> &'static str {
        self.conflict.as_ref().map_or("Ask", ConflictChoice::label)
    }

    /// Which slots this interior has room for, and where each one goes.
    fn rows(&self, area: Rect) -> Vec<(Slot, Rect)> {
        let mut wanted = vec![
            Slot::Title,
            Slot::Target,
            Slot::Stats,
            Slot::MaskLabel,
            Slot::Mask,
            Slot::Checks,
        ];
        if self.options_open {
            wanted.push(Slot::Conflict);
        }
        wanted.push(Slot::Buttons);

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

    /// A field row split into the field itself and its side button.
    fn field_row(rect: Rect) -> (Rect, Option<Rect>) {
        if rect.width < SIDE_BUTTON_FLOOR {
            return (rect, None);
        }
        let field_w = rect.width.saturating_sub(SIDE_BUTTON_WIDTH);
        let field = Rect::new(rect.x, rect.y, field_w, 1);
        let button = Rect::new(
            rect.x.saturating_add(field_w).saturating_add(1),
            rect.y,
            SIDE_BUTTON_WIDTH.saturating_sub(1),
            1,
        );
        (field, Some(button))
    }

    /// Draw one small right-hand button, `[+F7]`.
    fn draw_side_button(
        f: &mut Frame,
        area: Rect,
        label: &str,
        focused: bool,
        style: &DialogStyle,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let body = text::fit_left(
            label,
            usize::from(area.width),
            text::Crop::End,
            ellipsis(style.ascii),
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(body, style.button(focused))))
                .style(style.body()),
            area,
        );
    }

    /// The button row's labels, at the longest spelling that fits.
    ///
    /// the design lists five buttons and the design declares 60x15 usable;
    /// the full row is 59 columns and a 60-column dialog has 54 to give. The
    /// framework's [`draw_buttons`] drops what does not fit **from the right**,
    /// which would silently take `Options >>` away - so the labels shorten
    /// first and all five survive. The same degradation the key bar makes
    /// (`crate::ui::keybar_slots`), for the same reason: a missing slot is
    /// worse than an abbreviated one.
    fn button_labels(&self, width: u16) -> [&'static str; 5] {
        let options: [&'static str; 2] = if self.options_open {
            ["Options <<", "Opts <<"]
        } else {
            ["Options >>", "Opts >>"]
        };
        let long = options.first().copied().unwrap_or("Options");
        let short = options.get(1).copied().unwrap_or("Opts");
        // Longest first; the last is the floor and is taken whether it fits or
        // not, because five stubs still read as five buttons.
        let tiers: [[&'static str; 5]; 4] = [
            ["OK", "F2 Queue", "Tree", "Cancel", long],
            ["OK", "F2 Queue", "Tree", "Cancel", short],
            ["OK", "F2 Queue", "Tree", "Cancel", "Opts"],
            ["OK", "Queue", "Tree", "Cancel", "Opts"],
        ];
        let floor = tiers.last().copied().unwrap_or(["OK", "", "", "", ""]);
        tiers
            .into_iter()
            .find(|labels| row_width(labels) <= usize::from(width))
            .unwrap_or(floor)
    }
}

impl Accelerated for CopyMoveDialog {
    type Control = Control;

    fn mnemonics(&self) -> &'static [(Control, char)] {
        MNEMONICS
    }

    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            // The conflict stepper is behind `Options >>`. While that is
            // collapsed the control is not on the screen, so its letter is
            // swallowed and nothing happens - it does not open the section,
            // because that is a second meaning the user did not ask for.
            //
            Control::Conflict if !self.options_open => Accel::Absent,
            // A stepper is focus-only, and so are the two fields.
            Control::Target | Control::Mask | Control::Conflict => Accel::Focus,
            Control::Preserve | Control::Verify => Accel::Check,
            // `NewDir` and `MaskHistory` are buttons with no letter; naming
            // them here is what lets `F7` and `F8` press them by the same route
            // every other button is pressed by.
            Control::NewDir
            | Control::MaskHistory
            | Control::Ok
            | Control::Queue
            | Control::Tree
            | Control::Cancel
            | Control::Options => Accel::Press,
        }
    }

    fn focus_control(&mut self, control: Control) {
        if let Some(at) = self.controls().iter().position(|c| *c == control) {
            self.ring.set(at);
        }
    }

    /// **on**, never a toggle. A repeated `Alt+P` leaves
    /// `Preserve attributes` ticked; `Space` is how it comes off.
    fn switch_on(&mut self, control: Control) {
        match control {
            Control::Preserve => self.preserve_attrs = true,
            Control::Verify => self.verify = true,
            Control::Target
            | Control::NewDir
            | Control::Mask
            | Control::MaskHistory
            | Control::Conflict
            | Control::Ok
            | Control::Queue
            | Control::Tree
            | Control::Cancel
            | Control::Options => {}
        }
    }

    /// The one place each button's action lives, for `Enter`, for `F2`/`F7`/
    /// `F8` and for `Alt`+letter alike.
    fn press(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Ok => self.accept(false),
            Control::Queue => self.accept(true),
            Control::NewDir => self.new_dir(),
            Control::MaskHistory => self.recall_mask(),
            Control::Tree => self.tree(),
            Control::Cancel => DialogOutcome::Cancel,
            Control::Options => {
                self.options_open = !self.options_open;
                // Keep `Options` itself focused, so the section it opened is a
                // `Tab` away rather than wherever the ring happened to land.
                self.rebuild_ring(Control::Options);
                DialogOutcome::Consumed
            }
            Control::Target
            | Control::Mask
            | Control::Preserve
            | Control::Verify
            | Control::Conflict => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for CopyMoveDialog {
    fn id(&self) -> DialogId {
        DialogId::CopyMove
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// Keep the statistics line in step with the walks that resolve it.
    ///
    ///
    /// Two things, once a frame: the spinner stops when no size walk is left
    /// running, and a walk that finished over one of this dialog's own sources
    /// is added to the figure - which is what makes the `≥` disappear here at
    /// the same moment it disappears from the panel behind.
    fn job_update(&mut self, jobs: &[JobStatus]) {
        for job in jobs {
            let Some(summary) = job.finished.as_deref() else {
                continue;
            };
            for (path, stats) in &summary.sized {
                if !self.sources.iter().any(|s| s == path) || self.folded.iter().any(|s| s == path)
                {
                    continue;
                }
                self.folded.push(path.clone());
                self.stats.bytes = self.stats.bytes.saturating_add(stats.bytes);
                self.stats.files = self.stats.files.saturating_add(stats.files);
                self.stats.dirs = self.stats.dirs.saturating_add(stats.dirs);
                self.stats.unsized_dirs = self.stats.unsized_dirs.saturating_sub(1);
                self.size_text = bytes_text(self.stats.bytes, &self.size_cfg);
            }
        }
        self.sizing = jobs
            .iter()
            .any(|j| j.kind == JobKind::Size && j.finished.is_none());
    }

    fn title(&self) -> String {
        self.heading()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = text::width(&self.title_line())
            .max(text::width(self.target.text()))
            .max(text::width(&self.stats_line(false)))
            // The button row: five buttons, `[ ` and ` ]` each, two spaces
            // between. It is what actually sets the floor.
            .max(58);
        let w = u16::try_from(widest.saturating_add(4)).unwrap_or(u16::MAX);
        let rows = if self.options_open { 8 } else { 7 };
        (w.clamp(46, 78), rows + 2)
    }

    /// Ten letters while `Options >>` is open, nine while it is collapsed.
    ///
    ///
    /// The conflict stepper's `c` drops out with the section it lives in, which
    /// is why [`Dialog::mnemonic_letters`] returns a `Vec` rather than a
    /// `&'static [char]`: the answer is per-instance.
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
        // the mnemonics come first, before anything that reads
        // `key.action` and before either field can see the key.
        // `Alt+S` is `Options` here and never the `search`, which is
        // the context precedence doing what it already does.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }

        // Accelerators, which work without focusing the control they name -
        // the same rule the design states for the progress dialog's two
        // buttons, applied to the two the design labels with a key in them.
        // Each goes through `press`, so a button has one definition and not
        // one per route.
        match key.press.code {
            KeyCode::F(2) => return self.press(Control::Queue),
            KeyCode::F(7) => return self.press(Control::NewDir),
            KeyCode::F(8) => return self.press(Control::MaskHistory),
            _ => {}
        }

        let focused = self.focused();
        if key.is_accept() {
            return match focused {
                Control::NewDir
                | Control::MaskHistory
                | Control::Queue
                | Control::Tree
                | Control::Cancel
                | Control::Options => self.press(focused),
                // A form accepts from its fields and its checkboxes, so the
                // common case is "type the mask, press Enter".
                Control::Target
                | Control::Mask
                | Control::Preserve
                | Control::Verify
                | Control::Conflict
                | Control::Ok => self.press(Control::Ok),
            };
        }

        // Vertical movement walks the form wherever focus is; horizontal
        // movement belongs to the field being edited, and only outside one does
        // it walk controls.
        match key.press.code {
            KeyCode::Down => {
                self.ring.next();
                return DialogOutcome::Consumed;
            }
            KeyCode::Up => {
                self.ring.prev();
                return DialogOutcome::Consumed;
            }
            _ => {}
        }

        match focused {
            Control::Target => {
                if self.target.handle(key) {
                    return DialogOutcome::Consumed;
                }
            }
            Control::Mask => {
                if self.mask.handle(key) {
                    return DialogOutcome::Consumed;
                }
            }
            Control::Preserve | Control::Verify => {
                if key.text() == Some(' ') {
                    // `Space` is the toggle; `Alt`+letter is the switch-on,
                    // and they are deliberately different.
                    if focused == Control::Preserve {
                        self.preserve_attrs = !self.preserve_attrs;
                    } else {
                        self.verify = !self.verify;
                    }
                    return DialogOutcome::Consumed;
                }
            }
            Control::Conflict => match key.press.code {
                KeyCode::Left => {
                    self.cycle_conflict(false);
                    return DialogOutcome::Consumed;
                }
                KeyCode::Right => {
                    self.cycle_conflict(true);
                    return DialogOutcome::Consumed;
                }
                _ => {
                    if key.text() == Some(' ') {
                        self.cycle_conflict(true);
                        return DialogOutcome::Consumed;
                    }
                }
            },
            Control::NewDir
            | Control::MaskHistory
            | Control::Ok
            | Control::Queue
            | Control::Tree
            | Control::Cancel
            | Control::Options => {}
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
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let body: Style = style.body();
        let focused = self.focused();
        for (slot, rect) in self.rows(area) {
            match slot {
                // The `t` of the trailing `to`, which the word-start rule of
                // the design picks over any `t` inside the
                // verb. This line is the target field's label.
                Slot::Title => draw_mnemonic(f, rect, &self.title_line(), 't', body, style.ascii),
                Slot::Target => {
                    let (field, button) = Self::field_row(rect);
                    self.target.render(f, field, style);
                    if let Some(button) = button {
                        Self::draw_side_button(
                            f,
                            button,
                            "[+F7]",
                            focused == Control::NewDir,
                            style,
                        );
                    }
                }
                Slot::Stats => draw_text(f, rect, &self.stats_line(style.ascii), body, style.ascii),
                Slot::MaskLabel => {
                    draw_mnemonic(f, rect, MASK_LABEL, 'f', body, style.ascii);
                }
                Slot::Mask => {
                    let (field, button) = Self::field_row(rect);
                    self.mask.render(f, field, style);
                    if let Some(button) = button {
                        Self::draw_side_button(
                            f,
                            button,
                            "[+F8]",
                            focused == Control::MaskHistory,
                            style,
                        );
                    }
                }
                Slot::Checks => {
                    let preserve =
                        checkbox("Preserve attributes", self.preserve_attrs, style.ascii);
                    let verify = checkbox("Verify", self.verify, style.ascii);
                    let left_style = style.button(focused == Control::Preserve);
                    let right_style = style.button(focused == Control::Verify);
                    let width = usize::from(rect.width);
                    let right_w = text::width(&verify);
                    if right_w.saturating_add(4) <= width {
                        // `Verify` stays hard against the right-hand edge, so
                        // the padding gives up the two columns
                        // `draw_mnemonic_pieces` puts between its pieces.
                        let room = width.saturating_sub(right_w).saturating_sub(2);
                        let head =
                            text::fit_left(&preserve, room, text::Crop::End, ellipsis(style.ascii));
                        let pieces = [
                            Piece::new(head, Some('p'), left_style, focused == Control::Preserve),
                            Piece::new(verify, Some('v'), right_style, focused == Control::Verify),
                        ];
                        draw_mnemonic_pieces(f, rect, &pieces, body);
                    } else {
                        draw_mnemonic(f, rect, &preserve, 'p', left_style, style.ascii);
                    }
                }
                Slot::Conflict => {
                    let text = format!(
                        "On conflict:  {} {} {}",
                        if style.ascii { "<" } else { "\u{2039}" },
                        self.conflict_label(),
                        if style.ascii { ">" } else { "\u{203a}" },
                    );
                    draw_mnemonic(
                        f,
                        rect,
                        &text,
                        'c',
                        style.button(focused == Control::Conflict),
                        style.ascii,
                    );
                }
                Slot::Buttons => {
                    let labels = self.button_labels(rect.width);
                    let index = match focused {
                        Control::Ok => 0,
                        Control::Queue => 1,
                        Control::Tree => 2,
                        Control::Cancel => 3,
                        Control::Options => 4,
                        // Focus is on a field, a checkbox or the stepper, so no
                        // button is highlighted.
                        Control::Target
                        | Control::NewDir
                        | Control::Mask
                        | Control::MaskHistory
                        | Control::Preserve
                        | Control::Verify
                        | Control::Conflict => usize::MAX,
                    };
                    let with_letters: Vec<(&str, Option<char>)> = labels
                        .iter()
                        .zip(BUTTON_MNEMONICS)
                        .map(|(label, letter)| (*label, letter))
                        .collect();
                    draw_mnemonic_buttons(f, rect, &with_letters, index, style);
                }
            }
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let focused = self.focused();
        let slot = match focused {
            Control::Target => Slot::Target,
            Control::Mask => Slot::Mask,
            // Nothing else in this dialog carries a caret.
            Control::NewDir
            | Control::MaskHistory
            | Control::Preserve
            | Control::Verify
            | Control::Conflict
            | Control::Ok
            | Control::Queue
            | Control::Tree
            | Control::Cancel
            | Control::Options => return None,
        };
        let rect = self
            .rows(area)
            .into_iter()
            .find_map(|(s, r)| (s == slot).then_some(r))?;
        let (field, _) = Self::field_row(rect);
        if focused == Control::Target {
            self.target.cursor(field)
        } else {
            self.mask.cursor(field)
        }
    }
}

impl std::fmt::Debug for CopyMoveDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopyMoveDialog")
            .field("kind", &self.kind)
            .field("count", &self.count)
            .field("target", &self.target.text())
            .field("mask", &self.mask.text())
            .field("preserve_attrs", &self.preserve_attrs)
            .field("verify", &self.verify)
            .field("conflict", &self.conflict)
            .field("focused", &self.focused())
            .finish()
    }
}

#[cfg(test)]
impl CopyMoveDialog {
    /// Pretend the dialog opened `ago` in the past, so the spinner's phase is
    /// a fixed value rather than whatever the test machine's clock says.
    fn opened_ago(&mut self, ago: std::time::Duration) {
        if let Some(then) = Instant::now().checked_sub(ago) {
            self.opened = then;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::{KeyModifiers, KeyPress};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::time::Duration;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn stats() -> SelectionStats {
        SelectionStats {
            bytes: 19_058_360_320,
            files: 523,
            dirs: 95,
            unsized_dirs: 0,
        }
    }

    fn human() -> PanelConfig {
        PanelConfig {
            human_sizes: true,
            ..PanelConfig::default()
        }
    }

    fn dialog() -> CopyMoveDialog {
        CopyMoveDialog::new(JobKind::Move, 3, "/srv/media/*.*", stats(), &human())
    }

    /// `Alt`+letter, the keystroke the design is about.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    /// Every character drawn with [`ratatui::style::Modifier::UNDERLINED`],
    /// folded to lower case. the underline, read off the buffer
    /// rather than off the table.
    fn underlined(d: &CopyMoveDialog, w: u16, h: u16) -> Vec<char> {
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

    fn style(ascii: bool) -> DialogStyle {
        DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii)
    }

    fn render(d: &CopyMoveDialog, w: u16, h: u16, ascii: bool) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let st = style(ascii);
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

    fn typed(d: &mut CopyMoveDialog, text: &str) {
        for c in text.chars() {
            d.handle_key(&key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn the_title_line_states_the_operation_and_the_count() {
        // taken verbatim from the reference screenshot.
        assert_eq!(dialog().title_line(), "Rename/Move 3 file(s) to");
        let one = CopyMoveDialog::new(JobKind::Copy, 1, "/tmp/*.*", stats(), &human());
        assert_eq!(one.title_line(), "Copy 1 file(s) to");
    }

    #[test]
    fn typing_replaces_the_preselected_mask_and_leaves_the_path() {
        // The point of the dialog.
        let mut d = dialog();
        typed(&mut d, "*.bak");
        assert_eq!(d.target(), "/srv/media/*.bak");
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::CopyMove(answer)) => {
                assert_eq!(answer.target, "/srv/media/*.bak");
                assert!(!answer.queue);
            }
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn the_statistics_line_is_the_one_from_the_screenshot() {
        // `17.75 G · 523 files · 95 folders`. The size is whatever the `size`
        // column would print for the same byte count, which under
        // `panel.human_sizes` is `18 G` - the own example reads `17.75 G`, two
        // decimals that the format cannot produce. Matching the panel is the
        // constraint in words; the screenshot's figure is a screenshot of a
        // different program's formatter.
        let d = dialog();
        assert_eq!(
            d.stats_line(false),
            "18 G \u{b7} 523 files \u{b7} 95 folders"
        );
        assert_eq!(d.stats_line(true), "18 G - 523 files - 95 folders");
        assert!(d.stats_line(true).is_ascii());
    }

    #[test]
    fn an_unsized_directory_makes_the_total_a_lower_bound() {
        // never a computed-looking total that is partial.
        let cfg = human();
        let mut partial = stats();
        partial.unsized_dirs = 2;
        let mut d = CopyMoveDialog::new(JobKind::Copy, 3, "/tmp/*.*", partial, &cfg);
        assert!(
            d.stats_line(false).starts_with('\u{2265}'),
            "{}",
            d.stats_line(false)
        );
        assert!(
            d.stats_line(true).starts_with(">="),
            "{}",
            d.stats_line(true)
        );

        // And a running walk spins rather than blocking.
        d.set_sizing(true);
        d.opened_ago(Duration::from_millis(0));
        assert!(
            d.stats_line(true).starts_with('|'),
            "{}",
            d.stats_line(true)
        );
        d.opened_ago(Duration::from_millis(120));
        assert!(
            d.stats_line(true).starts_with('/'),
            "{}",
            d.stats_line(true)
        );

        // A walk that lands replaces the figure and the bound goes.
        d.set_sizing(false);
        d.set_stats(stats(), &cfg);
        assert!(!d.stats_line(false).contains('\u{2265}'));
    }

    /// the statistics line is "the last chance to
    /// notice a mistake", so it keeps up with the walk that resolves it. The
    /// dialog is handed the live job table every frame
    /// ([`crate::app::App::sync_job_dialogs`]); before this it ignored it, so
    /// the spinner turned for as long as the dialog was open and the figure
    /// stayed at the lower bound it was born with.
    #[test]
    fn a_size_walk_that_lands_resolves_the_dialogs_own_figure() {
        let cfg = human();
        let partial = SelectionStats {
            bytes: 0,
            files: 0,
            dirs: 1,
            unsized_dirs: 1,
        };
        let mut d = CopyMoveDialog::new(JobKind::Copy, 1, "/srv/media/*.*", partial, &cfg);
        d.watch(vec![VfsPath::local("/home/t/tree")]);
        d.set_sizing(true);
        assert!(d.stats_line(false).contains('\u{2265}'), "the premise");

        // The walk is still running.
        let mut walking = JobStatus::queued(crate::ops::JobId(1), JobKind::Size);
        walking.started = true;
        d.job_update(std::slice::from_ref(&walking));
        assert!(d.stats_line(true).contains(">="), "still a lower bound");

        // It finishes over this dialog's own source.
        walking.apply(&crate::ops::JobEvent::Finished {
            summary: Box::new(crate::ops::JobSummary {
                kind: JobKind::Size,
                files_done: 47_000,
                dirs_done: 0,
                bytes_done: 20_000_000_000,
                skipped: 0,
                failures: Vec::new(),
                cancelled: false,
                elapsed: Duration::ZERO,
                sized: vec![(
                    VfsPath::local("/home/t/tree"),
                    crate::ops::TreeStats {
                        bytes: 20_000_000_000,
                        files: 47_000,
                        dirs: 12,
                    },
                )],
                differing: Vec::new(),
                first_difference: None,
            }),
        });
        d.job_update(&[walking.clone()]);

        let line = d.stats_line(false);
        assert!(!line.contains('\u{2265}'), "the bound is resolved: {line}");
        assert!(line.contains("47000") || line.contains("47,000"), "{line}");
        assert_eq!(d.stats().bytes, 20_000_000_000);
        assert!(
            !line.starts_with('\u{25D0}') && !line.starts_with('|'),
            "and the spinner stopped: {line}"
        );

        // A second delivery of the same walk cannot count it twice.
        d.job_update(&[walking]);
        assert_eq!(d.stats().files, 47_000);
    }

    #[test]
    fn f2_queues_from_anywhere_and_ok_starts_now() {
        // "F2 Queue (append to the background queue instead of
        // starting now)".
        let mut d = dialog();
        match d.handle_key(&key(KeyCode::F(2))) {
            DialogOutcome::Accept(DialogResult::CopyMove(answer)) => assert!(answer.queue),
            other => panic!("F2 should queue, got {other:?}"),
        }
        // And the button, reached by Tab, does the same.
        let mut d = dialog();
        while d.focused() != Control::Queue {
            d.handle_key(&key(KeyCode::Tab));
        }
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::CopyMove(answer)) => assert!(answer.queue),
            other => panic!("the F2 Queue button should queue, got {other:?}"),
        }
    }

    #[test]
    fn f7_opens_a_create_directory_prompt_on_top() {
        // "A + F7 button creates a new directory **for the
        // target**." Its own id, because the plain `F7` arm creates under the
        // active panel and with `F5` the target is the other one.
        let mut d = dialog();
        match d.handle_key(&key(KeyCode::F(7))) {
            DialogOutcome::Push(next) => assert_eq!(next.id(), DialogId::MkdirForTarget),
            other => panic!("expected a pushed prompt, got {other:?}"),
        }
    }

    #[test]
    fn f8_recalls_a_mask_and_says_so_when_there_is_no_history() {
        let mut d = dialog();
        match d.handle_key(&key(KeyCode::F(8))) {
            DialogOutcome::Push(next) => assert_eq!(next.id(), DialogId::Message),
            other => panic!("expected the empty-history message, got {other:?}"),
        }

        let mut d = dialog().with_history(vec!["*.rs".to_string(), "*.toml".to_string()]);
        d.handle_key(&key(KeyCode::F(8)));
        assert_eq!(d.file_mask(), "*.rs");
        d.handle_key(&key(KeyCode::F(8)));
        assert_eq!(d.file_mask(), "*.toml");
        d.handle_key(&key(KeyCode::F(8)));
        assert_eq!(d.file_mask(), "*.rs", "and it wraps");
    }

    #[test]
    fn the_tree_button_names_the_milestone_that_brings_it() {
        let mut d = dialog();
        while d.focused() != Control::Tree {
            d.handle_key(&key(KeyCode::Tab));
        }
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Push(next) => assert_eq!(next.id(), DialogId::Message),
            other => panic!("expected the stub message, got {other:?}"),
        }
    }

    #[test]
    fn the_checkboxes_toggle_with_space_and_travel_in_the_answer() {
        let mut d = dialog();
        assert!(d.preserve_attrs());
        assert!(!d.verify());
        while d.focused() != Control::Preserve {
            d.handle_key(&key(KeyCode::Tab));
        }
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(!d.preserve_attrs());
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), Control::Verify);
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(d.verify());

        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::CopyMove(answer)) => {
                assert!(!answer.preserve_attrs);
                assert!(answer.verify);
            }
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn options_expands_the_conflict_policy_and_cycles_it() {
        let mut d = dialog();
        assert!(!d.options_open());
        assert_eq!(d.conflict(), None);
        while d.focused() != Control::Options {
            d.handle_key(&key(KeyCode::Tab));
        }
        d.handle_key(&key(KeyCode::Enter));
        assert!(d.options_open());
        assert_eq!(d.focused(), Control::Options, "focus stayed on the button");

        while d.focused() != Control::Conflict {
            d.handle_key(&key(KeyCode::Tab));
        }
        d.handle_key(&key(KeyCode::Right));
        assert_eq!(d.conflict(), Some(ConflictChoice::Overwrite));
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(d.conflict(), None, "Ask is the first entry");
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(
            d.conflict(),
            ConflictChoice::ALL.last().copied(),
            "and it wraps backwards"
        );

        // Collapsing it again does not lose the choice.
        while d.focused() != Control::Options {
            d.handle_key(&key(KeyCode::Tab));
        }
        d.handle_key(&key(KeyCode::Enter));
        assert!(!d.options_open());
        assert_eq!(d.conflict(), ConflictChoice::ALL.last().copied());
    }

    #[test]
    fn esc_cancels_and_an_empty_target_is_refused() {
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));

        let mut d = dialog();
        // Backspace once removes the whole preselected mask, then clear the rest.
        for _ in 0..40 {
            d.handle_key(&key(KeyCode::Backspace));
        }
        assert_eq!(d.target(), "");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));
    }

    #[test]
    fn tab_and_shift_tab_walk_every_control_and_wrap() {
        let mut d = dialog();
        let controls = d.controls();
        assert_eq!(d.focused(), Control::Target);
        for expected in controls.iter().skip(1) {
            d.handle_key(&key(KeyCode::Tab));
            assert_eq!(d.focused(), *expected);
        }
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), Control::Target, "it wraps");
        d.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(d.focused(), Control::Options, "and the other way");
    }

    #[test]
    fn it_renders_at_every_size_the_spec_names() {
        // 60x15 is a supported size.
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                let mut d = dialog();
                d.options_open = true;
                d.rebuild_ring(Control::Target);
                let out = dump(&render(&d, w, h, ascii));
                assert!(out.contains("OK"), "{w}x{h} ascii={ascii}:\n{out}");
                assert!(out.contains("/srv/media"), "{w}x{h}:\n{out}");
            }
        }
    }

    #[test]
    fn all_five_buttons_survive_the_minimum_width() {
        // the design lists five and the design declares 60x15 usable, so at
        // 60 the labels shorten rather than `Options >>` silently going.
        let d = dialog();
        for width in [54u16, 56, 58, 60, 100] {
            let labels = d.button_labels(width);
            assert_eq!(labels.len(), 5, "{width}");
            assert!(labels.iter().all(|l| !l.is_empty()), "{width}: {labels:?}");
            assert!(
                row_width(&labels) <= usize::from(width),
                "{width}: {labels:?} needs {}",
                row_width(&labels)
            );
        }
        assert_eq!(d.button_labels(100)[4], "Options >>");
        assert_eq!(d.button_labels(54)[4], "Opts");

        // And what is drawn is what was measured: nothing is dropped.
        let out = dump(&render(&dialog(), 60, 15, false));
        for label in ["OK", "F2 Queue", "Tree", "Cancel", "Opts"] {
            assert!(out.contains(label), "{label} is missing at 60x15:\n{out}");
        }
    }

    #[test]
    fn every_glyph_it_draws_has_an_ascii_form() {
        // the separator, the bound, the spinner and the ellipsis.
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (24, 4)] {
            let mut d = dialog();
            d.options_open = true;
            d.rebuild_ring(Control::Target);
            d.set_sizing(true);
            let inner = dump(&render_inner(&d, w, h, true));
            assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
        }
    }

    #[test]
    fn the_statistics_and_the_target_survive_the_smallest_terminal() {
        // The two things the design calls out by name are the last to go.
        let d = dialog();
        let out = dump(&render(&d, 60, 15, false));
        assert!(out.contains("523 files"), "the statistics line:\n{out}");
        assert!(out.contains("/srv/media"), "the target:\n{out}");
        assert!(out.contains("Rename/Move 3 file(s)"), "the count:\n{out}");
    }

    #[test]
    fn a_short_interior_drops_rows_in_a_fixed_order_and_never_the_target() {
        let d = dialog();
        for height in 1u16..12 {
            let area = Rect::new(0, 0, 60, height);
            let rows = d.rows(area);
            assert!(rows.len() <= usize::from(height), "{height}: {rows:?}");
            let slots: Vec<Slot> = rows.iter().map(|(s, _)| *s).collect();
            if height >= 2 {
                assert!(slots.contains(&Slot::Target), "{height}: {slots:?}");
                assert!(slots.contains(&Slot::Buttons), "{height}: {slots:?}");
            }
            // No two slots share a row.
            let mut ys: Vec<u16> = rows.iter().map(|(_, r)| r.y).collect();
            ys.sort_unstable();
            ys.dedup();
            assert_eq!(ys.len(), rows.len(), "{height}: overlapping rows");
            for (_, rect) in &rows {
                assert!(rect.bottom() <= area.bottom(), "{height}: {rect:?}");
                assert!(rect.width > 0 && rect.height > 0, "{height}: {rect:?}");
            }
        }
    }

    #[test]
    fn a_zero_sized_interior_draws_nothing_and_survives() {
        let d = dialog();
        for (w, h) in [(0u16, 0u16), (1, 1), (12, 3), (13, 4)] {
            let _ = render(&d, w.max(1), h.max(1), false);
            let rows = d.rows(Rect::new(0, 0, w, h));
            for (_, rect) in rows {
                assert!(rect.width > 0 && rect.height > 0, "{w}x{h}: {rect:?}");
            }
        }
    }

    #[test]
    fn the_cursor_follows_the_focused_field_and_leaves_when_a_button_has_focus() {
        let mut d = dialog();
        let area = Rect::new(1, 1, 58, 13);
        let (_, y) = d.cursor(area).expect("the target field has the cursor");
        let target_row = d
            .rows(area)
            .into_iter()
            .find_map(|(s, r)| (s == Slot::Target).then_some(r))
            .expect("a target row");
        assert_eq!(y, target_row.y);

        while d.focused() != Control::Ok {
            d.handle_key(&key(KeyCode::Tab));
        }
        assert_eq!(d.cursor(area), None, "a button does not own the caret");
    }

    #[test]
    fn every_control_with_a_letter_is_reachable_by_it() {
        // the design, control by control. The two fields and the conflict
        // stepper are focus-only; the checkboxes tick; the buttons press.
        let mut d = dialog();
        d.handle_key(&alt('s'));
        assert!(d.options_open(), "Alt+S expanded the options row");
        for (letter, want) in [
            ('t', Control::Target),
            ('f', Control::Mask),
            ('c', Control::Conflict),
        ] {
            d.handle_key(&alt(letter));
            assert_eq!(d.focused(), want, "Alt+{letter}");
        }
        assert_eq!(d.conflict(), None, "the stepper was focused, not stepped");
        assert_eq!(d.target(), "/srv/media/*.*", "and nothing was typed");

        let mut d = dialog();
        match d.handle_key(&alt('o')) {
            DialogOutcome::Accept(DialogResult::CopyMove(answer)) => {
                assert!(!answer.queue, "Alt+O is OK, not Queue");
            }
            other => panic!("Alt+O pressed OK, got {other:?}"),
        }

        let mut d = dialog();
        match d.handle_key(&alt('u')) {
            DialogOutcome::Accept(DialogResult::CopyMove(answer)) => {
                assert!(answer.queue, "Alt+U is F2 Queue");
            }
            other => panic!("Alt+U pressed F2 Queue, got {other:?}"),
        }

        let mut d = dialog();
        assert!(matches!(d.handle_key(&alt('r')), DialogOutcome::Push(_)));
        assert_eq!(d.focused(), Control::Tree, "and focus landed on it first");

        let mut d = dialog();
        assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));
    }

    #[test]
    fn alt_q_is_still_quit_and_never_starts_a_background_job() {
        // the rule 4, and its only application:
        // `Alt+Q` is the `quit`, and a mnemonic that shadowed it
        // would start a background job on a keystroke half the users of this
        // program press to leave. `F2 Queue` is `Alt+U` for that reason.
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&alt('q')),
            DialogOutcome::Ignored | DialogOutcome::Consumed
        ));
        assert!(!d.mnemonic_letters().contains(&'q'));
        assert!(d.mnemonic_letters().contains(&'u'));
    }

    #[test]
    fn a_mnemonic_never_turns_a_checkbox_off() {
        // "a key that enabled on the way in and disabled on the
        // way back would make a repeated keystroke destructive". `Space` is the
        // toggle; `Alt`+letter only ever ticks.
        let mut d = dialog();
        assert!(!d.verify());
        d.handle_key(&alt('v'));
        assert!(d.verify(), "Alt+V ticked it");
        d.handle_key(&alt('v'));
        assert!(d.verify(), "Alt+V again left it ticked");

        // And one that starts on stays on, rather than being toggled off by
        // the key that names it.
        assert!(d.preserve_attrs());
        d.handle_key(&alt('p'));
        assert!(d.preserve_attrs(), "Alt+P left Preserve attributes on");
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(!d.preserve_attrs(), "and Space is still the toggle");
    }

    #[test]
    fn a_collapsed_options_row_swallows_its_letter() {
        // the conflict stepper is not on
        // the screen while `Options >>` is collapsed, so `Alt+C` is consumed
        // and nothing happens. It does **not** open the section, because that
        // is a second meaning the user did not ask for.
        let mut d = dialog();
        assert!(!d.options_open());
        assert!(matches!(d.handle_key(&alt('c')), DialogOutcome::Consumed));
        assert!(!d.options_open(), "it did not expand the row");
        assert_eq!(d.conflict(), None);
        assert!(!d.mnemonic_letters().contains(&'c'), "nor advertised");

        d.handle_key(&alt('s'));
        assert!(d.mnemonic_letters().contains(&'c'), "and back once open");
    }

    #[test]
    fn a_dropped_title_line_does_not_disable_its_key() {
        // the design I9. The title line is second in `DROP_ORDER`,
        // so a short enough interior loses it - and with it the underline that
        // advertises `Alt+T`. The key still focuses the target field, because a
        // mnemonic is a property of the control and not of the paint.
        let d = dialog();
        let (_, want_h) = d.size_hint();
        // `size_hint` counts the border; `rows` lays out the interior.
        let roomy = want_h.saturating_sub(2);
        let cramped = roomy.saturating_sub(2);

        let has_title = |h: u16| -> bool {
            let area = Rect::new(0, 0, 60, h);
            d.rows(area).iter().any(|(slot, _)| *slot == Slot::Title)
        };
        assert!(has_title(roomy), "the title line fits at {roomy} rows");
        assert!(!has_title(cramped), "and is gone at {cramped}");

        let mut d = dialog();
        d.handle_key(&key(KeyCode::Tab));
        assert_ne!(d.focused(), Control::Target);
        d.handle_key(&alt('t'));
        assert_eq!(d.focused(), Control::Target, "with the label on screen");

        // And with the row dropped: the same key, the same answer. Nothing in
        // `handle_key` consults the layout, so a mnemonic is a property of the
        // control rather than of the paint.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Tab));
        let screen = dump(&render(&d, 60, cramped.saturating_add(2), false));
        assert!(
            !screen.contains("file(s) to"),
            "the title line is gone:\n{screen}"
        );
        d.handle_key(&alt('t'));
        assert_eq!(d.focused(), Control::Target, "with the label dropped");
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
        }
        // The button row's letters are a second list, so they are checked
        // against the first rather than trusted.
        let mut d = dialog();
        d.handle_key(&alt('s'));
        for width in [80u16, 60, 46] {
            let labels = d.button_labels(width);
            let controls = [
                Control::Ok,
                Control::Queue,
                Control::Tree,
                Control::Cancel,
                Control::Options,
            ];
            for ((label, letter), control) in labels.iter().zip(BUTTON_MNEMONICS).zip(controls) {
                assert_eq!(letter, d.mnemonic_of(control), "{control:?} at {width}");
                let found = letter.and_then(|c| crate::dialog::split_mnemonic(label, c));
                assert!(found.is_some(), "{label:?} has no {letter:?} to underline");
            }
        }
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // Both states of the collapsible section (the design I3),
        // at a size where nothing is cropped or dropped.
        for open in [false, true] {
            let mut d = dialog();
            if open {
                d.handle_key(&alt('s'));
            }
            assert_eq!(d.options_open(), open);
            let mut want = d.mnemonic_letters();
            let mut got = underlined(&d, 100, 24);
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "options_open={open}: underlines on screen");
        }
    }
}
