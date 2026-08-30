//! The Multi-Rename Tool.
//!
//! ```text
//! ┌───────────────── Multi-Rename Tool: 4 file(s) ──────────────────┐
//! │ Rename mask: file name              Extension                   │
//! │ [N]_[C]                             [E]                         │
//! │ Search for            Replace with  [ ]1x [ ][E] [ ]RegEx …     │
//! │ report                summary                                   │
//! │ Case: Unchanged   Counter: start 10  step 5  digits 3           │
//! │ Old name  Ext  New name   Size  Date          Location   Status │
//! │ a         txt  a_010.txt    10  2026-08-28 …  …/media           │
//! │ b         txt  b_015.txt    10  2026-08-28 …  …/media           │
//! │ 4 of 4 rows change                                              │
//! │ [ Start! ] [ Undo ] [ Result list ] [ Reset ] [ Close ]         │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! the four control groups, the preview table and the action row. The model
//! underneath is [`crate::rename`]: the dialog holds the controls, and every
//! question about what a name becomes or whether it can be used is [`Plan`]'s.
//!
//! # The two rules that make it a dialog and not a program
//!
//! * **It reads nothing.** [`Plan::build`] is pure, the rows and the sibling
//!   names arrive at construction, and the New name column is recomputed on
//!   every keystroke in any control from memory alone (
//!   the design).
//! * **`Start!` is disabled while any collision stands**, and
//!   [`crate::rename::plan::RenameStatus::blocks`] is the whole of that
//!   sentence - the button consults the plan, never a second opinion.
//!
//! # At 60 by 15
//!
//! Below [`PREVIEW_MIN_WIDTH`] columns or [`PREVIEW_MIN_HEIGHT`] rows the
//! dialog draws the two mask fields, the counter, the error line and the action
//! row, and **no preview table**, with one line reading `preview needs a wider
//! terminal`. It never draws a broken table.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::field::Field;
use super::tabbed::{Table, TableColumn};
use super::{checkbox, row};
use crate::config::PanelConfig;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    MultiRenameAnswer, Piece, draw_mnemonic, draw_mnemonic_buttons, draw_mnemonic_pieces,
    draw_text,
};
use crate::input::{Action, DialogId, KeyCode, KeyModifiers};
use crate::panel::format::{date_text, size_text};
use crate::rename::mask::DatePart;
use crate::rename::plan::{Plan, PreviewColumn, Settings};
use crate::rename::replace::Case;
use crate::ui::text;
use crate::vfs::{Entry, VfsPath};

/// The narrowest interior that gets a preview table.
pub const PREVIEW_MIN_WIDTH: u16 = 72;

/// The shortest dialog that gets one.
pub const PREVIEW_MIN_HEIGHT: u16 = 18;

/// the columns, in its order, with the status column last.
///
/// The headers are [`PreviewColumn::header`]'s, and a test asserts they still
/// are: two lists of column headings that can disagree is one list too many.
const PREVIEW: &[TableColumn] = &[
    TableColumn {
        header: "Old name",
        min: 10,
        weight: 3,
        flexible: true,
        right: false,
        crop_left: false,
    },
    TableColumn {
        header: "Ext",
        min: 6,
        weight: 0,
        flexible: false,
        right: false,
        crop_left: false,
    },
    TableColumn {
        header: "New name",
        min: 10,
        weight: 3,
        flexible: true,
        right: false,
        crop_left: false,
    },
    TableColumn {
        header: "Size",
        min: 10,
        weight: 0,
        flexible: false,
        right: true,
        crop_left: false,
    },
    TableColumn {
        header: "Date",
        min: 16,
        weight: 0,
        flexible: false,
        right: false,
        crop_left: false,
    },
    TableColumn {
        header: "Location",
        min: 8,
        weight: 2,
        flexible: true,
        right: false,
        // Contract, the tail of a path identifies it.
        crop_left: true,
    },
    TableColumn {
        header: "Status",
        min: 9,
        weight: 0,
        flexible: false,
        right: false,
        crop_left: false,
    },
];

/// One focusable control, in `Tab` order.
///
/// `pub` because it is [`Accelerated::Control`], and a private associated type
/// in a public trait's interface trips `private_interfaces` under `-D warnings`.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// the first combo, default `[N]`.
    NameMask,
    /// Its second, default `[E]`.
    ExtMask,
    /// `Search for`.
    Search,
    /// `Replace with`.
    With,
    /// `1x`.
    FirstOnly,
    /// `[E]`: replace in the extension too.
    IncludeExt,
    /// `RegEx`.
    Regex,
    /// `Subst.`.
    Substitute,
    /// The `^` control, which is case sensitivity.
    MatchCase,
    /// The Upper/lowercase dropdown.
    Case,
    /// `Start at`.
    CounterStart,
    /// `Step by`.
    CounterStep,
    /// `Digits`.
    CounterDigits,
    /// The preview table.
    Preview,
    /// `Start!`.
    Start,
    /// `Undo`.
    Undo,
    /// `Result list`.
    ResultList,
    /// The reset control, also `Ctrl+R`.
    Reset,
    /// `Close`.
    Close,
}

/// Every control, in `Tab` order. The ring's length is this.
const CONTROLS: &[Control] = &[
    Control::NameMask,
    Control::ExtMask,
    Control::Search,
    Control::With,
    Control::FirstOnly,
    Control::IncludeExt,
    Control::Regex,
    Control::Substitute,
    Control::MatchCase,
    Control::Case,
    Control::CounterStart,
    Control::CounterStep,
    Control::CounterDigits,
    Control::Preview,
    Control::Start,
    Control::Undo,
    Control::ResultList,
    Control::Reset,
    Control::Close,
];

/// The name mask's label, and the string the underline is searched
/// in: the word-start rule of the design puts the `m` on
/// `mask` rather than on the `m` inside `Rename`.
const NAME_MASK_LABEL: &str = "Rename mask: file name";
/// The extension mask's label.
const EXT_MASK_LABEL: &str = "Extension";
/// The search field's label. `Alt+S` is `Search for` here and in the Find
/// dialog: two dialogs, one habit.
const SEARCH_LABEL: &str = "Search for";
/// The replacement field's label.
const WITH_LABEL: &str = "Replace with";
/// The counter group's own label, which names no control and therefore
/// underlines nothing.
const COUNTER_LABEL: &str = "Counter:";

/// the `Alt` mnemonics for this dialog.
///
/// Nineteen controls and sixteen letters. Three go without, each for a stated
/// reason:
///
/// * [`Control::IncludeExt`], whose whole label is the placeholder glyph `[E]`
///   and whose only letter is the `e` [`Control::ExtMask`] already has;
/// * [`Control::Case`], whose letters are all spent by the time it is
///   assigned, and a combo is the cheapest thing to lose because the design
///   gives an accelerator on one no meaning anyway;
/// * [`Control::Preview`], which has no label to underline, and which
///   `Ctrl+1`..`Ctrl+7` already sort from anywhere.
///
/// `Alt+N` is deliberately unused: this dialog has no `Cancel`, and the design
/// reserves the letter rather than spending it on something else. `Esc` closes.
///
/// `h` on `Match case` is forced - `m`, `a`, `t`, `c`, `s` and `e` are all
/// taken by the time it is assigned, and `h` is the only letter left in its
/// label.
pub const MNEMONICS: &[(Control, char)] = &[
    (Control::NameMask, 'm'),
    (Control::ExtMask, 'e'),
    (Control::Search, 's'),
    (Control::With, 'w'),
    (Control::FirstOnly, 'x'),
    (Control::Regex, 'g'),
    (Control::Substitute, 'b'),
    (Control::MatchCase, 'h'),
    (Control::CounterStart, 't'),
    (Control::CounterStep, 'p'),
    (Control::CounterDigits, 'd'),
    (Control::Start, 'a'),
    (Control::Undo, 'u'),
    (Control::ResultList, 'l'),
    (Control::Reset, 'r'),
    (Control::Close, 'c'),
];

/// The letter each cell of the action row underlines, in the row's own order.
///
/// Beside [`MNEMONICS`] rather than derived from it so the five cells and the
/// table cannot drift; a test asserts they agree.
const BUTTON_MNEMONICS: [Option<char>; 5] = [Some('a'), Some('u'), Some('l'), Some('r'), Some('c')];

/// Which row of the interior a piece of the dialog occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// `Rename mask: file name` / `Extension`.
    MaskLabels,
    /// The two mask fields.
    Masks,
    /// `Search for` / `Replace with`.
    ReplaceLabels,
    /// The two replace fields.
    Replace,
    /// The five toggles.
    Toggles,
    /// The case dropdown and the counter.
    CaseCounter,
    /// The preview table, which takes whatever is left.
    Preview,
    /// What stands in for it below contract the floor.
    PreviewHint,
    /// The first problem, or how many rows change.
    Error,
    /// The action row.
    Buttons,
}

/// What is given up first when the interior is too short.
///
/// The mask fields, the counter line, the error line and the buttons are never
/// in this list: names those five as what a 60x15 terminal still gets. The
/// labels go first because the fields beneath them are the controls.
const DROP_ORDER: [Slot; 4] = [
    Slot::ReplaceLabels,
    Slot::MaskLabels,
    Slot::Toggles,
    Slot::Replace,
];

/// The digits field's ceiling. Wider than any counter a filename wants, and
/// narrow enough that the padded value cannot approach `NAME_MAX` on its own.
const MAX_DIGITS: u8 = 18;

/// the Multi-Rename Tool.
pub struct MultiRenameDialog {
    /// The rows as the panel had them, kept so the plan can be rebuilt on
    /// every keystroke without asking the panel again.
    rows: Vec<(Entry, VfsPath)>,
    /// What each directory already holds.
    siblings: HashMap<VfsPath, HashSet<String>>,
    settings: Settings,
    plan: Plan,

    name_mask: Field,
    ext_mask: Field,
    search: Field,
    with: Field,
    counter_start: Field,
    counter_step: Field,
    counter_digits: Field,

    table: Table,
    /// True once the user has sorted, so the first plan keeps **panel order** -
    /// which is what numbers the counter until they say otherwise.
    sorted: bool,
    /// the `Undo` button is disabled with nothing to undo.
    can_undo: bool,
    /// And `Result list` with nothing to show.
    has_result: bool,
    /// A bad regex, reported under the table rather than swallowed.
    error: Option<String>,
    /// How the Size and Date columns are spelled, so they match the panel.
    cfg: PanelConfig,
    ring: FocusRing,
}

impl MultiRenameDialog {
    /// A dialog over `rows`.
    ///
    /// `rows` are the entries and their real addresses, in panel order - which
    /// is the order that numbers the counter until the table is sorted.
    /// `siblings` is what [`Plan::build`] needs and the panel
    /// already holds. `settings` is what the last `Ctrl+M` left, so reopening
    /// offers it again.
    pub fn new(
        rows: Vec<(Entry, VfsPath)>,
        siblings: HashMap<VfsPath, HashSet<String>>,
        settings: Settings,
        can_undo: bool,
    ) -> Self {
        let plan = Plan::build(rows.clone(), &settings, &siblings);
        let mut table = Table::new(PREVIEW);
        table.set_rows(plan.items().len());
        let mut dialog = Self {
            rows,
            siblings,
            name_mask: Field::with_text(&settings.name_mask),
            ext_mask: Field::with_text(&settings.ext_mask),
            search: Field::with_text(&settings.replace.search),
            with: Field::with_text(&settings.replace.with),
            counter_start: Field::with_text(settings.counter.start.to_string()),
            counter_step: Field::with_text(settings.counter.step.to_string()),
            counter_digits: Field::with_text(settings.counter.digits.to_string()),
            settings,
            plan,
            table,
            sorted: false,
            can_undo,
            has_result: false,
            error: None,
            cfg: PanelConfig::default(),
            ring: FocusRing::new(CONTROLS.len()),
        };
        dialog.rebuild();
        dialog
    }

    /// Say that there is a result list to show, so `Result list` is offered.
    #[must_use]
    pub const fn with_result(mut self, has_result: bool) -> Self {
        self.has_result = has_result;
        self
    }

    /// Spell the Size and Date columns the way the panel spells them.
    ///
    #[must_use]
    pub fn with_config(mut self, cfg: &PanelConfig) -> Self {
        self.cfg = cfg.clone();
        self
    }

    /// The preview, statuses and all.
    pub const fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The four control groups as they stand.
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The refusal currently shown, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Is `Start!` offered?
    ///
    /// the "the `Start!` button is disabled while any collision
    /// stands", plus the case with nothing to do: a batch of no-ops is not a
    /// rename, and offering it would queue a job with no pairs in it.
    pub fn can_start(&self) -> bool {
        !self.plan.blocked() && self.plan.changes() > 0
    }

    /// Read every control back into [`Settings`] and rebuild the plan.
    ///
    /// Called after any key that could have changed a control. A counter field
    /// that does not parse leaves the counter as it was, so a half-typed `-`
    /// does not renumber the table.
    fn rebuild(&mut self) {
        self.settings.name_mask = self.name_mask.text().to_string();
        self.settings.ext_mask = self.ext_mask.text().to_string();
        self.settings.replace.search = self.search.text().to_string();
        self.settings.replace.with = self.with.text().to_string();
        if let Ok(start) = self.counter_start.text().trim().parse::<i64>() {
            self.settings.counter.start = start;
        }
        if let Ok(step) = self.counter_step.text().trim().parse::<i64>() {
            self.settings.counter.step = step;
        }
        if let Ok(digits) = self.counter_digits.text().trim().parse::<u8>() {
            self.settings.counter.digits = digits.clamp(1, MAX_DIGITS);
        }

        // The one place a bad regex is reported, and it does not stop the
        // preview: the masks still expand, and the user is still typing.
        self.error = if self.settings.replace.is_empty() {
            None
        } else {
            self.settings.replace.compile().err().map(|e| e.to_string())
        };

        self.plan = Plan::build(self.rows.clone(), &self.settings, &self.siblings);
        if self.sorted {
            let (index, reverse) = self.table.sort();
            if let Some(column) = PreviewColumn::ALL.get(index) {
                self.plan.sort(*column, reverse, &self.settings);
            }
        }
        self.table.set_rows(self.plan.items().len());
    }

    /// Which control has focus.
    fn focused(&self) -> Control {
        CONTROLS
            .get(self.ring.index())
            .copied()
            .unwrap_or(Control::NameMask)
    }

    /// The field a control edits, if it edits one.
    fn field_mut(&mut self, control: Control) -> Option<&mut Field> {
        match control {
            Control::NameMask => Some(&mut self.name_mask),
            Control::ExtMask => Some(&mut self.ext_mask),
            Control::Search => Some(&mut self.search),
            Control::With => Some(&mut self.with),
            Control::CounterStart => Some(&mut self.counter_start),
            Control::CounterStep => Some(&mut self.counter_step),
            Control::CounterDigits => Some(&mut self.counter_digits),
            Control::FirstOnly
            | Control::IncludeExt
            | Control::Regex
            | Control::Substitute
            | Control::MatchCase
            | Control::Case
            | Control::Preview
            | Control::Start
            | Control::Undo
            | Control::ResultList
            | Control::Reset
            | Control::Close => None,
        }
    }

    /// Flip the toggle a control names. `false` when it is not a toggle.
    fn toggle(&mut self, control: Control) -> bool {
        let replace = &mut self.settings.replace;
        match control {
            Control::FirstOnly => replace.first_only = !replace.first_only,
            Control::IncludeExt => replace.include_ext = !replace.include_ext,
            Control::Regex => replace.regex = !replace.regex,
            Control::Substitute => replace.substitute = !replace.substitute,
            Control::MatchCase => replace.match_case = !replace.match_case,
            Control::NameMask
            | Control::ExtMask
            | Control::Search
            | Control::With
            | Control::Case
            | Control::CounterStart
            | Control::CounterStep
            | Control::CounterDigits
            | Control::Preview
            | Control::Start
            | Control::Undo
            | Control::ResultList
            | Control::Reset
            | Control::Close => return false,
        }
        true
    }

    /// Step the Upper/lowercase dropdown.
    fn cycle_case(&mut self, forward: bool) {
        let all = Case::CHOICES;
        let len = all.len().max(1);
        let current = all
            .iter()
            .position(|c| *c == self.settings.case)
            .unwrap_or(0);
        let next = if forward {
            current.saturating_add(1).rem_euclid(len)
        } else {
            current
                .saturating_add(len)
                .saturating_sub(1)
                .rem_euclid(len)
        };
        self.settings.case = all.get(next).copied().unwrap_or_default();
    }

    /// the reset control: masks back to `[N]` / `[E]`, everything
    /// else to its default.
    fn reset(&mut self) {
        self.settings = Settings::reset();
        self.name_mask.set_text(&self.settings.name_mask);
        self.ext_mask.set_text(&self.settings.ext_mask);
        self.search.set_text("");
        self.with.set_text("");
        self.counter_start
            .set_text(self.settings.counter.start.to_string());
        self.counter_step
            .set_text(self.settings.counter.step.to_string());
        self.counter_digits
            .set_text(self.settings.counter.digits.to_string());
        self.sorted = false;
        self.rebuild();
    }

    /// The answer this dialog carries.
    fn answer(&self, undo: bool, show_result: bool) -> DialogOutcome {
        DialogOutcome::Accept(DialogResult::MultiRename(Box::new(MultiRenameAnswer {
            pairs: if undo || show_result {
                Vec::new()
            } else {
                self.plan.pairs()
            },
            undo,
            show_result,
            settings: self.settings.clone(),
        })))
    }

    /// `Start!`, which does nothing while a collision stands.
    fn start(&self) -> DialogOutcome {
        if self.can_start() {
            self.answer(false, false)
        } else {
            DialogOutcome::Consumed
        }
    }

    /// The line under the table: the first problem, or what will happen.
    pub fn status_line(&self) -> String {
        if let Some(error) = self.error.as_deref() {
            return error.to_string();
        }
        if let Some((index, status)) = self.plan.first_problem() {
            let name = self
                .plan
                .items()
                .get(index)
                .map_or_else(String::new, |i| i.entry.name.clone());
            return format!(
                "row {}: {} - {name}",
                index.saturating_add(1),
                status.label()
            );
        }
        // the design asks for "insert buttons for each placeholder so the
        // syntax never has to be memorised". A row of buttons for fourteen
        // placeholders does not fit beside two combo boxes at the design's
        // floor, so the affordance is this line while a mask field has focus:
        // the same information, at the moment it is wanted, in the space a
        // dialog has.
        if matches!(self.focused(), Control::NameMask | Control::ExtMask) {
            return placeholder_hint();
        }
        let changes = self.plan.changes();
        format!("{changes} of {} rows change", self.plan.items().len())
    }

    /// The toggles line, which is the five in one row.
    ///
    /// One piece per control rather than one string, for two reasons: a
    /// mnemonic is scoped to its own control's label,
    /// and a piece carries its own style, which
    /// is what gives these five the focus highlight every other dialog's
    /// controls already have.
    fn toggle_pieces(&self, ascii: bool, style: &DialogStyle) -> Vec<Piece> {
        let r = &self.settings.replace;
        let focused = self.focused();
        vec![
            Piece::new(
                checkbox("1x", r.first_only, ascii),
                Some('x'),
                style.button(focused == Control::FirstOnly),
                focused == Control::FirstOnly,
            ),
            Piece::new(
                // `[E]` is a placeholder glyph, not a word: its only letter is
                // the `e` `Extension` already holds.
                checkbox("[E]", r.include_ext, ascii),
                None,
                style.button(focused == Control::IncludeExt),
                focused == Control::IncludeExt,
            ),
            Piece::new(
                checkbox("RegEx", r.regex, ascii),
                Some('g'),
                style.button(focused == Control::Regex),
                focused == Control::Regex,
            ),
            Piece::new(
                checkbox("Subst.", r.substitute, ascii),
                Some('b'),
                style.button(focused == Control::Substitute),
                focused == Control::Substitute,
            ),
            Piece::new(
                checkbox("Match case", r.match_case, ascii),
                Some('h'),
                style.button(focused == Control::MatchCase),
                focused == Control::MatchCase,
            ),
        ]
    }

    /// The case dropdown and the counter, one piece per control.
    ///
    /// `Counter:` is a group heading rather than a control, so it is its own
    /// piece with no letter: as one string with `start 10`, `Alt+T` would
    /// underline the `t` of `Counter`.
    fn case_counter_pieces(&self, style: &DialogStyle) -> Vec<Piece> {
        let c = &self.settings.counter;
        let focused = self.focused();
        vec![
            Piece::new(
                format!("Case: {}", self.settings.case.label()),
                None,
                style.button(focused == Control::Case),
                focused == Control::Case,
            ),
            // A group heading and not a control: nothing focuses it, so it is
            // never the piece a narrow row has to keep.
            Piece::new(COUNTER_LABEL, None, style.body(), false),
            Piece::new(
                format!("start {}", c.start),
                Some('t'),
                style.button(focused == Control::CounterStart),
                focused == Control::CounterStart,
            ),
            Piece::new(
                format!("step {}", c.step),
                Some('p'),
                style.button(focused == Control::CounterStep),
                focused == Control::CounterStep,
            ),
            Piece::new(
                format!("digits {}", c.digits),
                Some('d'),
                style.button(focused == Control::CounterDigits),
                focused == Control::CounterDigits,
            ),
        ]
    }

    /// One cell of the preview.
    ///
    /// `column` indexes [`PreviewColumn::ALL`], which is the same order
    /// [`PREVIEW`] is in.
    fn cell(&self, index: usize, column: usize) -> (String, Option<Style>) {
        let Some(item) = self.plan.items().get(index) else {
            return (String::new(), None);
        };
        let Some(which) = PreviewColumn::ALL.get(column) else {
            return (String::new(), None);
        };
        let body = match which {
            // The stem, because `Ext` is its own column - the same split the
            // panel's `name` and `ext` columns make.
            PreviewColumn::OldName => item.entry.split_name().0.to_string(),
            PreviewColumn::Ext => item.entry.extension().to_string(),
            // The whole new name, extension included: it is what the file will
            // be called, and there is no second column to put the rest in.
            PreviewColumn::NewName => item.new_name.clone(),
            PreviewColumn::Size => size_text(&item.entry, &self.cfg),
            PreviewColumn::Date => date_text(&item.entry, &self.cfg),
            PreviewColumn::Location => item.dir.to_string(),
            PreviewColumn::Status => item.status.label(),
        };
        (body, None)
    }

    /// Which slots this interior has room for, and where each one goes.
    ///
    /// The preview takes whatever is left after the fixed rows, and is dropped
    /// entirely below contract the floor.
    fn rows_of(&self, area: Rect) -> Vec<(Slot, Rect)> {
        let mut wanted = vec![
            Slot::MaskLabels,
            Slot::Masks,
            Slot::ReplaceLabels,
            Slot::Replace,
            Slot::Toggles,
            Slot::CaseCounter,
            Slot::Error,
            Slot::Buttons,
        ];
        let height = usize::from(area.height);
        for slot in DROP_ORDER {
            if wanted.len() <= height {
                break;
            }
            wanted.retain(|s| *s != slot);
        }
        wanted.truncate(height);

        // Whatever is left over is the preview, if there is room for a header
        // and a row and the interior is wide enough for the columns. Below
        // contract the floor the same rows carry the one line that says so,
        // which keeps the error line and the action row at the bottom either
        // way.
        let spare = height.saturating_sub(wanted.len());
        let filler = if spare == 0 {
            None
        } else if spare >= 2 && area.width >= PREVIEW_MIN_WIDTH {
            Some(Slot::Preview)
        } else {
            Some(Slot::PreviewHint)
        };

        let mut out: Vec<(Slot, Rect)> = Vec::with_capacity(wanted.len().saturating_add(1));
        let mut y = 0u16;
        for slot in wanted {
            // The preview goes where the design draws it: under the four
            // control groups and above the line that reports the first problem.
            if slot == Slot::Error
                && let Some(filler) = filler
            {
                let h = u16::try_from(spare).unwrap_or(u16::MAX);
                if let Some(rect) = block(area, y, h) {
                    out.push((filler, rect));
                }
                y = y.saturating_add(h);
            }
            let Some(rect) = block(area, y, 1) else {
                break;
            };
            out.push((slot, rect));
            y = y.saturating_add(1);
        }
        out
    }

    /// Where the preview table is drawn, or `None` when contract the floor
    /// replaced it with the one line that says why.
    pub fn preview_rect(&self, area: Rect) -> Option<Rect> {
        self.rows_of(area)
            .into_iter()
            .find(|(slot, _)| *slot == Slot::Preview)
            .map(|(_, rect)| rect)
    }

    /// The action row's labels, at the longest spelling that fits.
    fn button_labels(width: u16) -> [&'static str; 5] {
        let tiers: [[&'static str; 5]; 2] = [
            ["Start!", "Undo", "Result list", "Reset", "Close"],
            ["Start!", "Undo", "Results", "Reset", "Close"],
        ];
        let floor = tiers.last().copied().unwrap_or(["Start!", "", "", "", ""]);
        tiers
            .into_iter()
            .find(|labels| row_width(labels) <= usize::from(width))
            .unwrap_or(floor)
    }

    /// Which button of the action row is focused, for the highlight. Out of
    /// range when focus is anywhere else, which draws none of them highlighted.
    fn focused_button(&self) -> usize {
        match self.focused() {
            Control::Start => 0,
            Control::Undo => 1,
            Control::ResultList => 2,
            Control::Reset => 3,
            Control::Close => 4,
            // Focus is on a field, a toggle, the combo or the table, so no
            // button is highlighted.
            Control::NameMask
            | Control::ExtMask
            | Control::Search
            | Control::With
            | Control::FirstOnly
            | Control::IncludeExt
            | Control::Regex
            | Control::Substitute
            | Control::MatchCase
            | Control::Case
            | Control::CounterStart
            | Control::CounterStep
            | Control::CounterDigits
            | Control::Preview => usize::MAX,
        }
    }

    /// Draw two fields side by side, with the focused one's caret honoured.
    fn split(area: Rect) -> (Rect, Rect) {
        let half = area.width / 2;
        let left = Rect::new(area.x, area.y, half.saturating_sub(1).max(1), 1);
        let right = Rect::new(
            area.x.saturating_add(half),
            area.y,
            area.width.saturating_sub(half),
            1,
        );
        (left, right)
    }
}

/// A `height`-row block of `area` starting at row `y`, or `None` when it does
/// not fit. Every rectangle this dialog builds goes through here.
fn block(area: Rect, y: u16, height: u16) -> Option<Rect> {
    if area.width == 0 || height == 0 || y >= area.height {
        return None;
    }
    let h = height.min(area.height.saturating_sub(y));
    Some(Rect::new(area.x, area.y.saturating_add(y), area.width, h))
}

/// How many columns a row of buttons occupies, the same arithmetic
/// [`draw_buttons`] does when it decides what to drop.
fn row_width(labels: &[&str]) -> usize {
    let mut used = 0usize;
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            used = used.saturating_add(2);
        }
        used = used.saturating_add(text::width(label)).saturating_add(4);
    }
    used.saturating_add(1)
}

/// The placeholder buttons the design asks for, as a hint line.
///
/// > with insert buttons for each placeholder so the syntax never has to be
/// > memorised
///
/// A row of buttons for fourteen placeholders does not fit beside two combo
/// boxes at the floor, so the affordance is a hint line naming them
/// all - which is the same information, in the space a dialog has.
pub fn placeholder_hint() -> String {
    let mut out = String::from("[N] [N1-3] [C] [P] [E]");
    for part in DatePart::ALL {
        out.push(' ');
        out.push_str(part.tag());
    }
    out
}

impl Accelerated for MultiRenameDialog {
    type Control = Control;

    fn mnemonics(&self) -> &'static [(Control, char)] {
        MNEMONICS
    }

    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            // The combo, the table and the seven fields are focus-only:
            // the design gives no meaning to accelerating a three-way
            // control, and both candidate meanings turn something off.
            //
            Control::NameMask
            | Control::ExtMask
            | Control::Search
            | Control::With
            | Control::Case
            | Control::CounterStart
            | Control::CounterStep
            | Control::CounterDigits
            | Control::Preview => Accel::Focus,
            Control::FirstOnly
            | Control::IncludeExt
            | Control::Regex
            | Control::Substitute
            | Control::MatchCase => Accel::Check,
            Control::Start
            | Control::Undo
            | Control::ResultList
            | Control::Reset
            | Control::Close => Accel::Press,
        }
    }

    fn focus_control(&mut self, control: Control) {
        if let Some(at) = CONTROLS.iter().position(|c| *c == control) {
            self.ring.set(at);
        }
    }

    /// **on**, never a toggle. A repeated `Alt+G` leaves `RegEx`
    /// ticked; `Space` is how it comes off, one keystroke away with the focus
    /// now on the box.
    fn switch_on(&mut self, control: Control) {
        let replace = &mut self.settings.replace;
        match control {
            Control::FirstOnly => replace.first_only = true,
            Control::IncludeExt => replace.include_ext = true,
            Control::Regex => replace.regex = true,
            Control::Substitute => replace.substitute = true,
            Control::MatchCase => replace.match_case = true,
            Control::NameMask
            | Control::ExtMask
            | Control::Search
            | Control::With
            | Control::Case
            | Control::CounterStart
            | Control::CounterStep
            | Control::CounterDigits
            | Control::Preview
            | Control::Start
            | Control::Undo
            | Control::ResultList
            | Control::Reset
            | Control::Close => return,
        }
        // Every control this dialog has feeds the preview, so switching one on
        // renumbers the New name column the same keystroke.
        self.rebuild();
    }

    /// The one place each button of the action row acts.
    fn press(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Start => self.start(),
            // A greyed button is still focused and still refuses, which is the
            // same answer `Enter` on it gives.
            Control::Undo if self.can_undo => self.answer(true, false),
            Control::ResultList if self.has_result => self.answer(false, true),
            Control::Undo | Control::ResultList => DialogOutcome::Consumed,
            Control::Reset => {
                self.reset();
                DialogOutcome::Consumed
            }
            Control::Close => DialogOutcome::Cancel,
            Control::NameMask
            | Control::ExtMask
            | Control::Search
            | Control::With
            | Control::FirstOnly
            | Control::IncludeExt
            | Control::Regex
            | Control::Substitute
            | Control::MatchCase
            | Control::Case
            | Control::CounterStart
            | Control::CounterStep
            | Control::CounterDigits
            | Control::Preview => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for MultiRenameDialog {
    fn id(&self) -> DialogId {
        DialogId::MultiRename
    }

    fn title(&self) -> String {
        // `file(s)` literally, and at a count of one, exactly as the design
        // spells the copy dialog's title.
        format!("Multi-Rename Tool: {} file(s)", self.rows.len())
    }

    fn size_hint(&self) -> (u16, u16) {
        // Eight fixed rows, plus a preview worth having, plus the border.
        (110, 24)
    }

    /// All sixteen letters.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // the mnemonics come first, before anything that reads
        // `key.action` - the `Ctrl+R` reset included - and before any field can
        // see the key.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        // The reset control is bound to the button **and** to `Ctrl+R` inside
        // the dialog.
        //
        // The raw key as well as the action, exactly as `DialogKey::is_cancel`
        // reads both: `reread` is bound in the `[panel]` table and there is no
        // `[dialog]` binding for it, so a dialog that waited for the action
        // alone would never see `Ctrl+R`.
        if key.action == Some(Action::Reread)
            || (key.press.mods.contains(KeyModifiers::CONTROL)
                && key.press.code == KeyCode::Char('r'))
        {
            self.reset();
            return DialogOutcome::Consumed;
        }

        let focused = self.focused();

        // `Ctrl+<n>` sorts the preview from anywhere, which is the accelerator
        // the design already gives the panel's columns. The status column
        // is not sortable, so its digit is consumed and ignored rather than
        // moving a marker onto a column the plan will not sort by.
        if key.press.mods.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = key.press.code
            && let Some(digit) = c.to_digit(10)
            && digit >= 1
        {
            let index = usize::try_from(digit).unwrap_or(1).saturating_sub(1);
            match PreviewColumn::ALL.get(index) {
                Some(column) if column.sortable() => {
                    // The first sort of the session is ascending even on the
                    // column the table happens to start pointed at: the plan
                    // has not been sorted at all until now, so there is nothing
                    // for the same key to reverse. the "the same
                    // key again reverses" applies from the second press on.
                    let again = self.sorted;
                    self.sorted = true;
                    if again || index != self.table.sort().0 {
                        self.table.set_sort(index);
                    }
                    self.rebuild();
                    return DialogOutcome::Consumed;
                }
                Some(_) => return DialogOutcome::Consumed,
                None => {}
            }
        }

        if key.is_accept() {
            // One route to each button: `Enter`
            // presses whichever has focus, exactly as `Alt`+letter does. A form
            // accepts from its fields, so from anywhere else it is `Start!`.
            return match focused {
                Control::Undo
                | Control::ResultList
                | Control::Reset
                | Control::Close
                | Control::Start => self.press(focused),
                Control::NameMask
                | Control::ExtMask
                | Control::Search
                | Control::With
                | Control::FirstOnly
                | Control::IncludeExt
                | Control::Regex
                | Control::Substitute
                | Control::MatchCase
                | Control::Case
                | Control::CounterStart
                | Control::CounterStep
                | Control::CounterDigits
                | Control::Preview => self.press(Control::Start),
            };
        }

        // Vertical movement walks the form wherever focus is, except inside
        // the preview, where it is the table's.
        if focused == Control::Preview && self.table.handle(key, true) {
            return DialogOutcome::Consumed;
        }
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

        if focused == Control::Case {
            match key.press.code {
                KeyCode::Left => {
                    self.cycle_case(false);
                    self.rebuild();
                    return DialogOutcome::Consumed;
                }
                KeyCode::Right => {
                    self.cycle_case(true);
                    self.rebuild();
                    return DialogOutcome::Consumed;
                }
                _ => {
                    if key.text() == Some(' ') {
                        self.cycle_case(true);
                        self.rebuild();
                        return DialogOutcome::Consumed;
                    }
                }
            }
        }

        if key.text() == Some(' ') && self.toggle(focused) {
            self.rebuild();
            return DialogOutcome::Consumed;
        }

        if let Some(field) = self.field_mut(focused)
            && field.handle(key)
        {
            self.rebuild();
            return DialogOutcome::Consumed;
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
        let body = style.body();
        let focused = self.focused();
        let dim = Style::new().fg(style.border).bg(style.bg);
        for (slot, rect) in self.rows_of(area) {
            match slot {
                Slot::MaskLabels => {
                    let (left, right) = Self::split(rect);
                    draw_mnemonic(f, left, NAME_MASK_LABEL, 'm', body, style.ascii);
                    draw_mnemonic(f, right, EXT_MASK_LABEL, 'e', body, style.ascii);
                }
                Slot::Masks => {
                    let (left, right) = Self::split(rect);
                    self.name_mask.render(f, left, style);
                    self.ext_mask.render(f, right, style);
                }
                Slot::ReplaceLabels => {
                    let (left, right) = Self::split(rect);
                    draw_mnemonic(f, left, SEARCH_LABEL, 's', body, style.ascii);
                    draw_mnemonic(f, right, WITH_LABEL, 'w', body, style.ascii);
                }
                Slot::Replace => {
                    let (left, right) = Self::split(rect);
                    self.search.render(f, left, style);
                    self.with.render(f, right, style);
                }
                Slot::Toggles => {
                    draw_mnemonic_pieces(f, rect, &self.toggle_pieces(style.ascii, style), body);
                }
                Slot::CaseCounter => {
                    draw_mnemonic_pieces(f, rect, &self.case_counter_pieces(style), body);
                }
                Slot::Preview => {
                    let cell = |index: usize, column: usize| self.cell(index, column);
                    self.table
                        .render(f, rect, style, focused == Control::Preview, &cell);
                }
                Slot::PreviewHint => {
                    // never a broken table, and never silence about why the table
                    // is not there.
                    if let Some(line) = row(rect, 0) {
                        draw_text(f, line, "preview needs a wider terminal", dim, style.ascii);
                    }
                }
                Slot::Error => {
                    let style_of = if self.error.is_some() || self.plan.blocked() {
                        Style::new()
                            .fg(style.button_focus)
                            .bg(style.bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        body
                    };
                    draw_text(f, rect, &self.status_line(), style_of, style.ascii);
                }
                Slot::Buttons => {
                    let labels = Self::button_labels(rect.width);
                    let with_letters: Vec<(&str, Option<char>)> = labels
                        .iter()
                        .zip(BUTTON_MNEMONICS)
                        .map(|(label, letter)| (*label, letter))
                        .collect();
                    draw_mnemonic_buttons(f, rect, &with_letters, self.focused_button(), style);
                }
            }
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let focused = self.focused();
        let rows = self.rows_of(area);
        let find = |slot: Slot| rows.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r);
        let (field, rect) = match focused {
            Control::NameMask => (&self.name_mask, find(Slot::Masks).map(|r| Self::split(r).0)),
            Control::ExtMask => (&self.ext_mask, find(Slot::Masks).map(|r| Self::split(r).1)),
            Control::Search => (&self.search, find(Slot::Replace).map(|r| Self::split(r).0)),
            Control::With => (&self.with, find(Slot::Replace).map(|r| Self::split(r).1)),
            // Nothing else in this dialog carries a caret: the counter fields
            // are drawn as pieces of a shared row rather than as boxed fields.
            Control::FirstOnly
            | Control::IncludeExt
            | Control::Regex
            | Control::Substitute
            | Control::MatchCase
            | Control::Case
            | Control::CounterStart
            | Control::CounterStep
            | Control::CounterDigits
            | Control::Preview
            | Control::Start
            | Control::Undo
            | Control::ResultList
            | Control::Reset
            | Control::Close => return None,
        };
        rect.and_then(|rect| field.cursor(rect))
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
    use crate::config::{ColorDepth, Theme};
    use crate::input::KeyPress;
    use crate::rename::mask::Counter;
    use crate::rename::plan::RenameStatus;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{Duration, SystemTime};

    fn dir() -> VfsPath {
        VfsPath::local("/srv/media")
    }

    fn entry(name: &str) -> Entry {
        let mut e = Entry::file(name);
        e.size = 1024;
        e.mtime = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000));
        e
    }

    fn rows(names: &[&str]) -> Vec<(Entry, VfsPath)> {
        names.iter().map(|n| (entry(n), dir().join(n))).collect()
    }

    fn siblings(names: &[&str]) -> HashMap<VfsPath, HashSet<String>> {
        let mut map = HashMap::new();
        map.insert(dir(), names.iter().map(|n| (*n).to_string()).collect());
        map
    }

    fn dialog(names: &[&str]) -> MultiRenameDialog {
        MultiRenameDialog::new(rows(names), siblings(names), Settings::reset(), false)
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn with_mods(code: KeyCode, mods: KeyModifiers) -> DialogKey {
        DialogKey::raw(KeyPress::new(code, mods))
    }

    fn typed(d: &mut MultiRenameDialog, text: &str) {
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
    /// table, so a declared letter with no paint behind it fails the test that
    /// uses this.
    fn underlined(d: &MultiRenameDialog, w: u16, h: u16) -> Vec<char> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
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
        out.iter().map(|c| c.to_ascii_lowercase()).collect()
    }

    /// Every underlined cell, paired with the character drawn to its left.
    ///
    /// Which is what tells the two `x`s of `[x] 1x` apart: the label's is
    /// preceded by the `1`, the tick mark's by the `[`. The plain set of
    /// underlined characters cannot, because it is `x` either way.
    fn underlined_after(d: &MultiRenameDialog, w: u16, h: u16) -> Vec<(char, char)> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
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
                if !cell.modifier.contains(Modifier::UNDERLINED) {
                    continue;
                }
                let before = x
                    .checked_sub(1)
                    .and_then(|left| buffer.cell((left, y)))
                    .and_then(|left| left.symbol().chars().next())
                    .unwrap_or(' ');
                out.push((before, cell.symbol().chars().next().unwrap_or(' ')));
            }
        }
        out
    }

    #[test]
    fn ticking_the_1x_box_leaves_its_underline_on_its_own_label() {
        // "The letter is shown underlined in that control's
        // label." A ticked box is drawn `[x] 1x` and the tick is the literal
        // character `x` at a word start, so the underline used to move off the
        // label and onto the mark for exactly as long as the box stayed
        // ticked - and back again when it came off. Read off the buffer,
        // because the set of underlined *characters* is `x` either way and
        // cannot tell the two apart.
        let mut d = dialog(&["a.txt", "b.txt"]);
        assert!(!d.settings().replace.first_only, "the box starts unticked");
        let unticked = underlined_after(&d, 130, 30);
        assert!(
            unticked.contains(&('1', 'x')),
            "the `x` of `1x`: {unticked:?}"
        );

        assert!(matches!(d.handle_key(&alt('x')), DialogOutcome::Consumed));
        assert!(d.settings().replace.first_only, "Alt+X ticked it");
        let ticked = underlined_after(&d, 130, 30);
        assert_eq!(ticked, unticked, "and moved no underline anywhere");
        assert!(
            !ticked.contains(&('[', 'x')),
            "never the tick mark inside the brackets: {ticked:?}"
        );
    }

    /// Move focus to a named control, the way `Tab` would.
    fn focus(d: &mut MultiRenameDialog, control: Control) {
        for _ in 0..CONTROLS.len() {
            if d.focused() == control {
                return;
            }
            d.handle_key(&key(KeyCode::Tab));
        }
        panic!("{control:?} is not in the ring");
    }

    fn new_names(d: &MultiRenameDialog) -> Vec<String> {
        d.plan()
            .items()
            .iter()
            .map(|i| i.new_name.clone())
            .collect()
    }

    #[test]
    fn the_preview_columns_and_the_model_agree_about_their_headings() {
        // Two lists of column headings that can disagree is one list too many.
        assert_eq!(PREVIEW.len(), PreviewColumn::ALL.len());
        for (column, table) in PreviewColumn::ALL.iter().zip(PREVIEW) {
            assert_eq!(column.header(), table.header);
        }
    }

    #[test]
    fn it_opens_on_the_default_masks_with_nothing_to_do() {
        let mut d = dialog(&["a.txt", "b.txt"]);
        assert_eq!(d.settings().name_mask, "[N]");
        assert_eq!(d.settings().ext_mask, "[E]");
        assert_eq!(new_names(&d), vec!["a.txt", "b.txt"]);
        assert_eq!(d.plan().changes(), 0);
        assert!(!d.can_start(), "a batch of no-ops is not a rename");
        // It opens on the name mask, so the line under the table is the
        // placeholder list; step off it and the line reports the batch.
        assert_eq!(d.status_line(), placeholder_hint());
        focus(&mut d, Control::Preview);
        assert_eq!(d.status_line(), "0 of 2 rows change");
    }

    #[test]
    fn typing_a_mask_updates_the_new_name_column_on_every_keystroke() {
        // "The New name column updates on every keystroke in any
        // control." The field opens on `[N]` with the caret at its end, so
        // typing appends to the mask.
        let mut d = dialog(&["a.txt", "b.txt"]);
        focus(&mut d, Control::NameMask);
        typed(&mut d, "_");
        assert_eq!(
            new_names(&d),
            vec!["a_.txt", "b_.txt"],
            "after one keystroke"
        );
        typed(&mut d, "x");
        assert_eq!(new_names(&d), vec!["a_x.txt", "b_x.txt"]);
        assert!(d.can_start());
        assert_eq!(
            d.status_line(),
            placeholder_hint(),
            "a focused mask field is offered the placeholder list"
        );
        focus(&mut d, Control::Preview);
        assert_eq!(d.status_line(), "2 of 2 rows change");
        focus(&mut d, Control::NameMask);

        // A mask with no `[N]` in it makes every row the same name, which is a
        // collision with another renamed file and refuses the batch.
        for _ in 0.."[N]_x".len() {
            d.handle_key(&key(KeyCode::Backspace));
        }
        typed(&mut d, "same");
        assert_eq!(new_names(&d), vec!["same.txt", "same.txt"]);
        assert!(!d.can_start());
        assert!(
            d.status_line().starts_with("row 1: dup of 2"),
            "{}",
            d.status_line()
        );
    }

    #[test]
    fn the_counter_previews_what_acceptance_criterion_nine_asks_for() {
        // Four marked files, `[N]_[C]`, start 10, step 5, three digits.
        let mut d = MultiRenameDialog::new(
            rows(&["a", "a", "a", "a"]),
            siblings(&["a"]),
            Settings {
                name_mask: "[N]_[C]".to_string(),
                ext_mask: String::new(),
                counter: Counter {
                    start: 10,
                    step: 5,
                    digits: 3,
                },
                ..Settings::reset()
            },
            false,
        );
        assert_eq!(new_names(&d), vec!["a_010", "a_015", "a_020", "a_025"]);
        assert!(d.can_start());
        assert_eq!(d.plan().pairs().len(), 4);

        // And the counter fields are what drive it: typing into `Digits`
        // renumbers.
        focus(&mut d, Control::CounterDigits);
        d.handle_key(&key(KeyCode::Backspace));
        typed(&mut d, "1");
        assert_eq!(new_names(&d), vec!["a_10", "a_15", "a_20", "a_25"]);
        assert_eq!(d.settings().counter.digits, 1);
    }

    #[test]
    fn a_half_typed_counter_leaves_the_table_alone() {
        // A field mid-edit is not a number, and a preview that emptied itself
        // between two keystrokes would be unusable.
        let mut d = dialog(&["a"]);
        focus(&mut d, Control::CounterStart);
        d.handle_key(&key(KeyCode::Backspace));
        assert_eq!(d.settings().counter.start, 1, "the last good value stands");
        typed(&mut d, "-");
        assert_eq!(d.settings().counter.start, 1);
        typed(&mut d, "5");
        assert_eq!(d.settings().counter.start, -5);
    }

    #[test]
    fn start_is_disabled_while_a_collision_stands_and_a_changed_mask_re_enables_it() {
        // Acceptance criterion 11.
        let mut d = MultiRenameDialog::new(
            rows(&["a.txt"]),
            siblings(&["a.txt", "taken.txt"]),
            Settings {
                name_mask: "taken".to_string(),
                ..Settings::reset()
            },
            false,
        );
        assert!(!d.can_start());
        assert!(d.status_line().contains("exists"), "{}", d.status_line());
        // Pressing it does nothing at all rather than queueing a job.
        focus(&mut d, Control::Start);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));

        focus(&mut d, Control::NameMask);
        d.handle_key(&key(KeyCode::End));
        typed(&mut d, "2");
        assert_eq!(new_names(&d), vec!["taken2.txt"]);
        assert!(d.can_start());
        focus(&mut d, Control::Start);
        let outcome = d.handle_key(&key(KeyCode::Enter));
        match outcome {
            DialogOutcome::Accept(DialogResult::MultiRename(answer)) => {
                assert_eq!(
                    answer.pairs,
                    vec![(dir().join("a.txt"), dir().join("taken2.txt"))]
                );
                assert!(!answer.undo);
                assert!(!answer.show_result);
                assert_eq!(answer.settings.name_mask, "taken2");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_blocking_status_disables_start() {
        // a loop over the enum, so a variant added later cannot quietly
        // leave the button enabled.
        let all = [
            (RenameStatus::Ok, true),
            (RenameStatus::NoChange, false),
            (RenameStatus::Exists, false),
            (RenameStatus::Duplicate(1), false),
            (RenameStatus::InvalidChar('/'), false),
            (RenameStatus::Empty, false),
            (RenameStatus::TooLong, false),
            (RenameStatus::NoDate, true),
        ];
        for (status, startable) in all {
            // A one-row plan in exactly that state, built through the dialog so
            // the button and the plan are the two things being compared.
            let d = match &status {
                RenameStatus::Ok => dialog_with_mask(&["a"], &["a"], "b"),
                RenameStatus::NoChange => dialog_with_mask(&["a"], &["a"], "a"),
                RenameStatus::Exists => dialog_with_mask(&["a"], &["a", "b"], "b"),
                RenameStatus::Duplicate(_) => dialog_with_mask(&["a", "c"], &["a", "c"], "b"),
                RenameStatus::InvalidChar(_) => dialog_with_mask(&["a"], &["a"], "b/c"),
                RenameStatus::Empty => dialog_with_masks(&["a"], &["a"], "", ""),
                RenameStatus::TooLong => dialog_with_mask(&["a"], &["a"], &"x".repeat(300)),
                RenameStatus::NoDate => dialog_no_date(),
            };
            assert_eq!(
                d.can_start(),
                startable,
                "{status:?}: {:?}",
                d.plan()
                    .items()
                    .iter()
                    .map(|i| &i.status)
                    .collect::<Vec<_>>()
            );
            assert_eq!(d.plan().blocked(), status.blocks(), "{status:?}");
        }
    }

    fn dialog_with_mask(names: &[&str], listed: &[&str], mask: &str) -> MultiRenameDialog {
        dialog_with_masks(names, listed, mask, "")
    }

    fn dialog_with_masks(
        names: &[&str],
        listed: &[&str],
        mask: &str,
        ext: &str,
    ) -> MultiRenameDialog {
        MultiRenameDialog::new(
            rows(names),
            siblings(listed),
            Settings {
                name_mask: mask.to_string(),
                ext_mask: ext.to_string(),
                ..Settings::reset()
            },
            false,
        )
    }

    fn dialog_no_date() -> MultiRenameDialog {
        let mut e = entry("a");
        e.mtime = None;
        MultiRenameDialog::new(
            vec![(e, dir().join("a"))],
            HashMap::new(),
            Settings {
                name_mask: "[YMD]b".to_string(),
                ext_mask: String::new(),
                ..Settings::reset()
            },
            false,
        )
    }

    #[test]
    fn the_toggles_and_the_dropdown_are_the_five_and_the_five() {
        // the five toggles and its five-entry dropdown, each one
        // reachable and each one visible in the New name column.
        let mut d = dialog(&["Report.txt"]);
        focus(&mut d, Control::Search);
        typed(&mut d, "r");
        focus(&mut d, Control::With);
        typed(&mut d, "X");
        assert_eq!(
            new_names(&d),
            vec!["XepoXt.txt"],
            "case-insensitive by default (contract 10.16)"
        );

        focus(&mut d, Control::MatchCase);
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(d.settings().replace.match_case);
        assert_eq!(new_names(&d), vec!["RepoXt.txt"], "the `^` toggle is case");

        focus(&mut d, Control::FirstOnly);
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(d.settings().replace.first_only);
        assert_eq!(new_names(&d), vec!["RepoXt.txt"], "one occurrence, one hit");

        // `[E]` reaches the extension, and `RegEx` and `Subst.` are the other
        // two.
        focus(&mut d, Control::IncludeExt);
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(d.settings().replace.include_ext);
        focus(&mut d, Control::Regex);
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(d.settings().replace.regex);
        focus(&mut d, Control::Substitute);
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(d.settings().replace.substitute);

        focus(&mut d, Control::Case);
        d.handle_key(&key(KeyCode::Right));
        assert_eq!(d.settings().case, Case::Lower);
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(d.settings().case, Case::Unchanged);
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(
            d.settings().case,
            Case::EachWordCapital,
            "the dropdown wraps"
        );
    }

    #[test]
    fn a_bad_regex_is_reported_and_does_not_empty_the_table() {
        let mut d = dialog(&["a.txt"]);
        focus(&mut d, Control::Regex);
        d.handle_key(&key(KeyCode::Char(' ')));
        focus(&mut d, Control::Search);
        typed(&mut d, "(unclosed");
        assert!(d.error().is_some(), "a bad pattern is reported");
        assert_eq!(new_names(&d), vec!["a.txt"], "and the masks still expand");
        assert!(
            d.status_line().contains("search pattern"),
            "{}",
            d.status_line()
        );
    }

    #[test]
    fn sorting_the_preview_renumbers_the_counter() {
        // "the sort determines counter order".
        let mut d = MultiRenameDialog::new(
            rows(&["c", "a", "b"]),
            siblings(&["a", "b", "c"]),
            Settings {
                name_mask: "[C]-[N]".to_string(),
                ext_mask: String::new(),
                ..Settings::reset()
            },
            false,
        );
        assert_eq!(
            new_names(&d),
            vec!["1-c", "2-a", "3-b"],
            "panel order first"
        );
        d.handle_key(&with_mods(KeyCode::Char('1'), KeyModifiers::CONTROL));
        assert_eq!(new_names(&d), vec!["1-a", "2-b", "3-c"]);
        d.handle_key(&with_mods(KeyCode::Char('1'), KeyModifiers::CONTROL));
        assert_eq!(
            new_names(&d),
            vec!["1-c", "2-b", "3-a"],
            "and again reverses"
        );
        // The status column does not sort, and its digit does not move the
        // marker either.
        let before = new_names(&d);
        d.handle_key(&with_mods(KeyCode::Char('7'), KeyModifiers::CONTROL));
        assert_eq!(new_names(&d), before);
    }

    #[test]
    fn the_reset_control_puts_the_masks_back() {
        // the reset control, on the button and on `Ctrl+R`.
        let mut d = dialog(&["a.txt"]);
        focus(&mut d, Control::NameMask);
        typed(&mut d, "_x");
        focus(&mut d, Control::Case);
        d.handle_key(&key(KeyCode::Right));
        assert_ne!(*d.settings(), Settings::reset());

        let refresh = DialogKey {
            press: KeyPress::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            action: Some(Action::Reread),
        };
        d.handle_key(&refresh);
        assert_eq!(*d.settings(), Settings::reset());
        assert_eq!(new_names(&d), vec!["a.txt"]);

        // And the button does the same thing.
        focus(&mut d, Control::NameMask);
        typed(&mut d, "z");
        focus(&mut d, Control::Reset);
        d.handle_key(&key(KeyCode::Enter));
        assert_eq!(*d.settings(), Settings::reset());
    }

    #[test]
    fn undo_and_result_list_are_disabled_until_there_is_something_behind_them() {
        let mut d = dialog(&["a.txt"]);
        focus(&mut d, Control::Undo);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));
        focus(&mut d, Control::ResultList);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));

        let mut d = MultiRenameDialog::new(
            rows(&["a.txt"]),
            siblings(&["a.txt"]),
            Settings::reset(),
            true,
        )
        .with_result(true);
        focus(&mut d, Control::Undo);
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::MultiRename(answer)) => {
                assert!(answer.undo);
                assert!(answer.pairs.is_empty());
            }
            other => panic!("{other:?}"),
        }

        let mut d = MultiRenameDialog::new(
            rows(&["a.txt"]),
            siblings(&["a.txt"]),
            Settings::reset(),
            true,
        )
        .with_result(true);
        focus(&mut d, Control::ResultList);
        match d.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::MultiRename(answer)) => {
                assert!(answer.show_result);
                assert!(!answer.undo);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn esc_and_close_both_leave_without_renaming_anything() {
        let mut d = dialog(&["a.txt"]);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
        focus(&mut d, Control::Close);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn it_draws_at_every_size_the_spec_declares_usable() {
        // the design and at 60x15 the two mask fields, the counter, the
        // error line and the action row survive and there is no preview
        // table.
        let d = MultiRenameDialog::new(
            rows(&["a.txt", "b.txt"]),
            siblings(&["a.txt", "b.txt"]),
            Settings {
                name_mask: "[N]_[C]".to_string(),
                ..Settings::reset()
            },
            false,
        );
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        for (w, h) in [(200u16, 50u16), (120, 30), (80, 24), (60, 15)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let mut inner = Rect::default();
            terminal
                .draw(|f| {
                    inner = crate::dialog::draw(f, &d, f.area(), &style);
                })
                .expect("draw");
            let buf = terminal.backend().buffer().clone();
            let screen: String = (0..h)
                .map(|y| {
                    (0..w)
                        .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains("[N]_[C]"), "{w}x{h}:\n{screen}");
            assert!(screen.contains("Counter:"), "{w}x{h}:\n{screen}");
            assert!(screen.contains("Start!"), "{w}x{h}:\n{screen}");
            let has_table = screen.contains("New name");
            let says_why = screen.contains("preview needs a wider terminal");
            assert!(
                has_table != says_why,
                "one or the other, {w}x{h}:\n{screen}"
            );
            assert_eq!(
                has_table,
                inner.width >= PREVIEW_MIN_WIDTH,
                "{w}x{h} gave an interior {} wide:\n{screen}",
                inner.width
            );
        }
    }

    #[test]
    fn the_placeholder_hint_names_every_placeholder_the_parser_knows() {
        // the insert buttons, as a hint line.
        let hint = placeholder_hint();
        for part in DatePart::ALL {
            assert!(hint.contains(part.tag()), "{hint} lacks {}", part.tag());
        }
        for tag in ["[N]", "[C]", "[P]", "[E]"] {
            assert!(hint.contains(tag), "{hint} lacks {tag}");
        }
    }

    #[test]
    fn every_field_and_combo_is_reachable_by_its_alt_letter() {
        // the design on a plain field: the caret moves in and nothing else
        // changes.
        let mut d = dialog(&["a.txt", "b.txt"]);
        let before = d.settings().clone();
        for (letter, want) in [
            ('m', Control::NameMask),
            ('e', Control::ExtMask),
            ('s', Control::Search),
            ('w', Control::With),
            ('t', Control::CounterStart),
            ('p', Control::CounterStep),
            ('d', Control::CounterDigits),
        ] {
            d.handle_key(&alt(letter));
            assert_eq!(d.focused(), want, "Alt+{letter}");
        }
        assert_eq!(d.settings(), &before, "and none of them changed anything");
    }

    #[test]
    fn every_button_is_pressed_by_its_alt_letter() {
        // the design on a button: focus it and press it. `Alt+A` starts and
        // `Alt+S` is a `Search for` field, exactly as in the Find dialog - two
        // dialogs, one habit.
        let mut d = dialog(&["a.txt", "b.txt"]);
        focus(&mut d, Control::NameMask);
        typed(&mut d, "_x");
        assert!(d.can_start());
        match d.handle_key(&alt('a')) {
            DialogOutcome::Accept(DialogResult::MultiRename(answer)) => {
                assert_eq!(answer.pairs.len(), 2, "Alt+A pressed Start!");
                assert!(!answer.undo && !answer.show_result);
            }
            other => panic!("Alt+A pressed Start!, got {other:?}"),
        }

        let mut d = MultiRenameDialog::new(
            rows(&["a.txt"]),
            siblings(&["a.txt"]),
            Settings::reset(),
            true,
        );
        match d.handle_key(&alt('u')) {
            DialogOutcome::Accept(DialogResult::MultiRename(answer)) => assert!(answer.undo),
            other => panic!("Alt+U pressed Undo, got {other:?}"),
        }

        let mut d = dialog(&["a.txt"]).with_result(true);
        match d.handle_key(&alt('l')) {
            DialogOutcome::Accept(DialogResult::MultiRename(answer)) => {
                assert!(answer.show_result);
            }
            other => panic!("Alt+L pressed Result list, got {other:?}"),
        }

        let mut d = dialog(&["a.txt"]);
        focus(&mut d, Control::NameMask);
        typed(&mut d, "zzz");
        assert_ne!(d.settings().name_mask, "[N]");
        assert!(matches!(d.handle_key(&alt('r')), DialogOutcome::Consumed));
        assert_eq!(d.settings().name_mask, "[N]", "Alt+R reset the dialog");

        let mut d = dialog(&["a.txt"]);
        assert!(matches!(d.handle_key(&alt('c')), DialogOutcome::Cancel));
    }

    #[test]
    fn a_greyed_button_refuses_the_same_way_by_either_route() {
        // focusing a greyed control is not
        // "turning something on"; it is how a user reads why it is greyed. The
        // answer is the one `Enter` on it gives.
        let mut d = dialog(&["a.txt"]);
        assert!(matches!(d.handle_key(&alt('u')), DialogOutcome::Consumed));
        assert_eq!(d.focused(), Control::Undo, "and focus moved onto it");
        assert!(matches!(d.handle_key(&alt('l')), DialogOutcome::Consumed));
        assert_eq!(d.focused(), Control::ResultList);
    }

    #[test]
    fn a_mnemonic_never_turns_a_toggle_off() {
        // "a key that enabled on the way in and disabled on the
        // way back would make a repeated keystroke destructive, and the user is
        // reaching for it because they want to type there."
        let mut d = dialog(&["a.txt"]);
        let read = |d: &MultiRenameDialog, letter: char| -> bool {
            let r = &d.settings().replace;
            match letter {
                'x' => r.first_only,
                'g' => r.regex,
                'b' => r.substitute,
                'h' => r.match_case,
                _ => panic!("unlisted letter {letter}"),
            }
        };
        for letter in ['x', 'g', 'b', 'h'] {
            d.handle_key(&alt(letter));
            assert!(read(&d, letter), "Alt+{letter} ticked it");
            d.handle_key(&alt(letter));
            assert!(read(&d, letter), "Alt+{letter} again left it ticked");
        }
        // `Space` is how a toggle comes off, one keystroke away with the focus
        // now on it.
        d.handle_key(&alt('g'));
        d.handle_key(&key(KeyCode::Char(' ')));
        assert!(!d.settings().replace.regex);
    }

    #[test]
    fn a_mnemonic_never_types_into_a_mask_field() {
        // the design I8. The mnemonic check runs before any field
        // sees the key, and `DialogKey::text` is `None` under `ALT` in any
        // case.
        let mut d = dialog(&["a.txt", "b.txt"]);
        focus(&mut d, Control::NameMask);
        let before = d.settings().name_mask.clone();
        for letter in ['m', 'e', 's', 'w', 't', 'z', 'q'] {
            d.handle_key(&alt(letter));
            assert_eq!(d.settings().name_mask, before, "Alt+{letter}");
            focus(&mut d, Control::NameMask);
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

        // Three controls go without a letter, each for a reason, and
        // `Alt+N` is left unused because the design reserves it for a
        // `Cancel` this dialog does not have.
        let declared: Vec<Control> = MNEMONICS.iter().map(|(c, _)| *c).collect();
        let missing: Vec<Control> = CONTROLS
            .iter()
            .copied()
            .filter(|c| !declared.contains(c))
            .collect();
        assert_eq!(
            missing,
            vec![Control::IncludeExt, Control::Case, Control::Preview]
        );
        assert!(!seen.contains(&'n'), "Alt+N stays reserved for Cancel");

        // The action row's letters are a second list, so they are checked
        // against the first rather than trusted.
        let d = dialog(&["a.txt"]);
        let controls = [
            Control::Start,
            Control::Undo,
            Control::ResultList,
            Control::Reset,
            Control::Close,
        ];
        for width in [120u16, 60] {
            let labels = MultiRenameDialog::button_labels(width);
            for ((label, letter), control) in labels.iter().zip(BUTTON_MNEMONICS).zip(controls) {
                assert_eq!(letter, d.mnemonic_of(control), "{control:?} at {width}");
                let found = letter.and_then(|c| crate::dialog::split_mnemonic(label, c));
                assert!(found.is_some(), "{label:?} has no {letter:?} to underline");
            }
        }
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // Wide and tall enough that no label is cropped and no row is dropped
        // (the design I3), which is what makes "the set of
        // underlined cells is exactly the set of letters" an assertion rather
        // than an approximation.
        let d = dialog(&["a.txt", "b.txt"]);
        let mut want = d.mnemonic_letters();
        let mut got = underlined(&d, 130, 30);
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want, "underlines on screen");
    }
}
