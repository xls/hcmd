//! Every control the Find Files dialog has, and what kind of control it is.
//!
//!
//! The dialog's three tabs are three lists of [`Control`]s, and the order of
//! each list is the order `Tab` walks. Declaring them as tables rather than as
//! branches is what makes the ring and the mnemonics checkable: a control that
//! is in a tab's list and in no mnemonic table, or in two, is a bug a test can
//! find without pressing a key.
//!
//! The three enums beside them name the three shapes a control can take that
//! are not a plain text field: a [`Gate`] is on or off, a [`Stepper`] moves
//! through a fixed list, and a [`DateChoice`] is one of several ways of saying
//! when. A control's shape decides what its keys mean, so the shape is data
//! here rather than a match arm there.

/// One control of the dialog.
///
/// An enum rather than a bare index so that every `match` over the focused
/// control is exhaustive: the three tabs have three different focus orders and
/// a number would silently mean a different control on each of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The tab strip. Index 0 on every tab.
    Tabs,
    /// "Search for": the name mask.
    Name,
    /// The `RegEx` checkbox beside it.
    NameRegex,
    /// "Search in": the start path.
    Root,
    /// `>>`: append the field to the root list.
    RootAdd,
    /// The root list.
    RootList,
    /// "Devices": the picker.
    Devices,
    /// "Only search in selected directories/files".
    Restrict,
    /// "Search archives".
    Archives,
    /// "Search in subdirectories".
    Depth,
    /// "Find text": the checkbox that gates everything below it.
    FindText,
    /// The text pattern.
    Text,
    /// `Whole words only`.
    WholeWords,
    /// `Case sensitive`.
    CaseSensitive,
    /// `RegEx`, on the text pattern.
    TextRegex,
    /// `Hex`: a byte sequence, the viewer's own syntax.
    Hex,
    /// `Find files NOT containing the text`.
    Inverted,
    /// The `UTF-8` charset.
    Utf8,
    /// The `UTF-16` charset.
    Utf16,
    /// The `Latin-1 / windows-1252` charset.
    Latin1,
    /// The `CP437 (DOS)` charset.
    Cp437,
    /// `Size at least`.
    SizeMin,
    /// `Size at most`.
    SizeMax,
    /// The `Modified` three-way radio.
    Modified,
    /// The `after` date.
    After,
    /// The `before` date.
    Before,
    /// The `newer than N days` count.
    Days,
    /// The `Directories` tri-state.
    AttrDirectories,
    /// The `Hidden` tri-state.
    AttrHidden,
    /// The `Executable` tri-state.
    AttrExecutable,
    /// The `Symlinks` tri-state.
    AttrSymlinks,
    /// The `Read-only` tri-state.
    AttrReadOnly,
    /// The list of saved searches.
    SavedList,
    /// `Load`.
    Load,
    /// `Save as…`.
    SaveAs,
    /// `Delete`.
    Delete,
    /// `Start search`, the default button.
    Start,
    /// `Cancel`.
    Cancel,
    /// `Help`.
    Help,
}

/// The General tab's focus order.
///
/// The indices are the contract's: 0 is the tab strip, 10 is `Find text` and
/// 21 is `Start search`.
pub(super) const GENERAL: &[Control] = &[
    Control::Tabs,
    Control::Name,
    Control::NameRegex,
    Control::Root,
    Control::RootAdd,
    Control::RootList,
    Control::Devices,
    Control::Restrict,
    Control::Archives,
    Control::Depth,
    Control::FindText,
    Control::Text,
    Control::WholeWords,
    Control::CaseSensitive,
    Control::TextRegex,
    Control::Hex,
    Control::Inverted,
    Control::Utf8,
    Control::Utf16,
    Control::Latin1,
    Control::Cp437,
    Control::Start,
    Control::Cancel,
    Control::Help,
];

/// The Advanced tab's focus order.
pub(super) const ADVANCED: &[Control] = &[
    Control::Tabs,
    Control::SizeMin,
    Control::SizeMax,
    Control::Modified,
    Control::After,
    Control::Before,
    Control::Days,
    Control::AttrDirectories,
    Control::AttrHidden,
    Control::AttrExecutable,
    Control::AttrSymlinks,
    Control::AttrReadOnly,
    Control::Start,
    Control::Cancel,
    Control::Help,
];

/// The Load/Save tab's focus order.
pub(super) const LOAD_SAVE: &[Control] = &[
    Control::Tabs,
    Control::SavedList,
    Control::Load,
    Control::SaveAs,
    Control::Delete,
    Control::Start,
    Control::Cancel,
    Control::Help,
];

/// The General tab's `Alt` mnemonics (the last paragraph).
///
/// > Every control above carries an `Alt` mnemonic, underlined in its
/// > label. `Alt+T` is **Find text** and ticks the checkbox on its way to the
/// > field.
///
/// `S` and `T` are the own two examples and are fixed by it; the
/// rest are the first letter of the label wherever the letter was still free,
/// and the most readable remaining letter of that same label where it was not.
/// Every one of them appears in the label the dialog actually draws, which the
/// module's `every_mnemonic_is_underlined_in_its_own_label` test checks against
/// the rendered buffer rather than against this table.
///
/// [`Control::Text`] is deliberately absent: it shares `Alt+T` with the
/// checkbox that gates it, which is the "ticks it *and* puts the
/// caret in the field".
///
/// [`Control::RootAdd`] is absent too, and that one is a **gap in the design**
/// rather than a decision. the design gives that button the label `>>` and
/// the design requires the mnemonic to be "shown underlined in that
/// control's label" - a label with no letters in it cannot show one. Counting
/// it, the General tab's twenty-two controls between them offer only
/// twenty-one distinct letters, so uniqueness and completeness cannot both
/// hold. `Tab` still reaches it, from **Search in**, which is the field it
/// appends.
pub(super) const GENERAL_MNEMONICS: &[(Control, char)] = &[
    // Total Commander underlines the F in "Search for" and the S in
    // "Start search", and the button is the one people reach for. Reported
    // from a real session against the reference screenshots.
    (Control::Name, 'f'),
    (Control::NameRegex, 'e'),
    (Control::Root, 'i'),
    (Control::RootList, 'r'),
    (Control::Devices, 'd'),
    (Control::Restrict, 'y'),
    (Control::Archives, 'v'),
    (Control::Depth, 'b'),
    (Control::FindText, 't'),
    (Control::WholeWords, 'w'),
    (Control::CaseSensitive, 'c'),
    (Control::TextRegex, 'g'),
    (Control::Hex, 'x'),
    (Control::Inverted, 'o'),
    (Control::Utf8, 'u'),
    // No mnemonic: "UTF-16" holds only U, T, F and two digits. U is UTF-8's,
    // T is Find text's, F is now "Search for", and Alt+<digit> is the tab
    // strip. the design says a control with no letter to underline gets none
    // and is reached by Tab, which is this one.
    (Control::Latin1, 'l'),
    (Control::Cp437, 'p'),
    (Control::Start, 's'),
    (Control::Cancel, 'n'),
    (Control::Help, 'h'),
];

/// The Advanced tab's `Alt` mnemonics.
///
/// `Alt+A`, `Alt+N` and `Alt+H` are the three buttons and mean the same thing
/// on every tab: they are one control each, shown three times, and a button
/// that moved under the user when they changed tab would be worse than no
/// accelerator at all.
pub(super) const ADVANCED_MNEMONICS: &[(Control, char)] = &[
    (Control::SizeMin, 's'),
    (Control::SizeMax, 'z'),
    (Control::Modified, 'm'),
    (Control::After, 'f'),
    (Control::Before, 'b'),
    (Control::Days, 'y'),
    (Control::AttrDirectories, 'd'),
    (Control::AttrHidden, 'i'),
    (Control::AttrExecutable, 'e'),
    (Control::AttrSymlinks, 'l'),
    (Control::AttrReadOnly, 'r'),
    (Control::Start, 'a'),
    (Control::Cancel, 'n'),
    (Control::Help, 'h'),
];

/// The Load/Save tab's `Alt` mnemonics.
pub(super) const LOAD_SAVE_MNEMONICS: &[(Control, char)] = &[
    (Control::SavedList, 's'),
    (Control::Load, 'l'),
    (Control::SaveAs, 'v'),
    (Control::Delete, 'd'),
    (Control::Start, 'a'),
    (Control::Cancel, 'n'),
    (Control::Help, 'h'),
];

/// Which tab is open.
///
/// The strip's index is a `usize` because [`TabStrip`] is generic over its
/// titles; this is the one place that number is turned back into a meaning, so
/// nothing else in the dialog compares tabs by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabKind {
    /// the General tab.
    #[default]
    General,
    /// the Advanced tab.
    Advanced,
    /// the Load/Save tab.
    LoadSave,
}

/// Which of the dialog's eight text fields a control edits.
///
/// [`FindDialog::field_id`] is the **one** exhaustive match over [`Control`] on
/// that question, and everything that needs a field - the key handler, the
/// renderer, the cursor and the history dropdown - goes through it. A control
/// added later is then a compile error in one place rather than a field that
/// silently cannot be typed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    /// "Search for".
    Name,
    /// "Search in".
    Root,
    /// The "Find text" pattern.
    Text,
    /// `Size at least`.
    SizeMin,
    /// `Size at most`.
    SizeMax,
    /// The `after` date.
    After,
    /// The `before` date.
    Before,
    /// The `newer than N days` count.
    Days,
}

/// Why a control is drawn greyed and refuses to be ticked.
///
/// [`FindDialog::gate`] is the one exhaustive match over [`Control`] on that
/// question, and both the rendering and the refusal message come from it - so
/// a control that looks greyed cannot be one that quietly answers, and a
/// control that refuses cannot be one with nothing to say about why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// It answers keys.
    Live,
    /// One of the options the design greys out until "Find text" is ticked.
    FindText,
    /// "Only search in selected directories/files", with nothing marked.
    Marks,
    /// A date field the `Modified` radio does not read.
    DateMode,
    /// `Load` or `Delete` with nothing saved.
    SavedList,
}

/// What the arrow keys drive, when they drive a control rather than the focus
/// ring (the dropdown, radio and two lists).
///
/// [`FindDialog::stepper`] is the one exhaustive match over [`Control`] on that
/// question. Everything not named here moves the focus, which is what makes a
/// twenty-four-control form walkable with the arrows alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stepper {
    /// The arrows move the focus ring.
    None,
    /// "Search in subdirectories".
    Depth,
    /// The `Modified` radio.
    Date,
    /// The root list.
    Roots,
    /// The saved-search list.
    Saved,
}

/// The `Modified` radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateChoice {
    /// No date filter. The default, so an Advanced tab nobody has opened
    /// cannot change a result.
    #[default]
    Any,
    /// An absolute range, either end optional.
    Between,
    /// "newer than N days".
    Newer,
}

impl TabKind {
    /// The three titles, in the order the design names them.
    pub const TITLES: [&'static str; 3] = ["General", "Advanced", "Load/Save"];

    /// The tab at `index`, or [`TabKind::General`] for anything else: an index
    /// out of range is a tab that does not exist, and the first tab is the only
    /// answer that cannot hide a control.
    pub const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Advanced,
            2 => Self::LoadSave,
            _ => Self::General,
        }
    }

    /// Its index in the strip.
    pub const fn index(self) -> usize {
        match self {
            Self::General => 0,
            Self::Advanced => 1,
            Self::LoadSave => 2,
        }
    }

    /// Its focus order.
    pub const fn order(self) -> &'static [Control] {
        match self {
            Self::General => GENERAL,
            Self::Advanced => ADVANCED,
            Self::LoadSave => LOAD_SAVE,
        }
    }

    /// Its `Alt` mnemonics.
    ///
    /// Per tab, because only one tab's controls are on the screen and only its
    /// letters can be read off it. the uniqueness rule is enforced
    /// over each of these tables, which is the whole of what a user can reach
    /// without pressing `Alt+<n>` first.
    pub const fn mnemonics(self) -> &'static [(Control, char)] {
        match self {
            Self::General => GENERAL_MNEMONICS,
            Self::Advanced => ADVANCED_MNEMONICS,
            Self::LoadSave => LOAD_SAVE_MNEMONICS,
        }
    }
}

impl Gate {
    /// Why the control refused, or `None` when it did not.
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Live => None,
            Self::FindText => Some("tick Find text to search file contents"),
            Self::Marks => Some("nothing is marked in the panel"),
            Self::DateMode => Some("choose the Modified mode that reads it"),
            Self::SavedList => Some("no saved search yet"),
        }
    }
}

impl Stepper {
    /// Whether `Up`/`Down` drive it as well as `Left`/`Right`.
    ///
    /// A list has rows, so both axes walk it; a dropdown and a radio are one
    /// line, so `Up`/`Down` there belong to the form.
    pub const fn vertical(self) -> bool {
        matches!(self, Self::Saved)
    }
}

impl DateChoice {
    /// The three choices, in order.
    pub const ALL: [Self; 3] = [Self::Any, Self::Between, Self::Newer];

    /// The radio's label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Between => "between",
            Self::Newer => "newer than",
        }
    }
}
