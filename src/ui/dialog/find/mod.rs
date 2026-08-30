//! the Find Files dialog (`Alt+F7`).
//!
//! ```text
//!  ╭ Find files ────────────────────────────────────────────────────────────╮
//!  │ General │ Advanced │ Load/Save                                         │
//!  │Search for  *.rs                                        [ ] RegEx       │
//!  │Search in   /home/thorin/dev                            [>>]            │
//!  │Roots       ~/dev  +1                                   [ Devices ]     │
//!  │[ ] Only search in selected directories/files  [ ] Search archives      │
//!  │Subdirs     < all (unlimited depth) >                                   │
//!  │[x] Find text  TODO                                                     │
//!  │[ ] Whole words only  [ ] Case sensitive  [ ] RegEx  [ ] Hex            │
//!  │[ ] Find files NOT containing the text                                  │
//!  │[x] UTF-8  [ ] UTF-16  [ ] Latin-1 / windows-1252  [ ] CP437 (DOS)      │
//!  │                                                                        │
//!  │        [ Start search ]   [ Cancel ]   [ Help ]                        │
//!  ╰────────────────────────────────────────────────────────────────────────╯
//! ```
//!
//! Three tabs, exactly as the design names them: **General**, **Advanced**,
//! **Load/Save**. the design fix every control, its type and its default,
//! because the design says of the Advanced tab that "the exact TC field set
//! here has not been confirmed against a screenshot" - so the contract pins it
//! down from the own three nouns and this module implements that and nothing
//! beyond it.
//!
//! # "Find text" is a checkbox, in the type system
//!
//! This is the structural detail the design calls out and it is not a
//! rendering decision:
//!
//! * [`Query::content`] is an `Option<ContentQuery>` and [`Control::FindText`]
//!   is that `Option`. Unticked is `None`.
//! * A ticked box with an empty field is refused by [`Query::compile`], the one
//!   place a search is refused, so the dialog's message and the engine's cannot
//!   differ.
//! * An **unticked box with text in the field searches names only**, and the
//!   text stays in the field for next time. Nothing is inferred from a field
//!   being non-empty.
//!
//! The options below the box are drawn greyed and refuse `Space` while it is
//! off. They are drawn rather than hidden, because a control that appears when
//! a box is ticked makes the dialog jump and hides what the box is for. The
//! text field itself keeps accepting text while the box is off - that is the
//! case above, and a field that refused to be typed into could not produce it.
//!
//! # `Alt` mnemonics
//!
//! the closing paragraph: "Every control above carries an `Alt` mnemonic,
//! underlined in its label. `Alt+T` is **Find text** and ticks the checkbox on
//! its way to the field." With more than twenty controls across three tabs,
//! `Tab` alone is a form nobody fills in twice.
//!
//! The letters live in [`fields::GENERAL_MNEMONICS`],
//! [`fields::ADVANCED_MNEMONICS`] and [`fields::LOAD_SAVE_MNEMONICS`], one
//! table per tab, and the same table feeds both
//! the key handler and the underline in the label - so a key that reaches a
//! control and the letter drawn under it cannot drift apart.
//!
//! What a letter *does* is not written here: this dialog implements
//! [`Accelerated`] and the framework's [`Accelerated::mnemonic_key`] is the
//! one place the five behaviours are spelled out.
//! This module answers three questions about
//! itself - which letters are on the screen, what kind of control each one
//! names, and how to focus, tick or press it - and nothing else.
//!
//! # What the dialog does not do
//!
//! It does not touch the filesystem: the roots are
//! expanded with [`crate::panel::goto::expand`], which reads `$HOME` and the
//! environment and nothing else, and `searches.toml` is written by the event
//! loop from [`FindAnswer::saved`] rather than from `handle_key`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Clear;

use super::field::Field;
use super::tabbed::TabStrip;
use super::{checkbox, row};
use crate::config::units::ByteSize;
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FindAnswer,
    FocusRing, InputDialog, MessageDialog, Piece, draw_mnemonic, draw_mnemonic_buttons,
    draw_mnemonic_pieces, draw_text,
};
use crate::input::{DialogId, KeyCode, KeyModifiers, Milestone};
use crate::search::Session;
use crate::search::query::{
    AttrFilter, Charsets, ContentQuery, DateRange, Depth, MAX_ROOTS, NameMode, Query, SizeRange,
    TextMode, Tri,
};
use crate::search::saved::{DayEdge, History, SavedSearch, format_date, parse_date};
use crate::ui::text;
use crate::vfs::VfsPath;

pub mod draw;
pub mod fields;
pub mod keys;
pub mod validate;

use fields::{ADVANCED, GENERAL, LOAD_SAVE};
pub use fields::{Control, DateChoice, FieldId, Gate, Stepper, TabKind};

use validate::{default_depth_index, depth_index, fold_home, mask_text, parse_size, spell_size};

/// How many history entries the `Ctrl+Down` dropdown shows at once.
///
/// Eight rows is what fits under a field at the 60x15 floor without
/// covering the button row, and a combo box that hides its own `OK` is worse
/// than one that scrolls.
pub const DROPDOWN_ROWS: usize = 8;

/// The widest label column on a field row.
const LABEL_WIDTH: u16 = 12;

/// An open history dropdown: which field it belongs to and where its cursor is.
#[derive(Debug, Clone)]
struct Dropdown {
    control: Control,
    cursor: usize,
    items: Vec<String>,
}

/// the Find Files dialog.
pub struct FindDialog {
    tabs: TabStrip,
    tab: TabKind,
    /// One ring per tab, so `Tab` walks the tab you are on and returning to a
    /// tab returns to the control you left it on.
    general: FocusRing,
    advanced: FocusRing,
    load_save: FocusRing,

    // General
    name: Field,
    name_regex: bool,
    root: Field,
    /// The roots, as typed. Never empty; entry `root_cursor` is what the
    /// "Search in" field is editing.
    roots: Vec<String>,
    root_cursor: usize,
    /// The path the dialog was opened on, kept whole so a search started from
    /// inside an archive keeps its `#` segments.
    start: VfsPath,
    /// The panel's marks, for "Only search in selected directories/files".
    marked: Vec<VfsPath>,
    restrict: bool,
    archives: bool,
    depth: usize,
    find_text: bool,
    text: Field,
    whole_words: bool,
    case_sensitive: bool,
    text_regex: bool,
    hex: bool,
    inverted: bool,
    charsets: Charsets,

    // Advanced
    size_min: Field,
    size_max: Field,
    date_choice: DateChoice,
    after: Field,
    before: Field,
    days: Field,
    attrs: AttrFilter,

    // Load/Save
    saved: Vec<SavedSearch>,
    saved_cursor: usize,
    /// Whether the list has been changed and needs writing.
    saved_dirty: bool,

    history: History,
    dropdown: Option<Dropdown>,
    error: Option<String>,
}

impl FindDialog {
    /// Build the dialog from the panel it was opened on.
    ///
    /// `start` is the active panel's directory, which is what "Search in"
    /// defaults to. `marked` is the panel's marks: "Only search
    /// in selected directories/files" is **disabled when nothing is marked**,
    /// which is the own sentence and the reason this takes the list
    /// rather than a count.
    ///
    /// `state` is what the last opening left behind ([`Session`]): the previous
    /// query, the combo-box history, the saved searches and which tab was open.
    /// The previous query supplies every field **except the roots**, because
    /// the design fixes the start path as the active panel's directory and a
    /// stale root would send the next search somewhere the user is not looking.
    pub fn new(start: VfsPath, marked: Vec<VfsPath>, state: &Session) -> Self {
        let start_text = start.to_string();
        let mut dialog = Self {
            tabs: TabStrip::new(&TabKind::TITLES),
            tab: TabKind::General,
            general: FocusRing::new(GENERAL.len()),
            advanced: FocusRing::new(ADVANCED.len()),
            load_save: FocusRing::new(LOAD_SAVE.len()),

            name: Field::with_text("*"),
            name_regex: false,
            root: Field::with_text(start_text.clone()),
            roots: vec![start_text],
            root_cursor: 0,
            start,
            marked,
            restrict: false,
            archives: false,
            depth: default_depth_index(),
            find_text: false,
            text: Field::new(),
            whole_words: false,
            case_sensitive: false,
            text_regex: false,
            hex: false,
            inverted: false,
            charsets: Charsets::DEFAULT,

            size_min: Field::new(),
            size_max: Field::new(),
            date_choice: DateChoice::Any,
            after: Field::new(),
            before: Field::new(),
            days: Field::with_text("7"),
            attrs: AttrFilter::default(),

            saved: state.saved.clone(),
            saved_cursor: 0,
            saved_dirty: false,

            history: state.history.clone(),
            dropdown: None,
            error: None,
        };
        if let Some(last) = &state.last {
            dialog.apply(last, false);
        }
        dialog.set_tab(TabKind::from_index(state.tab));
        // Control 0 of every tab is the strip, which is where a fresh
        // [`FocusRing`] points and is not a control anything can be typed
        // into. Control 1 is the first real one of each tab - the mask, the
        // size range, the saved list - and the common case is
        // "type the mask, press Enter", which is only true if the mask is what
        // has focus when the dialog opens. All three rings, so `Alt+<n>` onto
        // another tab lands on that tab's first control rather than back on
        // the strip. `Shift+Tab` still reaches the strip in one key.
        dialog.general.set(1);
        dialog.advanced.set(1);
        dialog.load_save.set(1);
        // The mask is what a repeated search retypes first, so it opens
        // selected: one keystroke replaces it, exactly as the copy dialog's
        // mask does.
        let len = dialog.name.text().chars().count();
        dialog.name.select(0, len);
        dialog
    }

    /// Open on these search roots instead of the panel's directory
    /// (the `search_in_panel`).
    ///
    /// the `>>` button appends roots by hand; this is the same
    /// list arrived at by a keystroke, so `Alt+Shift+F7` can hand the dialog
    /// the rows the user marked. Capped at [`MAX_ROOTS`] for the same reason
    /// `>>` is: a query that cannot compile must not be reachable from a
    /// button.
    #[must_use]
    pub fn with_roots(mut self, roots: Vec<VfsPath>) -> Self {
        if roots.is_empty() {
            return self;
        }
        self.roots = roots
            .iter()
            .take(MAX_ROOTS)
            .map(ToString::to_string)
            .collect();
        self.root_cursor = 0;
        self.sync_root_field();
        self
    }

    /// Fill the controls from a query.
    ///
    /// `roots` says whether the query's roots replace the panel's - `Load` on
    /// the Load/Save tab means the whole saved search, and reopening the dialog
    /// does not.
    fn apply(&mut self, query: &Query, roots: bool) {
        self.name.set_text(if query.name.is_empty() {
            "*".to_string()
        } else {
            query.name.clone()
        });
        self.name_regex = matches!(query.name_mode, NameMode::Regex);
        if roots && !query.roots.is_empty() {
            self.roots = query.roots.iter().map(ToString::to_string).collect();
            self.root_cursor = 0;
            self.sync_root_field();
        }
        self.archives = query.search_archives;
        self.depth = depth_index(query.depth);
        match &query.content {
            Some(content) => {
                self.find_text = true;
                self.text.set_text(content.pattern.clone());
                self.whole_words = content.whole_words;
                self.case_sensitive = content.case_sensitive;
                self.text_regex = matches!(content.mode, TextMode::Regex);
                self.hex = matches!(content.mode, TextMode::Hex);
                self.inverted = content.inverted;
                self.charsets = content.charsets;
            }
            None => self.find_text = false,
        }
        self.size_min.set_text(spell_size(query.size.min));
        self.size_max.set_text(spell_size(query.size.max));
        match query.date {
            DateRange::Any => self.date_choice = DateChoice::Any,
            DateRange::Between { after, before } => {
                self.date_choice = DateChoice::Between;
                self.after
                    .set_text(after.map(format_date).unwrap_or_default());
                self.before
                    .set_text(before.map(format_date).unwrap_or_default());
            }
            DateRange::NewerThanDays(days) => {
                self.date_choice = DateChoice::Newer;
                self.days.set_text(days.to_string());
            }
        }
        self.attrs = query.attrs;
        self.error = None;
    }

    /// What the dialog has collected so far, whether or not it is valid.
    ///
    /// A field that does not parse contributes nothing rather than a guess -
    /// [`FindDialog::validate`] is what refuses it, on the tab that holds it,
    /// before a search can start.
    pub fn query(&self) -> Query {
        let roots = self.query_roots();
        let mut query = Query::new(roots.first().cloned().unwrap_or_else(|| self.start.clone()));
        query.roots = roots;
        query.name = mask_text(self.name.text());
        query.name_mode = if self.name_regex {
            NameMode::Regex
        } else {
            NameMode::Glob
        };
        query.restrict = if self.restrict_available() && self.restrict {
            self.marked.clone()
        } else {
            Vec::new()
        };
        query.search_archives = self.archives;
        query.depth = self.depth();
        // The checkbox *is* the `Option`. Not the field being non-empty.
        query.content = self.find_text.then(|| ContentQuery {
            pattern: self.text.text().to_string(),
            mode: self.text_mode(),
            whole_words: self.whole_words,
            case_sensitive: self.case_sensitive,
            inverted: self.inverted,
            charsets: self.charsets,
        });
        query.size = SizeRange {
            min: parse_size(self.size_min.text()).ok().flatten(),
            max: parse_size(self.size_max.text()).ok().flatten(),
        };
        query.date = self.date_range();
        query.attrs = self.attrs;
        query
    }

    /// The refusal currently shown, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The saved-search list as the Load/Save tab has it.
    pub fn saved(&self) -> &[SavedSearch] {
        &self.saved
    }

    /// Which tab is open, so the event loop can remember it.
    pub const fn tab(&self) -> TabKind {
        self.tab
    }

    /// The answer to the `Save as…` name prompt (`DialogId::SaveSearch`).
    ///
    /// Called by [`crate::input::dialog_accepted`] after downcasting, exactly
    /// as the copy dialog's `+ F7` answer reaches
    /// [`super::CopyMoveDialog::set_target`]: the prompt is pushed on top of
    /// this dialog and its answer belongs to the dialog underneath. The write
    /// itself happens in the event loop, from [`FindAnswer::saved`].
    ///
    /// An existing name is **replaced** rather than duplicated, because a list
    /// with two `nightly builds` in it cannot be told apart in the list box.
    pub fn save_as(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.error = Some("a saved search needs a name".to_string());
            return;
        }
        let entry = SavedSearch::new(name, &self.query());
        match self.saved.iter().position(|s| s.name == name) {
            Some(at) => {
                if let Some(slot) = self.saved.get_mut(at) {
                    *slot = entry;
                }
                self.saved_cursor = at;
            }
            None => {
                self.saved.push(entry);
                self.saved_cursor = self.saved.len().saturating_sub(1);
            }
        }
        self.saved_dirty = true;
        self.error = None;
    }

    // ------------------------------------------------------------ state ----

    /// The focus ring of the open tab.
    const fn ring(&self) -> &FocusRing {
        match self.tab {
            TabKind::General => &self.general,
            TabKind::Advanced => &self.advanced,
            TabKind::LoadSave => &self.load_save,
        }
    }

    /// The focus ring of the open tab, mutably.
    const fn ring_mut(&mut self) -> &mut FocusRing {
        match self.tab {
            TabKind::General => &mut self.general,
            TabKind::Advanced => &mut self.advanced,
            TabKind::LoadSave => &mut self.load_save,
        }
    }

    /// The focused control.
    fn focused(&self) -> Control {
        self.tab
            .order()
            .get(self.ring().index())
            .copied()
            .unwrap_or(Control::Tabs)
    }

    /// Move focus to a named control, if the open tab has one.
    fn focus(&mut self, control: Control) {
        if let Some(at) = self.tab.order().iter().position(|c| *c == control) {
            self.ring_mut().set(at);
        }
    }

    /// Open a tab, keeping the strip and the ring in step.
    fn set_tab(&mut self, tab: TabKind) {
        self.tab = tab;
        self.tabs.set_active(tab.index());
    }

    /// Why this control is greyed, or [`Gate::Live`].
    ///
    /// "Every option below is greyed out until it is checked",
    /// and "Only search in selected directories/files ... Disabled when nothing
    /// is marked". The date fields follow the design's
    /// "enabled by `between`" and "enabled by `newer than`" - they are drawn
    /// greyed until the `Modified` radio reads them, and typing into one
    /// selects that mode rather than being swallowed.
    ///
    /// The text field is **not** gated: an unticked box with text in it
    /// searches names only, and a field that could not be typed into could
    /// not produce that case.
    fn gate(&self, control: Control) -> Gate {
        match control {
            Control::WholeWords
            | Control::CaseSensitive
            | Control::TextRegex
            | Control::Hex
            | Control::Inverted
            | Control::Utf8
            | Control::Utf16
            | Control::Latin1
            | Control::Cp437 => {
                if self.find_text {
                    Gate::Live
                } else {
                    Gate::FindText
                }
            }
            Control::Restrict => {
                if self.restrict_available() {
                    Gate::Live
                } else {
                    Gate::Marks
                }
            }
            Control::After | Control::Before => {
                if self.date_choice == DateChoice::Between {
                    Gate::Live
                } else {
                    Gate::DateMode
                }
            }
            Control::Days => {
                if self.date_choice == DateChoice::Newer {
                    Gate::Live
                } else {
                    Gate::DateMode
                }
            }
            Control::Load | Control::Delete => {
                if self.saved.is_empty() {
                    Gate::SavedList
                } else {
                    Gate::Live
                }
            }
            Control::Tabs
            | Control::Name
            | Control::NameRegex
            | Control::Root
            | Control::RootAdd
            | Control::RootList
            | Control::Devices
            | Control::Archives
            | Control::Depth
            | Control::FindText
            | Control::Text
            | Control::SizeMin
            | Control::SizeMax
            | Control::Modified
            | Control::AttrDirectories
            | Control::AttrHidden
            | Control::AttrExecutable
            | Control::AttrSymlinks
            | Control::AttrReadOnly
            | Control::SavedList
            | Control::SaveAs
            | Control::Start
            | Control::Cancel
            | Control::Help => Gate::Live,
        }
    }

    /// Is this control live, or drawn greyed?
    fn enabled(&self, control: Control) -> bool {
        self.gate(control) == Gate::Live
    }

    /// the restrict box is disabled when nothing is marked.
    fn restrict_available(&self) -> bool {
        !self.marked.is_empty()
    }

    /// The chosen depth.
    fn depth(&self) -> Depth {
        Depth::CHOICES
            .get(self.depth)
            .copied()
            .unwrap_or(Depth::Unlimited)
    }

    /// Which of the three text modes the two checkboxes name.
    ///
    /// `Hex` and `RegEx` are mutually exclusive, which is enforced where they
    /// are ticked rather than here; if both were somehow set, hex wins, because
    /// a hex pattern read as a regex is a different search rather than a failed
    /// one.
    fn text_mode(&self) -> TextMode {
        if self.hex {
            TextMode::Hex
        } else if self.text_regex {
            TextMode::Regex
        } else {
            TextMode::Plain
        }
    }

    /// The date range the radio and its fields describe, leniently: an
    /// unparseable date is no bound at that end.
    fn date_range(&self) -> DateRange {
        match self.date_choice {
            DateChoice::Any => DateRange::Any,
            DateChoice::Between => DateRange::Between {
                after: parse_date(self.after.text(), DayEdge::Start),
                before: parse_date(self.before.text(), DayEdge::End),
            },
            // A zero is carried through as a zero, so `Query::compile` is what
            // refuses it and says so once. Only text that is not a number at
            // all has nothing to carry.
            DateChoice::Newer => match self.days.text().trim().parse::<u32>() {
                Ok(days) => DateRange::NewerThanDays(days),
                Err(_) => DateRange::Any,
            },
        }
    }

    /// The roots, expanded. Never empty, and never more than [`MAX_ROOTS`].
    ///
    /// A root whose text is exactly the path the dialog was opened on keeps
    /// that [`VfsPath`] whole, so a search started inside an archive is a
    /// search inside the archive rather than of a local path that looks like
    /// one (the last bullet).
    fn query_roots(&self) -> Vec<VfsPath> {
        let start_text = self.start.to_string();
        let base = self.start.local_path();
        let mut out: Vec<VfsPath> = Vec::new();
        for text in &self.roots {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let path = if text == start_text {
                self.start.clone()
            } else {
                // A root belongs to the backend the panel is on, and the text
                // cannot say which that is: `VfsPath`'s `Display` writes the
                // segment paths and never the backend, so a marked row on a
                // connection stringifies to `/home/user/docs` and is
                // indistinguishable from a local path of the same name. This
                // used to hand every such root to `VfsPath::local`, so
                // `Alt+Shift+F7` over a connected panel searched this machine
                // instead of the server, silently and with plausible results.
                // The same mistake, and the same fix, as `Ctrl+G` in
                // `input::dialogs`.
                match base {
                    Some(here) => match crate::panel::goto::expand(text, Some(here)) {
                        Ok(path) => VfsPath::local(path),
                        Err(_) => continue,
                    },
                    None => match crate::panel::goto::expand(text, Some(self.start.tail())) {
                        Ok(path) => self.start.with_tail(path),
                        Err(_) => continue,
                    },
                }
            };
            if !out.contains(&path) {
                out.push(path);
            }
            if out.len() >= MAX_ROOTS {
                break;
            }
        }
        if out.is_empty() {
            out.push(self.start.clone());
        }
        out
    }

    /// Point the "Search in" field at the focused root.
    fn sync_root_field(&mut self) {
        let text = self
            .roots
            .get(self.root_cursor)
            .cloned()
            .unwrap_or_default();
        self.root.set_text(text);
    }

    /// Write the "Search in" field back into the focused root.
    fn sync_root_entry(&mut self) {
        let text = self.root.text().to_string();
        if let Some(slot) = self.roots.get_mut(self.root_cursor) {
            *slot = text;
        }
    }
}

/// Which of `buttons` has focus, for [`crate::dialog::draw_buttons`].
///
/// `usize::MAX` when none of them does, which is that function's own way of
/// spelling "no button is highlighted".
fn button_index(focused: Control, buttons: &[Control]) -> usize {
    buttons
        .iter()
        .position(|c| *c == focused)
        .unwrap_or(usize::MAX)
}

/// A radio button: `(x) between`.
fn radio(label: &str, on: bool) -> String {
    format!("({}) {label}", if on { 'x' } else { ' ' })
}

/// A tri-state: `[x] Hidden`, `[-] Hidden`, `[ ] Hidden`
/// (the design - "not hidden" is as much a filter as
/// "hidden", and a two-state control can only express one of them).
fn tristate(label: &str, tri: Tri) -> String {
    format!("[{}] {label}", tri.label())
}

/// How wide a trailing button is, plus the space before it.
fn side_button_width(label: &str) -> u16 {
    u16::try_from(text::width(label).saturating_add(2)).unwrap_or(0)
}

/// The label column of the "Find text" row: the checkbox itself.
fn find_text_width() -> u16 {
    u16::try_from(text::width("[ ] Find text  ")).unwrap_or(LABEL_WIDTH)
}

/// Split a row into a label column, a field, and a trailing button.
///
/// Every part may come back zero-width, which every drawing function in
/// `ui::dialog` already treats as "do not draw" (no `Rect` without
/// checking it first).
fn columns(rect: Rect, label: u16, trailing: u16) -> (Rect, Rect, Rect) {
    let label = label.min(rect.width);
    let rest = rect.width.saturating_sub(label);
    // The field keeps at least four columns before the trailing button is
    // given any: a two-column field is not a field.
    let trailing = if rest > trailing.saturating_add(4) {
        trailing
    } else {
        0
    };
    let field = rest.saturating_sub(trailing);
    (
        Rect::new(rect.x, rect.y, label, 1),
        Rect::new(rect.x.saturating_add(label), rect.y, field, 1),
        Rect::new(
            rect.x.saturating_add(label).saturating_add(field),
            rect.y,
            trailing,
            1,
        ),
    )
}

/// The left or right half of a row.
fn half(rect: Rect, left: bool) -> Rect {
    let width = rect.width / 2;
    if left {
        Rect::new(rect.x, rect.y, width, 1)
    } else {
        Rect::new(
            rect.x.saturating_add(width),
            rect.y,
            rect.width.saturating_sub(width),
            1,
        )
    }
}

/// the `Alt` mnemonics, through the framework's one implementation
/// of them.
///
/// Everything this dialog says about an accelerator is said here, in three
/// small answers: which letters are on the screen, what kind of control each
/// one names, and how to focus, tick or press it.
/// [`Accelerated::mnemonic_key`] is what turns those into the design's
/// behaviour, and it is the framework's, so this dialog cannot spell "never
/// turns anything off" differently from the one beside it.
impl Accelerated for FindDialog {
    type Control = Control;

    /// The open tab's table and nothing else.
    ///
    /// Only one tab's controls are on the screen, and only its letters can be
    /// read off it, so `Alt+S` is **Search for** on General and `Size >=` on
    /// Advanced. the uniqueness rule is enforced over each of these
    /// tables, which is the whole of what a user can reach without pressing
    /// `Alt`+digit first.
    fn mnemonics(&self) -> &'static [(Control, char)] {
        self.tab.mnemonics()
    }

    /// What `Alt`+letter does once it has found its control.
    ///
    /// Exhaustive over [`Control`], so a control added to this dialog later
    /// has to be classified here rather than inheriting a neighbour's meaning.
    /// Nothing is [`Accel::Absent`]: a letter is only ever looked up in the
    /// open tab's own table, so a control that is not on the screen cannot be
    /// named by one in the first place.
    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            // the named accelerator: the checkbox is ticked and
            // the caret lands in the field it gates.
            Control::FindText => Accel::Gate(Control::Text),
            Control::NameRegex
            | Control::Restrict
            | Control::Archives
            | Control::WholeWords
            | Control::CaseSensitive
            | Control::TextRegex
            | Control::Hex
            | Control::Inverted
            | Control::Utf8
            | Control::Utf16
            | Control::Latin1
            | Control::Cp437 => Accel::Check,
            Control::RootAdd
            | Control::Devices
            | Control::Load
            | Control::SaveAs
            | Control::Delete
            | Control::Start
            | Control::Cancel
            | Control::Help => Accel::Press,
            // Fields, lists, the `Subdirs` dropdown, the `Modified` radio and
            // the five tri-states: focus and nothing else.
            // the design gives no
            // meaning to "accelerate a three-way control", and both candidate
            // meanings would turn one of its states off.
            Control::Tabs
            | Control::Name
            | Control::Root
            | Control::RootList
            | Control::Depth
            | Control::Text
            | Control::SizeMin
            | Control::SizeMax
            | Control::Modified
            | Control::After
            | Control::Before
            | Control::Days
            | Control::AttrDirectories
            | Control::AttrHidden
            | Control::AttrExecutable
            | Control::AttrSymlinks
            | Control::AttrReadOnly
            | Control::SavedList => Accel::Focus,
        }
    }

    /// A no-op for a control the open tab does not have in its ring, which is
    /// what [`FindDialog::focus`] already does.
    fn focus_control(&mut self, control: Control) {
        self.focus(control);
    }

    /// Switch a checkbox **on**, or do nothing at all.
    ///
    /// Deliberately not [`FindDialog::toggle`]: that is `Space`, which means
    /// "the other one of the two", and this is an accelerator, which means
    /// "the option I just named". A repeated `Alt`+letter therefore leaves the
    /// box ticked instead of undoing itself, which is the rule the design
    /// spends its longest sentence on. The tri-states are not checkboxes and
    /// are left alone; so is a control the gate has greyed, whose refusal is
    /// reported the same way `Space`'s is.
    fn switch_on(&mut self, control: Control) {
        let already_on = match control {
            Control::NameRegex => self.name_regex,
            Control::Restrict => self.restrict,
            Control::Archives => self.archives,
            Control::FindText => self.find_text,
            Control::WholeWords => self.whole_words,
            Control::CaseSensitive => self.case_sensitive,
            Control::TextRegex => self.text_regex,
            Control::Hex => self.hex,
            Control::Inverted => self.inverted,
            Control::Utf8 => self.charsets.utf8,
            Control::Utf16 => self.charsets.utf16,
            Control::Latin1 => self.charsets.latin1,
            Control::Cp437 => self.charsets.cp437,
            // Not checkboxes. Focus has already moved; there is nothing else
            // an accelerator can mean here.
            Control::Tabs
            | Control::Name
            | Control::Root
            | Control::RootAdd
            | Control::RootList
            | Control::Devices
            | Control::Depth
            | Control::Text
            | Control::SizeMin
            | Control::SizeMax
            | Control::Modified
            | Control::After
            | Control::Before
            | Control::Days
            | Control::AttrDirectories
            | Control::AttrHidden
            | Control::AttrExecutable
            | Control::AttrSymlinks
            | Control::AttrReadOnly
            | Control::SavedList
            | Control::Load
            | Control::SaveAs
            | Control::Delete
            | Control::Start
            | Control::Cancel
            | Control::Help => return,
        };
        if already_on {
            // On is where the accelerator wants it. Nothing to do, and above
            // all nothing to undo.
            self.error = None;
            return;
        }
        // One `toggle` from off is one tick, and it carries the gate's refusal
        // and the `Hex`/`RegEx` exclusion with it.
        let _ = self.toggle(control);
    }

    /// Press a button, which is the only thing [`Accel::Press`] names.
    ///
    /// [`Accelerated::mnemonic_key`] has already moved focus here,
    /// so a button that pushes another dialog
    /// leaves the focus somewhere sensible for when that dialog closes.
    fn press(&mut self, control: Control) -> DialogOutcome {
        self.activate_control(control)
    }
}

impl Dialog for FindDialog {
    fn id(&self) -> DialogId {
        DialogId::Find
    }

    fn title(&self) -> String {
        "Find files".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        // The charset row is the widest thing here, and twelve interior rows
        // is the General tab: the strip, nine control rows, a hint line and
        // the buttons. Both are clamped by `centred`, which is why every row
        // below is laid out against the rectangle it is actually given.
        (76, 14)
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if self.dropdown.is_some() {
            return self.dropdown_key(key);
        }
        // The tab strip first: `Alt+<n>` selects a tab from anywhere in the
        // dialog, which is the accelerator a legacy terminal can still reach.
        //
        if self.tabs.handle(key, self.ring().is(0)) {
            let tab = TabKind::from_index(self.tabs.active());
            self.set_tab(tab);
            return DialogOutcome::Consumed;
        }
        // Then `Alt+<letter>`, before anything that reads `key.action`: the
        // mnemonics are what make a form with more than twenty controls across
        // three tabs usable from the keyboard at all, and inside a dialog they win
        // over the global `Alt` bindings (the fixed order, step 3). A user who has
        // bound `alt+x` to `clear` in `[global]` must still get `Hex` here.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring_mut().handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        let focused = self.focused();

        // `F1` is the Help button, wherever focus is.
        if key.press.code == KeyCode::F(1) {
            return Self::help();
        }
        // `Ctrl+Down` opens a combo box's history.
        if key.press.code == KeyCode::Down
            && key.press.mods.contains(KeyModifiers::CONTROL)
            && matches!(focused, Control::Name | Control::Root | Control::Text)
        {
            return self.open_dropdown(focused);
        }
        if key.is_accept() {
            return self.activate();
        }
        if key.press.code == KeyCode::Char(' ') {
            // A button is pressed, everything else is toggled: `Space` and
            // `Enter` agree on a button, which is what a keyboard-driven form
            // is expected to do.
            if Self::is_button(focused) {
                return self.activate();
            }
            match self.toggle(focused) {
                DialogOutcome::Ignored => {}
                outcome => return outcome,
            }
        }
        if focused == Control::RootList && key.press.code == KeyCode::Delete {
            return self.drop_root();
        }

        // A greyed date field claims its mode as soon as it is typed into,
        // rather than swallowing the keystroke: with no mouse to click the
        // radio with first, "enabled by `between`" can only mean this.
        if !self.enabled(focused) && key.text().is_some() {
            self.claim_date_mode(focused);
        }
        // A focused field takes the key before the ring's movement does, so
        // `Left` in a path field moves the caret rather than the focus.
        if self.enabled(focused)
            && let Some(field) = self.field(focused)
        {
            let before = field.text().to_string();
            if field.handle(key) {
                let changed = before != field.text();
                if changed {
                    self.error = None;
                    if focused == Control::Root {
                        self.sync_root_entry();
                    }
                }
                return DialogOutcome::Consumed;
            }
        }

        match key.press.code {
            KeyCode::Left | KeyCode::Right => {
                let forward = key.press.code == KeyCode::Right;
                let stepper = Self::stepper(focused);
                if stepper == Stepper::None {
                    self.step_focus(forward);
                } else {
                    self.step(stepper, forward);
                }
                DialogOutcome::Consumed
            }
            KeyCode::Up | KeyCode::Down => {
                let forward = key.press.code == KeyCode::Down;
                let stepper = Self::stepper(focused);
                if stepper.vertical() {
                    self.step(stepper, forward);
                } else {
                    self.step_focus(forward);
                }
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
        if let Some(rect) = row(area, 0) {
            self.tabs.render(f, rect, style, self.ring().is(0));
        }
        match self.tab {
            TabKind::General => self.render_general(f, area, style, focused),
            TabKind::Advanced => self.render_advanced(f, area, style, focused),
            TabKind::LoadSave => self.render_load_save(f, area, style, focused),
        }

        // The hint line, one row above the buttons: the refusal if there is
        // one, otherwise what the greyed controls are waiting for.
        let hint_row = area.height.saturating_sub(2);
        if let Some(rect) = row(area, hint_row) {
            match (&self.error, self.tab) {
                (Some(error), _) => draw_text(f, rect, error, style.button(true), style.ascii),
                (None, TabKind::General) if !self.find_text => draw_text(
                    f,
                    rect,
                    "tick Find text to search file contents",
                    body,
                    style.ascii,
                ),
                (None, TabKind::Advanced) => {
                    draw_text(f, rect, &self.advanced_summary(), body, style.ascii);
                }
                (None, TabKind::General | TabKind::LoadSave) => {}
            }
        }
        if let Some(rect) = row(area, area.height.saturating_sub(1)) {
            let index = button_index(focused, &[Control::Start, Control::Cancel, Control::Help]);
            draw_mnemonic_buttons(
                f,
                rect,
                &[
                    ("Start search", self.mnemonic_of(Control::Start)),
                    ("Cancel", self.mnemonic_of(Control::Cancel)),
                    ("Help", self.mnemonic_of(Control::Help)),
                ],
                index,
                style,
            );
        }
        self.render_dropdown(f, area, style);
    }

    /// The open tab's letters and no others (the uniqueness test).
    ///
    /// Per instance and not per type, because the answer changes with the tab:
    /// three tables, three screens, three sets of letters.
    ///
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if self.dropdown.is_some() {
            return None;
        }
        let focused = self.focused();
        let rect = self.field_rect(area, focused)?;
        self.field_of(focused)?.cursor(rect)
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn typed(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::plain(KeyCode::Char(c)))
    }

    fn state() -> Session {
        Session::default()
    }

    fn dialog() -> FindDialog {
        FindDialog::new(VfsPath::local("/home/thorin/dev"), Vec::new(), &state())
    }

    fn marked_dialog() -> FindDialog {
        FindDialog::new(
            VfsPath::local("/home/thorin/dev"),
            vec![VfsPath::local("/home/thorin/dev/src")],
            &state(),
        )
    }

    /// A panel sitting on a connection, with two of its rows handed to the
    /// dialog as roots, which is what `Alt+Shift+F7` does (`input::search`).
    fn remote_dialog() -> FindDialog {
        let id = crate::remote::RemoteId(7);
        FindDialog::new(id.path("/home/user"), Vec::new(), &state())
            .with_roots(vec![id.path("/home/user/docs"), id.path("/home/user/src")])
    }

    /// `Alt+Shift+F7` over a connection searches the connection.
    ///
    /// The roots reach the dialog as text and text cannot say which backend it
    /// belongs to: `VfsPath`'s `Display` writes the segment paths and never the
    /// backend, so a marked remote row and a local path of the same name are
    /// the same characters. Rebuilding them as local paths searched this
    /// machine and reported plausible hits from it, which is the worst shape a
    /// wrong answer can take.
    #[test]
    fn a_search_over_a_connected_panel_stays_on_the_connection() {
        let roots = remote_dialog().query_roots();
        assert_eq!(roots.len(), 2, "{roots:?}");
        for root in &roots {
            assert_eq!(
                root.backend(),
                crate::vfs::BackendKind::Remote(crate::remote::RemoteId(7)),
                "{root} left the connection"
            );
            assert_eq!(root.local_path(), None, "{root} is addressable locally");
        }
        let named: Vec<String> = roots.iter().map(ToString::to_string).collect();
        assert_eq!(named, ["/home/user/docs", "/home/user/src"], "{named:?}");
    }

    /// The panel's own directory is still the root when nothing is marked, and
    /// it is still the connection's.
    #[test]
    fn a_connected_panel_with_no_marks_searches_its_own_directory() {
        let id = crate::remote::RemoteId(7);
        let dialog = FindDialog::new(id.path("/home/user"), Vec::new(), &state());
        let roots = dialog.query_roots();
        assert_eq!(roots, vec![id.path("/home/user")], "{roots:?}");
    }

    /// And a local panel is unchanged by the fix, including a relative root,
    /// which still resolves against the panel's own directory.
    #[test]
    fn a_local_search_still_resolves_against_the_local_directory() {
        let dialog = FindDialog::new(VfsPath::local("/home/thorin/dev"), Vec::new(), &state())
            .with_roots(vec![VfsPath::local("/home/thorin/dev/src")]);
        assert_eq!(
            dialog.query_roots(),
            vec![VfsPath::local("/home/thorin/dev/src")]
        );
    }

    /// Focus a control, opening the tab that holds it first: the three tabs
    /// have three focus rings and a control is only reachable from its own.
    fn focus(dialog: &mut FindDialog, control: Control) {
        for tab in [TabKind::General, TabKind::Advanced, TabKind::LoadSave] {
            if tab.order().contains(&control) {
                dialog.set_tab(tab);
                break;
            }
        }
        dialog.focus(control);
        assert_eq!(dialog.focused(), control);
    }

    fn type_text(dialog: &mut FindDialog, text: &str) {
        for c in text.chars() {
            dialog.handle_key(&typed(c));
        }
    }

    #[test]
    fn it_opens_on_the_general_tab_with_find_text_unticked() {
        // Acceptance criterion 1: three tabs, the start path pre-filled, and
        // `Find text` unticked with its options greyed.
        let dialog = dialog();
        assert_eq!(dialog.tab(), TabKind::General);
        assert_eq!(TabKind::TITLES.len(), 3);
        assert_eq!(dialog.root.text(), "/home/thorin/dev");
        assert!(!dialog.find_text);
        assert_eq!(dialog.query().content, None);
        for control in [
            Control::WholeWords,
            Control::CaseSensitive,
            Control::TextRegex,
            Control::Hex,
            Control::Inverted,
            Control::Utf8,
            Control::Utf16,
            Control::Latin1,
            Control::Cp437,
        ] {
            assert!(!dialog.enabled(control), "{control:?}");
        }
        assert_eq!(dialog.charsets, Charsets::DEFAULT, "UTF-8 alone");
        assert_eq!(dialog.depth(), Depth::Unlimited);
        assert!(
            dialog.attrs.is_any(),
            "an untouched Advanced tab filters nothing"
        );
        assert!(dialog.query().size.is_any());
    }

    #[test]
    fn the_contract_focus_order_is_the_ring() {
        // the numbers in that table are the ring
        // indices, and three agents read them.
        assert_eq!(GENERAL.get(10), Some(&Control::FindText));
        assert_eq!(GENERAL.get(21), Some(&Control::Start));
        assert_eq!(GENERAL.len(), 24);
        assert_eq!(ADVANCED.first(), Some(&Control::Tabs));
        assert_eq!(LOAD_SAVE.get(1), Some(&Control::SavedList));
        for order in [GENERAL, ADVANCED, LOAD_SAVE] {
            let tail = order.len().saturating_sub(3);
            assert_eq!(order.get(tail), Some(&Control::Start));
            assert_eq!(order.last(), Some(&Control::Help));
        }
    }

    #[test]
    fn content_search_is_the_checkbox_and_not_the_field() {
        // Invariant 16, the "important structural detail", and
        // acceptance criterion 3.
        let mut dialog = dialog();
        focus(&mut dialog, Control::Text);
        type_text(&mut dialog, "TODO");
        assert_eq!(dialog.text.text(), "TODO", "the field types while unticked");
        assert_eq!(
            dialog.query().content,
            None,
            "an unticked box searches names only, whatever the field holds"
        );

        focus(&mut dialog, Control::FindText);
        dialog.handle_key(&typed(' '));
        let content = dialog.query().content.expect("ticking is the Some");
        assert_eq!(content.pattern, "TODO");
        assert_eq!(content.mode, TextMode::Plain);
        assert_eq!(content.charsets, Charsets::DEFAULT);

        // And a ticked box with an empty field is a refusal, not a silent
        // downgrade to a name-only search.
        dialog.text.set_text("");
        focus(&mut dialog, Control::Start);
        let outcome = dialog.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, DialogOutcome::Consumed), "{outcome:?}");
        assert!(dialog.error().is_some(), "with the reason on the screen");
        assert!(dialog.query().content.is_some(), "and the box stays ticked");
    }

    #[test]
    fn the_options_below_find_text_refuse_space_until_it_is_ticked() {
        // drawn greyed, and they refuse `Space`.
        let mut dialog = dialog();
        for control in [
            Control::WholeWords,
            Control::CaseSensitive,
            Control::TextRegex,
            Control::Hex,
            Control::Inverted,
            Control::Utf16,
        ] {
            focus(&mut dialog, control);
            dialog.handle_key(&typed(' '));
        }
        assert!(!dialog.whole_words);
        assert!(!dialog.case_sensitive);
        assert!(!dialog.text_regex);
        assert!(!dialog.hex);
        assert!(!dialog.inverted);
        assert!(!dialog.charsets.utf16);
        assert_eq!(
            dialog.error(),
            Some("tick Find text to search file contents")
        );

        // Ticked, every one of them answers.
        focus(&mut dialog, Control::FindText);
        dialog.handle_key(&typed(' '));
        for control in [Control::WholeWords, Control::Inverted, Control::Utf16] {
            focus(&mut dialog, control);
            dialog.handle_key(&typed(' '));
        }
        assert!(dialog.whole_words);
        assert!(dialog.inverted);
        assert!(dialog.charsets.utf16);
        // Unticking gates them again without losing what they hold.
        focus(&mut dialog, Control::FindText);
        dialog.handle_key(&typed(' '));
        assert!(!dialog.enabled(Control::WholeWords));
        assert!(dialog.whole_words, "the answers survive the gate closing");
    }

    #[test]
    fn hex_and_regex_are_mutually_exclusive() {
        let mut dialog = dialog();
        focus(&mut dialog, Control::FindText);
        dialog.handle_key(&typed(' '));
        focus(&mut dialog, Control::TextRegex);
        dialog.handle_key(&typed(' '));
        focus(&mut dialog, Control::Hex);
        dialog.handle_key(&typed(' '));
        assert!(dialog.hex);
        assert!(!dialog.text_regex, "the one just ticked wins");
        dialog.text.set_text("DE AD BE EF");
        let content = dialog.query().content.expect("ticked");
        assert_eq!(content.mode, TextMode::Hex);
    }

    #[test]
    fn only_search_in_selected_is_disabled_when_nothing_is_marked() {
        // the design says so in as many words.
        let mut dialog = dialog();
        focus(&mut dialog, Control::Restrict);
        dialog.handle_key(&typed(' '));
        assert!(!dialog.restrict);
        assert_eq!(dialog.error(), Some("nothing is marked in the panel"));
        assert!(dialog.query().restrict.is_empty());

        let mut dialog = marked_dialog();
        focus(&mut dialog, Control::Restrict);
        dialog.handle_key(&typed(' '));
        assert!(dialog.restrict);
        assert_eq!(
            dialog.query().restrict,
            vec![VfsPath::local("/home/thorin/dev/src")]
        );
    }

    #[test]
    fn the_root_list_appends_and_deletes() {
        // the controls 3, 4 and 5: `>>` appends, `Delete` removes, and the
        // line counts the others.
        let mut dialog = dialog();
        assert_eq!(dialog.roots_line(), fold_home("/home/thorin/dev"));

        focus(&mut dialog, Control::RootAdd);
        dialog.handle_key(&key(KeyCode::Enter));
        assert_eq!(dialog.roots.len(), 2);
        assert_eq!(dialog.focused(), Control::Root, "type the new one over it");
        dialog.root.set_text("/srv/media");
        dialog.sync_root_entry();
        assert_eq!(
            dialog.query().roots,
            vec![
                VfsPath::local("/home/thorin/dev"),
                VfsPath::local("/srv/media")
            ]
        );
        assert!(
            dialog.roots_line().ends_with("+1"),
            "{}",
            dialog.roots_line()
        );

        focus(&mut dialog, Control::RootList);
        dialog.handle_key(&key(KeyCode::Delete));
        assert_eq!(
            dialog.query().roots,
            vec![VfsPath::local("/home/thorin/dev")]
        );
        // The last root cannot go: a search needs somewhere to start.
        dialog.handle_key(&key(KeyCode::Delete));
        assert_eq!(dialog.roots.len(), 1);
        assert_eq!(dialog.error(), Some("a search needs somewhere to start"));
    }

    #[test]
    fn the_subdirectory_dropdown_steps_through_the_choices() {
        let mut dialog = dialog();
        focus(&mut dialog, Control::Depth);
        assert_eq!(dialog.depth(), Depth::Unlimited);
        dialog.handle_key(&key(KeyCode::Left));
        assert_eq!(dialog.depth(), Depth::None, "`none` is before `all`");
        dialog.handle_key(&key(KeyCode::Right));
        dialog.handle_key(&key(KeyCode::Right));
        assert_eq!(dialog.depth(), Depth::Levels(1));
        assert_eq!(dialog.query().depth, Depth::Levels(1));
    }

    #[test]
    fn an_unreadable_advanced_field_is_refused_on_the_tab_that_holds_it() {
        // refused in the dialog, with the reason, on that tab.
        let mut dialog = dialog();
        dialog.size_min.set_text("eight megabytes");
        focus(&mut dialog, Control::Start);
        assert!(matches!(
            dialog.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));
        assert_eq!(dialog.tab(), TabKind::Advanced, "the tab strip marks it");
        assert!(
            dialog
                .error()
                .is_some_and(|e| e.starts_with("Size at least")),
            "{:?}",
            dialog.error()
        );

        dialog.size_min.set_text("8MiB");
        dialog.size_max.set_text("1MiB");
        dialog.set_tab(TabKind::General);
        focus(&mut dialog, Control::Start);
        dialog.handle_key(&key(KeyCode::Enter));
        assert_eq!(dialog.tab(), TabKind::Advanced);
        assert!(
            dialog.error().is_some_and(|e| e.contains("smallest size")),
            "{:?}",
            dialog.error()
        );

        dialog.size_max.set_text("1GiB");
        assert_eq!(
            dialog.query().size,
            SizeRange {
                min: Some(8 * 1024 * 1024),
                max: Some(1024 * 1024 * 1024)
            }
        );
    }

    #[test]
    fn the_date_fields_are_enabled_by_the_mode_that_reads_them() {
        let mut dialog = dialog();
        assert!(!dialog.enabled(Control::After));
        assert!(!dialog.enabled(Control::Days));
        assert_eq!(dialog.query().date, DateRange::Any);

        // Typing into a disabled date field selects the mode that reads it -
        // the control the user last touched is the one that means it.
        focus(&mut dialog, Control::Days);
        dialog.days.set_text("");
        type_text(&mut dialog, "3");
        assert_eq!(dialog.date_choice, DateChoice::Newer);
        assert_eq!(dialog.query().date, DateRange::NewerThanDays(3));

        // Zero days is refused rather than silently meaning "any".
        dialog.days.set_text("0");
        focus(&mut dialog, Control::Start);
        dialog.handle_key(&key(KeyCode::Enter));
        assert_eq!(dialog.tab(), TabKind::Advanced);
        assert!(
            dialog.error().is_some_and(|e| e.contains("newer than")),
            "{:?}",
            dialog.error()
        );

        dialog.set_tab(TabKind::Advanced);
        focus(&mut dialog, Control::Modified);
        dialog.date_choice = DateChoice::Between;
        dialog.after.set_text("2026-02-30");
        focus(&mut dialog, Control::Start);
        dialog.handle_key(&key(KeyCode::Enter));
        assert!(
            dialog.error().is_some_and(|e| e.contains("not a date")),
            "{:?}",
            dialog.error()
        );
        dialog.after.set_text("2026-01-01");
        dialog.before.set_text("2025-01-01");
        focus(&mut dialog, Control::Start);
        dialog.handle_key(&key(KeyCode::Enter));
        assert_eq!(dialog.tab(), TabKind::Advanced);
        assert!(
            dialog.error().is_some_and(|e| e.contains("date range")),
            "{:?}",
            dialog.error()
        );
    }

    #[test]
    fn the_attributes_are_tri_state_and_default_to_ignore() {
        // "not hidden" is as much a filter as "hidden".
        let mut dialog = dialog();
        focus(&mut dialog, Control::AttrHidden);
        dialog.handle_key(&typed(' '));
        assert_eq!(dialog.attrs.hidden, Tri::Yes);
        dialog.handle_key(&typed(' '));
        assert_eq!(dialog.attrs.hidden, Tri::No);
        dialog.handle_key(&typed(' '));
        assert_eq!(dialog.attrs.hidden, Tri::Ignore, "Space cycles all three");
        assert_eq!(dialog.query().attrs.hidden, Tri::Ignore);
    }

    #[test]
    fn the_advanced_summary_says_what_is_filtered() {
        // the one addition beyond TC, and the reason it earns its place.
        let mut dialog = dialog();
        assert_eq!(dialog.advanced_summary(), "no advanced filters");
        dialog.size_min.set_text("1MiB");
        dialog.date_choice = DateChoice::Newer;
        dialog.days.set_text("7");
        dialog.attrs.hidden = Tri::No;
        assert_eq!(
            dialog.advanced_summary(),
            "size >= 1MiB, modified < 7 days, not hidden"
        );
    }

    #[test]
    fn alt_number_selects_a_tab_from_anywhere_in_the_dialog() {
        // `Alt+<n>` is a plain ESC-prefixed digit and reaches a legacy
        // terminal, which `Ctrl+Tab` does not.
        let mut dialog = dialog();
        focus(&mut dialog, Control::Name);
        let alt2 = DialogKey::raw(KeyPress::new(KeyCode::Char('2'), KeyModifiers::ALT));
        assert!(matches!(dialog.handle_key(&alt2), DialogOutcome::Consumed));
        assert_eq!(dialog.tab(), TabKind::Advanced);
        let alt3 = DialogKey::raw(KeyPress::new(KeyCode::Char('3'), KeyModifiers::ALT));
        dialog.handle_key(&alt3);
        assert_eq!(dialog.tab(), TabKind::LoadSave);
        assert_eq!(
            dialog.focused(),
            Control::SavedList,
            "a fresh tab starts on its first real control, not on the strip"
        );

        // And the strip itself moves on Left/Right when it has focus. The
        // dialog's own `focus`, not the helper: the helper looks the control up
        // across all three tabs and the strip is in all three, so it would
        // switch back to the General tab on the way.
        dialog.focus(Control::Tabs);
        dialog.handle_key(&key(KeyCode::Left));
        assert_eq!(dialog.tab(), TabKind::Advanced);
    }

    #[test]
    fn each_tab_keeps_its_own_focus() {
        let mut dialog = dialog();
        focus(&mut dialog, Control::Archives);
        dialog.set_tab(TabKind::Advanced);
        assert_eq!(dialog.focused(), Control::SizeMin, "its own first control");
        dialog.set_tab(TabKind::General);
        assert_eq!(dialog.focused(), Control::Archives);
    }

    #[test]
    fn the_load_save_tab_saves_loads_and_deletes() {
        // The write happens in the event loop, from the answer's
        // `saved` list.
        let mut dialog = dialog();
        dialog.name.set_text("*.rs");
        focus(&mut dialog, Control::SaveAs);
        dialog.set_tab(TabKind::LoadSave);
        focus(&mut dialog, Control::SaveAs);
        let outcome = dialog.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, DialogOutcome::Push(_)), "{outcome:?}");

        dialog.save_as("rust sources");
        assert_eq!(dialog.saved().len(), 1);
        assert_eq!(
            dialog.saved().first().map(|s| s.name.as_str()),
            Some("rust sources")
        );

        // Saving the same name again replaces rather than duplicates.
        dialog.name.set_text("*.toml");
        dialog.save_as("rust sources");
        assert_eq!(dialog.saved().len(), 1);

        // Load puts it back into the controls.
        dialog.name.set_text("*.md");
        focus(&mut dialog, Control::Load);
        dialog.handle_key(&key(KeyCode::Enter));
        assert_eq!(dialog.name.text(), "*.toml");
        assert_eq!(dialog.tab(), TabKind::General, "Load shows what it loaded");

        // The answer carries the list so the event loop can write it.
        focus(&mut dialog, Control::Start);
        dialog.set_tab(TabKind::General);
        focus(&mut dialog, Control::Start);
        match dialog.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Accept(DialogResult::Find(answer)) => {
                assert_eq!(answer.query.name, "*.toml");
                assert_eq!(answer.saved.map(|s| s.len()), Some(1));
            }
            other => panic!("{other:?}"),
        }

        // Delete empties it, and an unchanged list is not written at all.
        dialog.set_tab(TabKind::LoadSave);
        focus(&mut dialog, Control::Delete);
        dialog.handle_key(&key(KeyCode::Enter));
        assert!(dialog.saved().is_empty());
        assert!(
            !super::tests::dialog().saved_dirty,
            "an untouched list is not written"
        );
    }

    #[test]
    fn an_empty_saved_name_is_refused_rather_than_saved() {
        let mut dialog = dialog();
        dialog.save_as("   ");
        assert!(dialog.saved().is_empty());
        assert_eq!(dialog.error(), Some("a saved search needs a name"));
    }

    #[test]
    fn the_history_dropdown_offers_what_was_searched_for_before() {
        let mut state = state();
        state.history.names = vec!["*.rs".to_string(), "*.toml".to_string()];
        let mut dialog = FindDialog::new(VfsPath::local("/tmp"), Vec::new(), &state);
        focus(&mut dialog, Control::Name);
        let ctrl_down = DialogKey::raw(KeyPress::new(KeyCode::Down, KeyModifiers::CONTROL));
        dialog.handle_key(&ctrl_down);
        assert!(dialog.dropdown.is_some());
        dialog.handle_key(&key(KeyCode::Down));
        dialog.handle_key(&key(KeyCode::Enter));
        assert!(dialog.dropdown.is_none());
        assert_eq!(dialog.name.text(), "*.toml");

        // A field with no history says so rather than opening an empty box.
        focus(&mut dialog, Control::Text);
        dialog.handle_key(&ctrl_down);
        assert!(dialog.dropdown.is_none());
        assert_eq!(dialog.error(), Some("no history yet"));
    }

    #[test]
    fn reopening_offers_the_last_search_but_not_its_roots() {
        // "Search for" defaults to the last search's mask; "Search in"
        // defaults to the **active panel's** directory, which is the point.
        let mut state = state();
        let mut last = Query::new(VfsPath::local("/srv/media"));
        last.name = "*.log".to_string();
        last.content = Some(ContentQuery {
            pattern: "panic".to_string(),
            mode: TextMode::Plain,
            whole_words: true,
            case_sensitive: false,
            inverted: false,
            charsets: Charsets::DEFAULT,
        });
        state.last = Some(last);
        state.tab = TabKind::Advanced.index();

        let dialog = FindDialog::new(VfsPath::local("/home/thorin/dev"), Vec::new(), &state);
        assert_eq!(dialog.name.text(), "*.log");
        assert!(dialog.find_text);
        assert!(dialog.whole_words);
        assert_eq!(dialog.root.text(), "/home/thorin/dev");
        assert_eq!(
            dialog.query().roots,
            vec![VfsPath::local("/home/thorin/dev")]
        );
        assert_eq!(
            dialog.tab(),
            TabKind::Advanced,
            "and the tab it was left on"
        );
    }

    #[test]
    fn a_star_and_an_empty_mask_are_the_same_question() {
        // `Query::name` is documented as "empty means `*`", and the field
        // opens on `*` so it can be typed over.
        let mut dialog = dialog();
        assert_eq!(dialog.name.text(), "*");
        assert_eq!(dialog.query().name, "");
        dialog.name.set_text("  *.rs  ");
        assert_eq!(dialog.query().name, "*.rs");
    }

    #[test]
    fn esc_cancels_and_the_cancel_button_agrees() {
        let mut dialog = dialog();
        assert!(matches!(
            dialog.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
        focus(&mut dialog, Control::Cancel);
        assert!(matches!(
            dialog.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn the_devices_button_and_help_say_what_they_are() {
        // The device picker is the design, which the design puts in v0.7: the
        // button says which milestone brings it rather than doing nothing.
        let mut dialog = dialog();
        focus(&mut dialog, Control::Devices);
        match dialog.handle_key(&key(KeyCode::Enter)) {
            DialogOutcome::Push(pushed) => assert_eq!(pushed.id(), DialogId::Message),
            other => panic!("{other:?}"),
        }
        match dialog.handle_key(&key(KeyCode::F(1))) {
            DialogOutcome::Push(pushed) => assert_eq!(pushed.title(), "Find files"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tab_walks_every_control_and_wraps() {
        // It opens on the **mask**, not on the strip: control 0 of every tab is
        // the strip and nothing can be typed into it, while the design's
        // common case is "type the mask, press Enter". So the walk starts at
        // control 1 and the strip is what the wrap comes back through.
        let mut dialog = dialog();
        assert_eq!(dialog.focused(), Control::Name, "it opens on the mask");
        let mut seen = Vec::new();
        for _ in 0..GENERAL.len() {
            seen.push(dialog.focused());
            dialog.handle_key(&DialogKey::raw(KeyPress::plain(KeyCode::Tab)));
        }
        let mut expected: Vec<Control> = GENERAL.iter().skip(1).copied().collect();
        expected.push(Control::Tabs);
        assert_eq!(seen, expected);
        assert_eq!(dialog.focused(), Control::Name, "and it wraps");
    }

    fn draw(dialog: &FindDialog, w: u16, h: u16, ascii: bool) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, dialog, f.area(), &style);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map_or(" ", ratatui::buffer::Cell::symbol)
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn it_draws_inside_every_terminal_the_spec_promises() {
        // the design declares 60x15 usable; every dialog in this module is
        // tested at three sizes and in both glyph sets.
        let mut dialog = dialog();
        dialog.save_as("nightly");
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                for tab in [TabKind::General, TabKind::Advanced, TabKind::LoadSave] {
                    dialog.set_tab(tab);
                    let lines = draw(&dialog, w, h, ascii);
                    assert_eq!(lines.len(), usize::from(h));
                    for line in &lines {
                        assert_eq!(line.chars().count(), usize::from(w), "{line:?}");
                        if ascii {
                            assert!(line.is_ascii(), "{tab:?} at {w}x{h}: {line:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_general_tab_shows_every_group_at_eighty_columns() {
        let mut dialog = dialog();
        dialog.find_text = true;
        dialog.text.set_text("TODO");
        let screen = draw(&dialog, 80, 24, true).join("\n");
        for needle in [
            "General",
            "Advanced",
            "Load/Save",
            "Search for",
            "Search in",
            "[>>]",
            "Devices",
            "Only search in selected",
            "Search archives",
            "all (unlimited depth)",
            "[x] Find text",
            "Whole words only",
            "Case sensitive",
            "Hex",
            "Find files NOT containing the text",
            "UTF-8",
            "CP437 (DOS)",
            "Start search",
            "Cancel",
            "Help",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    #[test]
    fn the_dropdown_draws_over_the_rows_below_the_field() {
        let mut state = state();
        state.history.names = vec!["*.rs".to_string(), "*.toml".to_string()];
        let mut dialog = FindDialog::new(VfsPath::local("/tmp"), Vec::new(), &state);
        focus(&mut dialog, Control::Name);
        dialog.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Down,
            KeyModifiers::CONTROL,
        )));
        let screen = draw(&dialog, 80, 24, true).join("\n");
        assert!(screen.contains("*.toml"), "{screen}");
        assert!(dialog.cursor(Rect::new(0, 0, 80, 24)).is_none());
    }
    // ------------------------------------------------- the design -------

    /// `Alt+<letter>`, in the `CSI u` shape the terminal layer normalises to.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    /// Every character the dialog draws underlined, in reading order.
    ///
    /// Reads the rendered buffer rather than the tables, so a mnemonic that is
    /// declared but never painted fails the test the design asks for.
    fn underlined(dialog: &FindDialog, w: u16, h: u16) -> Vec<char> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, dialog, f.area(), &style);
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

    /// Every underlined cell, paired with the character drawn to its left.
    ///
    /// Which is what tells the two `x`s of `[x] Hex` apart: the label's is
    /// preceded by the `e`, the tick mark's by the `[`. [`underlined`] cannot,
    /// because the character is `x` either way.
    fn underlined_after(dialog: &FindDialog, w: u16, h: u16) -> Vec<(char, char)> {
        let buffer = painted(dialog, w, h);
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

    /// The rendered screen, one `String` per row.
    fn screen(dialog: &FindDialog, w: u16, h: u16) -> Vec<String> {
        let buffer = painted(dialog, w, h);
        (0..h)
            .map(|y| {
                (0..w)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect()
            })
            .collect()
    }

    /// One render of `dialog` into a `w` by `h` terminal.
    fn painted(dialog: &FindDialog, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, dialog, f.area(), &style);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn ticking_the_hex_box_leaves_its_underline_on_its_own_label() {
        // "The letter is shown underlined in that control's
        // label." A ticked box is drawn `[x] Hex` and the tick is the literal
        // character `x` at a word start, so the underline used to move off the
        // label and onto the mark for exactly as long as the box stayed
        // ticked. Read off the buffer, because the set of underlined
        // *characters* is `x` either way and cannot tell the two apart.
        //
        let mut dialog = dialog();
        dialog.find_text = true;
        let unticked = underlined_after(&dialog, 100, 24);
        assert!(unticked.contains(&('e', 'x')), "the `x` of `Hex`");

        dialog.handle_key(&alt('x'));
        assert!(dialog.hex, "Alt+X ticked it");
        let ticked = underlined_after(&dialog, 100, 24);
        assert!(
            ticked.contains(&('e', 'x')) && !ticked.contains(&('[', 'x')),
            "still the `x` of `Hex`, never the tick mark: {ticked:?}"
        );
    }

    #[test]
    fn at_sixty_columns_a_mnemonic_never_ticks_a_box_that_is_not_drawn() {
        // the design promises 60x15 is usable. The dialog gets 56 columns
        // there and 54 inside its border, and the four
        // content-search boxes want 60 on their row and the four charsets 66
        // on theirs - so `Hex` and `CP437 (DOS)` used to be dropped off the
        // right-hand end while `Alt+X` and `Alt+P` still ticked them. The
        // search then ran in hex, or over a charset nobody chose, with
        // nothing on the screen to say so and no underline to read the letter
        // off. Every accelerator focuses its control before it
        // changes it, so the row now slides far enough right to show it.
        let mut dialog = dialog();
        dialog.find_text = true;

        dialog.handle_key(&alt('x'));
        assert!(dialog.hex, "Alt+X ticked Hex");
        let rows = screen(&dialog, 60, 15);
        assert!(
            rows.iter().any(|row| row.contains("[x] Hex")),
            "the box it ticked is on the screen: {rows:#?}"
        );
        assert!(
            underlined_after(&dialog, 60, 15).contains(&('e', 'x')),
            "with its letter underlined"
        );

        dialog.handle_key(&alt('v'));
        assert!(dialog.archives, "Alt+V ticked Search archives");
        let rows = screen(&dialog, 60, 15);
        assert!(
            rows.iter().any(|row| row.contains("[x] Search archives")),
            "and so is the box that shares its row with a 44-column label: {rows:#?}"
        );

        dialog.handle_key(&alt('p'));
        assert!(dialog.charsets.cp437, "Alt+P ticked CP437");
        let rows = screen(&dialog, 60, 15);
        assert!(
            rows.iter().any(|row| row.contains("[x] CP437 (DOS)")),
            "and so is that one: {rows:#?}"
        );
        assert!(
            underlined_after(&dialog, 60, 15).contains(&('C', 'P')),
            "with its letter underlined"
        );
    }

    #[test]
    fn alt_t_ticks_find_text_on_its_way_to_the_field() {
        // quoted whole because it is the one accelerator the
        // section names twice: "the content-search options are dead until the
        // Find text checkbox is ticked, so `Alt+T` ticks it *and* puts the
        // caret in the field, which is the one thing the keystroke could
        // sensibly mean." the design calls it "the accelerator that makes
        // the dialog worth using from the keyboard".
        let mut dialog = dialog();
        assert_eq!(dialog.focused(), Control::Name);
        assert!(!dialog.find_text);

        assert!(matches!(
            dialog.handle_key(&alt('t')),
            DialogOutcome::Consumed
        ));
        assert!(dialog.find_text, "the box is ticked");
        assert_eq!(dialog.focused(), Control::Text, "and the caret is in it");
        assert!(
            dialog.enabled(Control::WholeWords),
            "which is what un-greys the options below it"
        );

        // "An accelerator never turns anything off ... a key that enabled on
        // the way in and disabled on the way back would make a repeated
        // keystroke destructive."
        focus(&mut dialog, Control::Name);
        assert!(matches!(
            dialog.handle_key(&alt('t')),
            DialogOutcome::Consumed
        ));
        assert!(dialog.find_text, "still ticked the second time");
        assert_eq!(dialog.focused(), Control::Text);
    }

    #[test]
    fn a_mnemonic_reaches_every_control_that_has_one() {
        // the closing paragraph: "Every control above carries an
        // `Alt` mnemonic, underlined in its label." Reaching the
        // Find text field took ten `Tab` presses before this existed.
        for tab in [TabKind::General, TabKind::Advanced, TabKind::LoadSave] {
            for (control, letter) in tab.mnemonics() {
                let mut dialog = marked_dialog();
                dialog.set_tab(tab);
                let outcome = dialog.handle_key(&alt(*letter));
                assert!(
                    !matches!(outcome, DialogOutcome::Ignored),
                    "{tab:?} Alt+{letter} ({control:?}) was ignored"
                );
                if FindDialog::is_button(*control) {
                    // "On a button, it presses it" - which for `Cancel` closes
                    // the dialog and for `Start search` accepts it, so there is
                    // no focus left to look at.
                    continue;
                }
                // `Alt+T` lands on the field the checkbox gates, which is
                // the own exception and the only one.
                let want = if *control == Control::FindText {
                    Control::Text
                } else {
                    *control
                };
                assert_eq!(dialog.focused(), want, "{tab:?} Alt+{letter}");
            }
        }
    }

    #[test]
    fn a_mnemonic_never_turns_a_checkbox_off() {
        // "An accelerator never turns anything off." `Space` is
        // the key that means "the other one of the two"; `Alt+<letter>` means
        // "the option I just named", so pressing it twice is not a toggle.
        let mut dialog = dialog();
        dialog.handle_key(&alt('t'));
        let read = |d: &FindDialog, letter: char| match letter {
            'w' => d.whole_words,
            'c' => d.case_sensitive,
            'o' => d.inverted,
            // `UTF-16` has no mnemonic left, so `UTF-8` stands
            // for the charset checkboxes here.
            'u' => d.charsets.utf8,
            _ => panic!("unlisted letter {letter}"),
        };
        for letter in ['w', 'c', 'o', 'u'] {
            dialog.handle_key(&alt(letter));
            assert!(read(&dialog, letter), "Alt+{letter} ticked it");
            dialog.handle_key(&alt(letter));
            assert!(read(&dialog, letter), "Alt+{letter} again left it ticked");
        }
    }

    #[test]
    fn mnemonics_are_unique_within_a_dialog() {
        // "Mnemonics are **unique within a dialog**. A duplicate
        // is a bug rather than a first-one-wins rule, because the second
        // control becomes unreachable silently; it is caught by a test rather
        // than by inspection."
        //
        // Per tab, because that is what a user can reach without pressing
        // `Alt+<n>` first, and because the dialog's thirty-nine controls
        // cannot have thirty-nine distinct letters. The three buttons carry
        // the same letter on all three tabs on purpose: one control, shown
        // three times.
        for tab in [TabKind::General, TabKind::Advanced, TabKind::LoadSave] {
            let mut seen: Vec<char> = Vec::new();
            for (control, letter) in tab.mnemonics() {
                assert!(
                    letter.is_ascii_lowercase(),
                    "{tab:?} {control:?}: mnemonics are stored folded"
                );
                assert!(
                    !seen.contains(letter),
                    "{tab:?} {control:?}: Alt+{letter} is already taken"
                );
                seen.push(*letter);
                assert!(
                    tab.order().contains(control),
                    "{tab:?} {control:?} is not on this tab"
                );
            }
        }
        // And no mnemonic is a digit: the design reserves `Alt`+digit for the
        // tab strip "so mnemonics are letters and the two never collide".
        assert_eq!(
            DialogKey::raw(KeyPress::new(KeyCode::Char('2'), KeyModifiers::ALT)).mnemonic(),
            None
        );
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        // "The letter is shown underlined in that control's
        // label, so the whole set is readable off the screen rather than
        // memorised." Read off the rendered buffer, so a table entry with no
        // paint behind it fails here.
        for tab in [TabKind::General, TabKind::Advanced, TabKind::LoadSave] {
            let mut dialog = marked_dialog();
            dialog.set_tab(tab);
            // Ticked, so the gated half of the General tab is drawn live and
            // its labels are on the screen either way.
            dialog.find_text = true;
            let painted = underlined(&dialog, 100, 24);
            let mut want: Vec<char> = tab.mnemonics().iter().map(|(_, c)| *c).collect();
            let mut got: Vec<char> = painted.iter().map(|c| c.to_ascii_lowercase()).collect();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "{tab:?}: underlines on screen");
        }
    }

    #[test]
    fn the_root_add_button_is_the_one_control_spec_leaves_without_a_mnemonic() {
        // the design labels that button `>>` and the design requires the
        // mnemonic to be "shown underlined in that control's label". A label
        // with no letters in it cannot show one, and counting it the General
        // tab's twenty-two controls offer only twenty-one distinct letters
        // between them - so this is recorded here rather than guessed at.
        // `Tab` still reaches it, from the field it appends.
        let declared: Vec<Control> = fields::GENERAL_MNEMONICS.iter().map(|(c, _)| *c).collect();
        let missing: Vec<Control> = GENERAL
            .iter()
            .copied()
            .filter(|c| !declared.contains(c))
            .collect();
        assert_eq!(
            missing,
            vec![
                Control::Tabs,
                Control::RootAdd,
                Control::Text,
                Control::Utf16,
            ],
            "the strip is `Alt`+digit, the text field shares `Alt+T`, and \
             `UTF-16` has no letter left: U is UTF-8's, T is Find text's, F is \
             `Search for`, and the rest of it is digits"
        );
        let at = GENERAL
            .iter()
            .position(|c| *c == Control::RootAdd)
            .expect("the `>>` button is in the ring");
        assert_eq!(GENERAL.get(at.saturating_sub(1)), Some(&Control::Root));
    }

    #[test]
    fn a_dialog_mnemonic_is_alt_and_a_letter_and_nothing_else() {
        // "`Alt` with a *letter*, specifically, because the design
        // 3.1's floor is a terminal that sends `Alt+X` as a plain
        // `ESC`-prefixed byte." Both encodings arrive here as the same
        // `KeyPress`, so this is about the modifiers.
        assert_eq!(alt('T').mnemonic(), Some('t'), "folded to lower case");
        assert_eq!(
            DialogKey::raw(KeyPress::new(
                KeyCode::Char('T'),
                KeyModifiers::ALT | KeyModifiers::SHIFT
            ))
            .mnemonic(),
            Some('t'),
            "a terminal that reports the shift is describing the same key"
        );
        assert_eq!(
            DialogKey::raw(KeyPress::new(
                KeyCode::Char('t'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            ))
            .mnemonic(),
            None,
            "Ctrl+Alt+T is a different binding, not a sloppier spelling"
        );
        assert_eq!(typed('t').mnemonic(), None, "a bare letter is text");
        assert_eq!(
            DialogKey::raw(KeyPress::new(KeyCode::F(7), KeyModifiers::ALT)).mnemonic(),
            None
        );
    }

    #[test]
    fn alt_still_selects_a_tab_and_the_letters_do_not_collide_with_it() {
        // "`Alt` with a *digit* is already the tab strip, so
        // mnemonics are letters and the two never collide."
        let mut dialog = dialog();
        dialog.handle_key(&DialogKey::raw(KeyPress::new(
            KeyCode::Char('2'),
            KeyModifiers::ALT,
        )));
        assert_eq!(dialog.tab(), TabKind::Advanced);
        // And the Advanced tab's own letters work from there.
        assert!(matches!(
            dialog.handle_key(&alt('m')),
            DialogOutcome::Consumed
        ));
        assert_eq!(dialog.focused(), Control::Modified);
    }

    #[test]
    fn every_letter_names_a_control_the_open_tab_can_reach() {
        // the table, checked against the dialog
        // rather than against a second copy of itself: every letter resolves
        // back to the control it was declared for, that control is in the open
        // tab's focus ring, and none of them is `Accel::Absent` - this dialog
        // never claims a letter for something the user cannot see, because a
        // letter is only ever looked up in the open tab's own table.
        for tab in [TabKind::General, TabKind::Advanced, TabKind::LoadSave] {
            let mut dialog = marked_dialog();
            dialog.set_tab(tab);
            let table = dialog.mnemonics();
            assert_eq!(table, tab.mnemonics(), "{tab:?}: the open tab's table");
            for (control, letter) in table {
                assert_eq!(
                    dialog.mnemonic_of(*control),
                    Some(*letter),
                    "{tab:?} {control:?}: the underline and the key agree"
                );
                assert!(
                    tab.order().contains(control),
                    "{tab:?} {control:?} is not in this tab's ring"
                );
                assert!(
                    !matches!(dialog.accel(*control), Accel::Absent),
                    "{tab:?} {control:?} is on the screen, so it is not absent"
                );
            }
            // The census's own answer, which is what the framework's
            // uniqueness test reads: this
            // tab's letters, all of them, each of them once.
            let letters = Dialog::mnemonic_letters(&dialog);
            let mut seen: Vec<char> = Vec::new();
            for letter in &letters {
                assert!(
                    !seen.contains(letter),
                    "{tab:?}: Alt+{letter} is declared twice"
                );
                seen.push(*letter);
            }
            let mut want: Vec<char> = table.iter().map(|(_, c)| *c).collect();
            let mut got = letters.clone();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "{tab:?}: the census sees this tab's letters");
        }
    }

    #[test]
    fn a_mnemonic_presses_the_button_it_names() {
        // "a button: focus it and press it", and
        // the press is the button's own answer, not a special case. The
        // reachability test above stops at the buttons because pressing one
        // closes or pushes; this is where each of them is pressed by name.
        // `Alt+S` is Start search, which is what Total Commander underlines
        // and what the reference screenshots show.
        let start = marked_dialog().handle_key(&alt('s'));
        assert!(matches!(start, DialogOutcome::Accept(_)), "{start:?}");
        let cancel = marked_dialog().handle_key(&alt('n'));
        assert!(matches!(cancel, DialogOutcome::Cancel), "{cancel:?}");
        let help = marked_dialog().handle_key(&alt('h'));
        assert!(matches!(help, DialogOutcome::Push(_)), "{help:?}");
        let devices = marked_dialog().handle_key(&alt('d'));
        assert!(matches!(devices, DialogOutcome::Push(_)), "{devices:?}");

        // `>>` has no letter (the design labels it `>>`), so it is pressed
        // through the ring; the letter that does reach a button on the
        // Load/Save tab is `Alt+V`, which asks for the name to save under.
        let mut dialog = marked_dialog();
        dialog.set_tab(TabKind::LoadSave);
        let save = dialog.handle_key(&alt('v'));
        assert!(matches!(save, DialogOutcome::Push(_)), "{save:?}");
        assert_eq!(dialog.focused(), Control::SaveAs, "focus moved first");
    }

    #[test]
    fn a_mnemonic_is_read_before_a_rebound_global_action() {
        // `Keymap::resolve` falls back to
        // the `[global]` table while a dialog is open, so a user who bound
        // `alt+x` to `clear` has `DialogKey::action` set on the very key this
        // dialog spends on `Hex`. the design says the dialog's mnemonic wins.
        let mut dialog = dialog();
        dialog.handle_key(&alt('t'));
        let rebound = DialogKey {
            press: KeyPress::new(KeyCode::Char('x'), KeyModifiers::ALT),
            action: Some(crate::input::Action::Clear),
        };
        let outcome = dialog.handle_key(&rebound);
        assert!(matches!(outcome, DialogOutcome::Consumed), "{outcome:?}");
        assert!(dialog.hex, "Alt+X is Hex in this dialog");
        assert_eq!(dialog.focused(), Control::Hex);
    }

    #[test]
    fn a_mnemonic_never_types_and_never_edits() {
        // the design I8. `DialogKey::text` is `None` whenever
        // `Alt` is held, so a letter that jumps to a control cannot also be
        // typed into the field it jumped from - checked here rather than
        // assumed, because it is the difference between an accelerator and a
        // keystroke that quietly corrupts a search mask.
        let mut dialog = dialog();
        dialog.name.set_text("*.rs");
        dialog.name.select(0, 0);
        let before = dialog.name.text().to_string();
        let caret = dialog.name.caret();
        for letter in ['w', 'c', 'x', 'q', 'j'] {
            focus(&mut dialog, Control::Name);
            dialog.handle_key(&alt(letter));
            assert_eq!(dialog.name.text(), before, "Alt+{letter} typed nothing");
            assert_eq!(dialog.name.caret(), caret, "Alt+{letter} moved no caret");
        }
    }
}
