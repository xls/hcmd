//! The viewer's cursor selection.
//!
//! **One cursor and one selection, both byte ranges in the file.** In hex mode
//! the two sides are two views of them, not two of each, so `Tab` moves focus
//! and nothing else changes: an extension started on one side is continued on
//! the other. That is the whole reason `Tab` is a focus switch rather than two
//! independent cursors.
//!
//! # What lives here
//!
//! The one place that decides what a selection *is*. The model and the range
//! algebra, and nothing else: no reading, no window, no terminal, no knowledge
//! of where a row begins. The viewer walks the file and hands the two ends
//! here through [`apply_extend`]; the renderer asks [`Selection::covers_row`]
//! what a row carries.
//!
//! That division is what keeps the rule true of a selection as well
//! as of a layout: **a selection spanning 100 MB is four numbers.** Nothing in
//! this module can read a file, so nothing in this module can be the thing that
//! reads one.
//!
//! # Why an anchor and a head rather than a normalised range
//!
//! Because shrinking has to work. `Shift+Right` five times and then
//! `Shift+Left` seven has to end up selecting two bytes *before* where it
//! started, not nothing: a normalised `(lo, hi)` loses which end the cursor is
//! on the moment the two cross. [`Selection::range`] normalises on demand
//! instead, which costs a comparison and keeps the crossing honest.
//!
//! # Why two columns as well as two offsets
//!
//! A rectangular selection (the "a column block, which is how you
//! take one field out of aligned output") is the intersection of a row range
//! with a column band, and an offset alone does not say which column it is in:
//! in text mode the column is a *display* column of the expanded row, so tabs
//! count as the stops they draw as. The columns are maintained for a linear
//! selection too, so that `Ctrl+Shift` after `Shift` can turn one into a block
//! without the anchor's column having been thrown away in between.
//!

use crate::config::HexConfig;

/// Which half of a hex row the cursor is working in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HexSide {
    /// The hex columns. One press moves one column - a byte at `group = 8`, a
    /// whole word above it.
    #[default]
    Bytes,
    /// The character gutter. One press moves one character, which under a
    /// multi-byte encoding may be several bytes.
    Chars,
}

impl HexSide {
    /// `Tab`.
    pub const fn other(self) -> Self {
        match self {
            Self::Bytes => Self::Chars,
            Self::Chars => Self::Bytes,
        }
    }

    /// For the status line.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Chars => "chars",
        }
    }
}

/// How a selection grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    /// Whole lines wrap round, as in any editor.
    Linear,
    /// A column block - how you take one field out of aligned output.
    Rectangular,
}

/// What a key press does to the selection (the key table).
///
/// The key that produced it is the keymap's business;
/// what it *means* is this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extend {
    /// A bare movement. **Clears the selection.**
    ///
    /// the design is silent and every editor does this; a selection that survived
    /// an unshifted arrow would make the next `Ctrl+C` copy something the user
    /// thought they had let go of.
    None,
    /// `Shift` + a movement.
    Linear,
    /// `Ctrl+Shift` + a movement.
    Rectangular,
}

impl Extend {
    /// The kind a live selection takes on when this extension is applied, or
    /// `None` for a bare movement.
    pub const fn kind(self) -> Option<SelectKind> {
        match self {
            Self::None => None,
            Self::Linear => Some(SelectKind::Linear),
            Self::Rectangular => Some(SelectKind::Rectangular),
        }
    }
}

/// One movement, independent of the key that produced it.
///
/// Named for what it does to the **cursor**, not for the key: `RowStart` is
/// `Home`, and what a "row" is depends on wrap, which is the viewer's business
/// and not the keymap's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// One screen row up.
    Up,
    /// One screen row down.
    Down,
    /// One character back in text, one unit of the focused side in hex.
    Left,
    /// One character on in text, one unit of the focused side in hex.
    Right,
    /// One screenful less a row, up.
    PageUp,
    /// One screenful less a row, down.
    PageDown,
    /// The first character of the cursor's screen row (`Home`).
    RowStart,
    /// The last character of the cursor's screen row (`End`).
    RowEnd,
    /// The first byte of the file, by seek (`Ctrl+Home`).
    FileStart,
    /// The last byte of the file, by seek (`Ctrl+End`).
    FileEnd,
}

impl Motion {
    /// Does the goal column survive this movement?
    ///
    /// True for the vertical ones: a vertical move aims at the goal column and
    /// lands on the nearest column the row actually has, so a short line in the
    /// middle of a long file does not cost the column for the rest of it. A
    /// horizontal move, or a move to a row's end, *is* a statement about which
    /// column is wanted, so it sets the goal instead.
    ///
    ///
    /// The list is the: kept by `Up`, `Down`, `PageUp`, `PageDown`,
    /// `FileStart` and `FileEnd`; set by `Left`, `Right`, `RowStart` and
    /// `RowEnd`. It is a `match` rather than a comparison so that a motion
    /// added later cannot silently fall to one side.
    pub const fn keeps_goal_column(self) -> bool {
        match self {
            Self::Up | Self::Down | Self::PageUp | Self::PageDown => true,
            // Ctrl+Home / Ctrl+End land on a file end rather than in a column,
            // so there is no column in them to aim with and the old goal is the
            // only one there is.
            Self::FileStart | Self::FileEnd => true,
            Self::Left | Self::Right | Self::RowStart | Self::RowEnd => false,
        }
    }
}

/// A live selection: where it started, where the cursor has taken it, and how
/// it grows.
///
/// The anchor is kept rather than a normalised range so that shrinking works:
/// extending back past the anchor selects the other way round rather than
/// collapsing to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where `Shift` was first held.
    pub anchor: u64,
    /// Where the cursor is now.
    pub head: u64,
    /// The display column the anchor sat in - a text column in text mode, a
    /// byte column in hex. Only a rectangular selection reads it, and it is
    /// maintained for both kinds so that `Ctrl+Shift` can turn a linear
    /// selection into a block without the anchor's column having been lost.
    pub anchor_col: usize,
    /// The column the head is in.
    pub head_col: usize,
    /// Linear or rectangular.
    pub kind: SelectKind,
}

impl Selection {
    /// Start one at `at`, in column `col`.
    pub const fn new(at: u64, col: usize, kind: SelectKind) -> Self {
        Self {
            anchor: at,
            head: at,
            anchor_col: col,
            head_col: col,
            kind,
        }
    }

    /// The byte range, low end first. The head is exclusive going forward and
    /// inclusive coming back, so a one-byte selection is one byte either way.
    pub const fn range(&self) -> (u64, u64) {
        if self.head >= self.anchor {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The column band, low end first. `(lo, hi)` with `hi` **exclusive**.
    ///
    /// Meaningful for a rectangular selection; a linear one carries it so that
    /// `Alt+B` and `Ctrl+Shift` can make one out of it.
    pub const fn columns(&self) -> (usize, usize) {
        if self.head_col >= self.anchor_col {
            (self.anchor_col, self.head_col)
        } else {
            (self.head_col, self.anchor_col)
        }
    }

    /// How many bytes the range spans.
    ///
    /// For a rectangular selection this is the **span**, not the block's byte
    /// count: counting a block's bytes means reading every line between its two
    /// ends, and the design does not allow a status line to read 100 MB.
    /// The span is exact, free, and an upper
    /// bound on what a block copy can read, which is what makes it the right
    /// thing to check `viewer.copy_max` against.
    pub const fn len(&self) -> u64 {
        let (lo, hi) = self.range();
        hi.saturating_sub(lo)
    }

    /// True when nothing is actually selected.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Does this byte fall inside it?
    ///
    /// The byte range only; a rectangular selection's column band is a separate
    /// question and the row is where it can be asked (see [`Self::covers_row`]).
    pub const fn contains(&self, at: u64) -> bool {
        let (lo, hi) = self.range();
        at >= lo && at < hi
    }

    /// Does a row running `[from, to)` carry any of this selection?
    ///
    /// Linear: the byte ranges overlap, which is the ordinary half-open test.
    ///
    /// Rectangular: `from <= hi && to > lo`, so the head's own row is included
    /// even when the head sits exactly on its first byte.
    /// A block is the rows from the anchor's to
    /// the head's *inclusive of both*, and with `hi` exclusive the head's row
    /// would otherwise drop out of its own selection the moment the cursor
    /// reached column zero. The over-inclusion is bounded to exactly that row:
    /// the next row down starts past `hi` and fails the test.
    pub const fn covers_row(&self, from: u64, to: u64) -> bool {
        let (lo, hi) = self.range();
        match self.kind {
            SelectKind::Linear => from < hi && to > lo,
            SelectKind::Rectangular => from <= hi && to > lo,
        }
    }

    /// Move the head, keeping the anchor.
    ///
    /// The kind is replaced, which is what makes `Ctrl+Shift` after `Shift`
    /// convert a selection rather than start a second one.
    ///
    pub const fn extend_to(&mut self, at: u64, col: usize, kind: SelectKind) {
        self.head = at;
        self.head_col = col;
        self.kind = kind;
    }
}

/// Apply one movement's extension to the live selection.
///
///
/// The whole of that table, in one function, so that the ten motions cannot
/// disagree about what `Shift` means. `before` is the cursor's `(offset,
/// column)` as it was when the key arrived and `after` is where the movement
/// put it; the viewer computes both by walking the file and never by reading
/// its own laid-out rows.
///
/// * a bare movement returns `None` - the selection is cleared;
/// * a shifted movement with nothing live anchors at `before` and takes the
///   head to `after`, so the byte the cursor was on is inside the selection
///   going forward;
/// * a shifted movement with something live moves the head only, and takes the
///   extension's kind, which is how `Alt+B`'s job is also done by
///   `Ctrl+Shift` mid-selection.
pub fn apply_extend(
    sel: Option<Selection>,
    extend: Extend,
    before: (u64, usize),
    after: (u64, usize),
) -> Option<Selection> {
    let kind = extend.kind()?;
    let (at, col) = after;
    let mut live = match sel {
        Some(live) => live,
        None => Selection::new(before.0, before.1, kind),
    };
    live.extend_to(at, col, kind);
    Some(live)
}

/// What the status line says about a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionStatus {
    /// Linear or rectangular.
    pub kind: SelectKind,
    /// Byte span, low end first.
    pub from: u64,
    /// The high end, exclusive.
    pub to: u64,
    /// How wide a rectangular block is, in columns. Zero for a linear one.
    pub columns: usize,
}

impl SelectionStatus {
    /// What the status line should say about `sel`.
    ///
    /// The one place the "a block reports a span and a width, never a count"
    /// rule is applied (the design, invariant 14), so that the
    /// viewer and the renderer cannot come to different conclusions about it.
    pub const fn of(sel: &Selection) -> Self {
        let (from, to) = sel.range();
        let columns = match sel.kind {
            // A linear selection has no width to report: it wraps round, so
            // asking how many columns wide it is has no answer.
            SelectKind::Linear => 0,
            SelectKind::Rectangular => {
                let (lo, hi) = sel.columns();
                hi.saturating_sub(lo)
            }
        };
        Self {
            kind: sel.kind,
            from,
            to,
            columns,
        }
    }

    /// How many bytes the span covers.
    pub const fn span(&self) -> u64 {
        self.to.saturating_sub(self.from)
    }

    /// `sel 5 bytes`, or `sel block 6 cols over 1024 bytes`.
    ///
    ///
    /// The block form says `over` rather than giving a count because the count
    /// is the one thing it cannot know without reading every line the block
    /// spans.
    pub fn label(&self) -> String {
        let bytes = counted(self.span(), "byte", "bytes");
        match self.kind {
            SelectKind::Linear => format!("sel {bytes}"),
            SelectKind::Rectangular => {
                let cols = counted(self.columns as u64, "col", "cols");
                format!("sel block {cols} over {bytes}")
            }
        }
    }
}

/// `1 byte`, `5 bytes`: the count with the right noun.
///
/// The status line is read at the 60-column minimum, where "1 bytes"
/// is the kind of small wrongness that makes the rest look untrustworthy.
fn counted(n: u64, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// Whether a byte range lines up with the hex grouping.
///
/// A selection made on the characters side can begin or end **inside** a word,
/// because characters and words are not the same boundary. Nothing is rounded -
/// the range is the user's - but a copy from the bytes side cannot print a
/// value for a word it holds half of, so it falls back to hex digits and says
/// so.
///
/// Absolute file offsets are the right thing to measure because a row is a
/// whole number of words: [`super::hex::HexLayout::grouped`] applies the
/// rounding, so a row starts on a word boundary and the cells a row draws are
/// the words this counts.
///
/// `len` is the file's length when it is known, and the **end of the file is a
/// boundary too**: the last word "is shown as the bytes it has,
/// not padded with zeros it does not have", so a selection that stops where
/// the file stops is not half of anything and needs no fallback. Every other
/// end has to be on a word.
pub const fn word_aligned(from: u64, to: u64, len: Option<u64>, cfg: HexConfig) -> bool {
    let step = cfg.group.bytes() as u64;
    if step <= 1 {
        return true;
    }
    let at_eof = match len {
        Some(l) => to == l,
        None => false,
    };
    from.is_multiple_of(step) && (to.is_multiple_of(step) || at_eof)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear(anchor: u64, head: u64) -> Selection {
        let mut s = Selection::new(anchor, 0, SelectKind::Linear);
        s.head = head;
        s
    }

    /// A block from `(anchor, anchor_col)` to `(head, head_col)`.
    fn block(anchor: u64, anchor_col: usize, head: u64, head_col: usize) -> Selection {
        let mut s = Selection::new(anchor, anchor_col, SelectKind::Rectangular);
        s.extend_to(head, head_col, SelectKind::Rectangular);
        s
    }

    #[test]
    fn a_selection_reads_the_same_either_way_round() {
        let mut back = Selection::new(100, 0, SelectKind::Linear);
        back.head = 40;
        assert_eq!(back.range(), (40, 100));
        assert_eq!(back.len(), 60);

        let mut forward = Selection::new(40, 0, SelectKind::Linear);
        forward.head = 100;
        assert_eq!(forward.range(), (40, 100));
        assert_eq!(forward.len(), 60);
    }

    #[test]
    fn extending_back_past_the_anchor_selects_the_other_way() {
        // The anchor is kept rather than a normalised range precisely so this
        // works: shrinking to nothing and then continuing must not collapse.
        let mut s = Selection::new(50, 0, SelectKind::Linear);
        s.head = 60;
        assert_eq!(s.range(), (50, 60));
        s.head = 50;
        assert!(s.is_empty());
        s.head = 30;
        assert_eq!(
            s.range(),
            (30, 50),
            "now selecting backwards from the anchor"
        );
    }

    #[test]
    fn containment_is_half_open() {
        let s = linear(10, 14);
        assert!(!s.contains(9));
        assert!(s.contains(10), "the low end is in");
        assert!(s.contains(13));
        assert!(!s.contains(14), "the high end is not");
    }

    #[test]
    fn tab_returns_to_where_it_started() {
        assert_eq!(HexSide::Bytes.other(), HexSide::Chars);
        assert_eq!(HexSide::Bytes.other().other(), HexSide::Bytes);
        assert_eq!(HexSide::Bytes.label(), "bytes");
        assert_eq!(HexSide::Chars.label(), "chars");
    }

    #[test]
    fn word_alignment_is_only_a_question_above_one_byte() {
        use crate::config::{Endian, HexFormat, HexGroup};
        let bytes = HexConfig {
            group: HexGroup::Bits8,
            format: HexFormat::Hex,
            endian: Endian::Little,
        };
        assert!(
            word_aligned(3, 7, None, bytes),
            "single bytes are always aligned"
        );

        let words = HexConfig {
            group: HexGroup::Bits32,
            ..bytes
        };
        assert!(word_aligned(0, 8, None, words));
        assert!(!word_aligned(1, 8, None, words), "starts inside a word");
        assert!(!word_aligned(0, 7, None, words), "ends inside a word");

        // the file's last word "is shown as the bytes it has,
        // not padded with zeros it does not have", so a selection that stops
        // where the file stops is not half of anything - there is no rest of
        // the word for it to be missing.
        assert!(
            word_aligned(0, 7, Some(7), words),
            "the end of the file is a boundary too"
        );
        assert!(
            !word_aligned(1, 7, Some(7), words),
            "which says nothing about the low end"
        );
        assert!(
            !word_aligned(0, 7, Some(8), words),
            "and nothing about a word one byte short of complete"
        );
    }

    // ------------------------------------------------------ the columns ----

    #[test]
    fn the_column_band_is_low_end_first_and_the_high_end_is_exclusive() {
        let forward = block(0, 2, 96, 6);
        assert_eq!(forward.columns(), (2, 6), "columns 2, 3, 4 and 5");

        let backward = block(96, 6, 0, 2);
        assert_eq!(
            backward.columns(),
            (2, 6),
            "the same band, made the other way round"
        );
    }

    #[test]
    fn a_block_made_backwards_is_the_same_block() {
        // Ctrl+Shift+Up then Ctrl+Shift+Left has to select what Ctrl+Shift+Down
        // then Ctrl+Shift+Right over the same corners selects: a rectangle has
        // no direction.
        let down_right = block(32, 4, 80, 9);
        let up_left = block(80, 9, 32, 4);
        assert_eq!(down_right.range(), up_left.range());
        assert_eq!(down_right.columns(), up_left.columns());
        assert_eq!(
            SelectionStatus::of(&down_right).label(),
            SelectionStatus::of(&up_left).label()
        );
    }

    #[test]
    fn a_zero_width_block_selects_no_columns() {
        // Ctrl+Shift+Down with no horizontal movement: the rows are there but
        // the band is empty, and the status line says so rather than pretending
        // a column was chosen.
        let s = block(0, 3, 64, 3);
        assert_eq!(s.columns(), (3, 3));
        assert_eq!(SelectionStatus::of(&s).columns, 0);
    }

    // --------------------------------------------------------- the rows ----

    #[test]
    fn a_linear_selection_covers_the_rows_its_bytes_touch() {
        let s = linear(20, 40);
        assert!(!s.covers_row(0, 16), "entirely before it");
        assert!(s.covers_row(16, 32), "the anchor's row");
        assert!(s.covers_row(32, 48), "the head's row");
        assert!(!s.covers_row(48, 64), "entirely after it");
    }

    #[test]
    fn a_linear_selection_lets_go_of_the_row_its_head_starts() {
        // The head is exclusive, so a linear selection ending exactly at a row
        // boundary does not reach into the row that begins there.
        let s = linear(0, 32);
        assert!(s.covers_row(16, 32));
        assert!(!s.covers_row(32, 48));
    }

    #[test]
    fn a_block_covers_the_head_row_even_on_its_first_byte() {
        // Ctrl+Shift+Down twice from column 0 puts the head on the first byte
        // of its row. That row is the block's last row and dropping it would
        // make the block one row short of what is drawn.
        let s = block(0, 0, 32, 0);
        assert!(s.covers_row(0, 16), "the anchor's row");
        assert!(s.covers_row(16, 32), "the row between");
        assert!(s.covers_row(32, 48), "the head's own row, inclusive");
        assert!(!s.covers_row(48, 64), "and not the one after it");
    }

    #[test]
    fn a_block_that_has_not_moved_yet_covers_only_the_cursors_row() {
        let s = block(32, 4, 32, 4);
        assert!(!s.covers_row(16, 32));
        assert!(s.covers_row(32, 48));
        assert!(!s.covers_row(48, 64));
    }

    #[test]
    fn a_backwards_block_covers_both_end_rows() {
        // Anchor low in the file, head high up it: the anchor's row is the one
        // that now sits on the exclusive end and it must still be in the block.
        let s = block(40, 6, 0, 2);
        assert!(s.covers_row(0, 16), "the head's row");
        assert!(s.covers_row(32, 48), "the anchor's row");
        assert!(!s.covers_row(48, 64));
    }

    // ---------------------------------------------------- the extension ----

    #[test]
    fn an_extension_names_the_kind_it_makes() {
        assert_eq!(Extend::None.kind(), None);
        assert_eq!(Extend::Linear.kind(), Some(SelectKind::Linear));
        assert_eq!(Extend::Rectangular.kind(), Some(SelectKind::Rectangular));
    }

    #[test]
    fn a_bare_movement_clears_the_selection() {
        let live = linear(10, 20);
        assert_eq!(
            apply_extend(Some(live), Extend::None, (20, 4), (21, 5)),
            None
        );
        assert_eq!(apply_extend(None, Extend::None, (20, 4), (21, 5)), None);
    }

    #[test]
    fn shift_with_nothing_live_anchors_where_the_cursor_was() {
        let made = apply_extend(None, Extend::Linear, (20, 4), (24, 8));
        let made = made.expect("Shift with a movement starts a selection");
        assert_eq!(made.anchor, 20);
        assert_eq!(made.anchor_col, 4);
        assert_eq!(made.head, 24);
        assert_eq!(made.head_col, 8);
        assert_eq!(made.kind, SelectKind::Linear);
        assert_eq!(made.range(), (20, 24), "the byte it was on is selected");
    }

    #[test]
    fn shift_with_a_live_selection_moves_only_the_head() {
        let live = apply_extend(None, Extend::Linear, (20, 4), (24, 8));
        let live = live.expect("started");
        let grown = apply_extend(Some(live), Extend::Linear, (24, 8), (28, 12));
        let grown = grown.expect("extended");
        assert_eq!(grown.anchor, 20, "the anchor does not move");
        assert_eq!(grown.anchor_col, 4);
        assert_eq!(grown.head, 28);
        assert_eq!(grown.range(), (20, 28));
    }

    #[test]
    fn ctrl_shift_after_shift_converts_rather_than_starting_a_second() {
        // the design gives Ctrl+Shift to rectangular extension without
        // saying what it does to a linear selection already live. Converting it
        // keeps "one cursor and one selection" true.
        let live = apply_extend(None, Extend::Linear, (0, 0), (48, 3));
        let live = live.expect("started linear");
        assert_eq!(live.kind, SelectKind::Linear);

        let made = apply_extend(Some(live), Extend::Rectangular, (48, 3), (48, 7));
        let made = made.expect("converted");
        assert_eq!(made.kind, SelectKind::Rectangular);
        assert_eq!(made.anchor, 0, "the same anchor, not a second selection");
        assert_eq!(made.anchor_col, 0, "the anchor's column survived");
        assert_eq!(made.columns(), (0, 7));
    }

    #[test]
    fn extending_back_through_the_anchor_flips_the_band_too() {
        let live = apply_extend(None, Extend::Rectangular, (32, 8), (32, 12));
        let live = live.expect("started");
        assert_eq!(live.columns(), (8, 12));

        let flipped = apply_extend(Some(live), Extend::Rectangular, (32, 12), (32, 2));
        let flipped = flipped.expect("extended past the anchor's column");
        assert_eq!(
            flipped.columns(),
            (2, 8),
            "the band is now on the other side of the anchor"
        );
    }

    #[test]
    fn the_goal_column_survives_a_vertical_move_and_not_a_horizontal_one() {
        for motion in [
            Motion::Up,
            Motion::Down,
            Motion::PageUp,
            Motion::PageDown,
            Motion::FileStart,
            Motion::FileEnd,
        ] {
            assert!(
                motion.keeps_goal_column(),
                "{motion:?} does not choose a column, so it must not set the goal"
            );
        }
        for motion in [
            Motion::Left,
            Motion::Right,
            Motion::RowStart,
            Motion::RowEnd,
        ] {
            assert!(
                !motion.keeps_goal_column(),
                "{motion:?} is a statement about which column is wanted"
            );
        }
    }

    // ------------------------------------------------------ the readout ----

    #[test]
    fn a_linear_selection_reports_its_byte_count() {
        let s = linear(10, 15);
        assert_eq!(SelectionStatus::of(&s).label(), "sel 5 bytes");
    }

    #[test]
    fn one_of_anything_is_singular() {
        let one = linear(10, 11);
        assert_eq!(SelectionStatus::of(&one).label(), "sel 1 byte");

        let thin = block(0, 4, 1, 5);
        assert_eq!(
            SelectionStatus::of(&thin).label(),
            "sel block 1 col over 1 byte"
        );
    }

    #[test]
    fn a_block_reports_its_span_and_width_and_never_a_byte_count() {
        // Counting a block's bytes means reading every line
        // between its ends, which the design forbids a status line to do.
        let s = block(0, 2, 1024, 8);
        let status = SelectionStatus::of(&s);
        assert_eq!(status.columns, 6);
        assert_eq!(status.span(), 1024);
        assert_eq!(status.label(), "sel block 6 cols over 1024 bytes");
    }

    #[test]
    fn a_rectangular_span_is_not_the_blocks_byte_count() {
        // Six columns over sixty-five rows of a hundred-megabyte file: the span
        // is what is known for free and the count is what would cost the file.
        let s = block(0, 2, 100_000_000, 8);
        assert_eq!(s.len(), 100_000_000, "the span, stated as such");
        assert_eq!(
            SelectionStatus::of(&s).label(),
            "sel block 6 cols over 100000000 bytes",
            "and never a count of the bytes inside the block"
        );
    }

    #[test]
    fn a_backwards_selection_reports_the_same_thing_forwards() {
        let forward = linear(40, 100);
        let backward = linear(100, 40);
        assert_eq!(
            SelectionStatus::of(&forward),
            SelectionStatus::of(&backward)
        );
        assert_eq!(SelectionStatus::of(&backward).label(), "sel 60 bytes");
    }
}
