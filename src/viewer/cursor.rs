//! Where the cursor is on a laid-out row.
//!
//! **Pure.** Nothing here opens, seeks or reads anything: it is handed one
//! line's bytes - which the caller has already read - and answers the two
//! questions the cursor keeps asking.
//!
//! * *Which row and which column is this byte offset in?* ([`LineMap::locate`])
//! * *Which byte offset is that column of that row?* ([`LineMap::place`])
//!
//! # Why a map rather than arithmetic
//!
//! A cursor is a **file byte offset** (the design I5) and a column
//! is a **display column of the expanded row**.
//! Three transformations sit between them and none of them is arithmetic:
//! the active encoding decides how many bytes a character is,
//! tab expansion decides which column it lands in, and wrapping
//! decides which row that column belongs to. [`LineMap`] is those three
//! applied once, the way [`super::Viewer::layout`] applies them, so navigation
//! and the screen cannot disagree about where a row breaks.
//!
//! # Why it is built from the file and not from `Viewer::rows`
//!
//! `main::drain_input` applies every waiting key event before the next layout,
//! so on a held `Down` the movement code runs N times against rows that still
//! describe the screen as it was N keystrokes ago. Anything computed from them
//! lags by N and the cursor visibly trails the key.
//! A `LineMap` costs one bounded read - a
//! function of the terminal, never of the file - and is always current.

use std::ops::Range;

use unicode_width::UnicodeWidthStr;

use super::decode::TextEncoding;
use super::{encoding, text};

/// One character of a line: where it begins in the **file**, and where it
/// begins in the line's **expanded** text.
///
/// The two are not related by arithmetic. A four-byte UTF-8 character is one
/// column; a tab is one byte and up to [`text::MAX_TAB_WIDTH`] columns; a lone
/// carriage return is one byte and no columns at all, because
/// [`text::expand`] drops it rather than let it move the terminal's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharCell {
    /// The character's first byte, as a file offset.
    pub at: u64,
    /// Where the character begins in the expanded text.
    pub exp: usize,
}

/// One line of the file, expanded and wrapped exactly as `layout_text` does it.
///
/// "Line" is the unit [`super::Viewer::read_line`] returns, which is a line up
/// to the point where it becomes too long to materialise; past that it is one
/// row's worth of a line that continues, and the fields say which
/// (`broke`, `eof`).
#[derive(Debug, Clone)]
pub struct LineMap {
    /// The line's first byte.
    pub start: u64,
    /// True when `start` is a **proven** line start rather than a bounded
    /// guess inside a line too long to have been walked to the start of
    /// (the honesty rule).
    pub line_start: bool,
    /// One past the last byte of the line's body, before its terminator.
    pub body_end: u64,
    /// Where the row below this line's last row begins.
    pub next: u64,
    /// True when `next` is a proven line start, so the row below carries the
    /// next line number rather than this one's.
    pub broke: bool,
    /// True when the read that produced this line reached the end of the file.
    pub eof: bool,
    /// True when there was nothing here at all: the file ended before `start`.
    pub empty: bool,
    /// The expanded text of the whole line, tabs already stops and controls
    /// already pictures - byte for byte what the rows are cut out of.
    pub expanded: String,
    /// Every character of the line, in order. Sorted by both fields.
    pub chars: Vec<CharCell>,
    /// The wrapped rows, as byte ranges into `expanded`. Exactly one range,
    /// spanning the whole line, with wrapping off.
    pub rows: Vec<Range<usize>>,
}

impl LineMap {
    /// How many screen rows this line occupies. Never zero: an empty line is
    /// still a row.
    pub fn row_count(&self) -> usize {
        self.rows.len().max(1)
    }

    /// One row's byte range into [`LineMap::expanded`], clamped to a row that
    /// exists.
    pub fn row(&self, idx: usize) -> Range<usize> {
        let last = self.rows.len().saturating_sub(1);
        self.rows
            .get(idx.min(last))
            .cloned()
            .unwrap_or(0..self.expanded.len())
    }

    /// The expanded byte offset the character containing `at` begins at.
    ///
    /// Binary search rather than a scan, because a line is as long as
    /// `per_line` lets it be and the cursor asks this several times a
    /// keystroke.
    fn expanded_of(&self, at: u64) -> usize {
        let i = self.chars.partition_point(|c| c.at <= at);
        i.checked_sub(1)
            .and_then(|i| self.chars.get(i))
            .map_or(0, |c| c.exp)
    }

    /// Which row an expanded byte offset falls on.
    fn row_of_expanded(&self, exp: usize) -> usize {
        let mut last = 0_usize;
        for (i, r) in self.rows.iter().enumerate() {
            if exp < r.end {
                return i;
            }
            last = i;
        }
        last
    }

    /// The row `at` is on, and the **display column** it is in, measured from
    /// the start of that row.
    pub fn locate(&self, at: u64) -> (usize, usize) {
        let exp = self.expanded_of(at);
        let row = self.row_of_expanded(exp);
        let r = self.row(row);
        let head = self
            .expanded
            .get(r.start..exp.clamp(r.start, r.end))
            .unwrap_or("");
        (row, UnicodeWidthStr::width(head))
    }

    /// The file offset of the character at display column `col` of row `row`.
    ///
    /// A short row puts the cursor on its last character; the *goal* column is
    /// the caller's and is deliberately not clamped with it.
    /// Slicing is [`text::column_range`], the
    /// same rule the row is drawn through, so a grapheme straddling the column
    /// is not split.
    pub fn place(&self, row: usize, col: usize) -> u64 {
        let r = self.row(row);
        let piece = self.expanded.get(r.clone()).unwrap_or("");
        let byte = text::column_range(piece, 0, col).end;
        let target = r.start.saturating_add(byte);
        self.cell_at_or_before(&r, target)
            .unwrap_or_else(|| self.row_offset(row))
    }

    /// The last character of row `r` beginning at or before `target`.
    fn cell_at_or_before(&self, r: &Range<usize>, target: usize) -> Option<u64> {
        let mut found = None;
        for c in &self.chars {
            if c.exp >= r.end || c.exp > target {
                break;
            }
            if c.exp >= r.start {
                found = Some(c.at);
            }
        }
        found
    }

    /// The file offset row `row` begins at - `Home`.
    pub fn row_offset(&self, row: usize) -> u64 {
        let r = self.row(row);
        self.chars
            .iter()
            .find(|c| c.exp >= r.start)
            .map_or(self.start, |c| c.at)
    }

    /// The file offset of the **last character** of row `row` - `End`.
    ///
    /// The last character, not one past it: the cursor is a byte offset and
    /// every offset it takes has to be a byte the screen actually draws, or
    /// the renderer has no cell to put it in.
    pub fn row_end(&self, row: usize) -> u64 {
        let r = self.row(row);
        self.chars
            .iter()
            .rev()
            .find(|c| c.exp < r.end && c.exp >= r.start)
            .map_or_else(|| self.row_offset(row), |c| c.at)
    }

    /// One past the **last character** of row `row` - where `Shift+End` takes
    /// a selection's head.
    ///
    /// [`LineMap::row_end`] is where the *cursor* lands, which is that last
    /// character itself, because a cursor is a byte the screen draws. A
    /// selection's head is exclusive going forward, so covering the row's last
    /// character means pointing one past it.
    ///
    /// For a wrapped row that is the next row's first character; for a line's
    /// last row it is the end of the line's **text**, so `Shift+End` selects a
    /// line and not its terminator.
    pub fn row_end_head(&self, row: usize) -> u64 {
        let r = self.row(row);
        self.chars
            .iter()
            .find(|c| c.exp >= r.end)
            .map_or(self.body_end, |c| c.at)
    }

    /// The character after the one at `at`, when the line still has one.
    pub fn after(&self, at: u64) -> Option<u64> {
        self.chars.iter().find(|c| c.at > at).map(|c| c.at)
    }

    /// The character before the one at `at`, when the line still has one.
    pub fn before(&self, at: u64) -> Option<u64> {
        self.chars.iter().rev().find(|c| c.at < at).map(|c| c.at)
    }

    /// The file offset the row below `row` begins at, or `LineMap::next` when
    /// `row` is this line's last.
    pub fn row_end_offset(&self, row: usize) -> u64 {
        let r = self.row(row);
        self.chars
            .iter()
            .find(|c| c.exp >= r.end)
            .map_or(self.next, |c| c.at)
    }

    /// Where the cursor draws inside row `row`'s own text, as a byte index
    /// into it - what [`super::Row::Text`]'s `cursor` field carries.
    ///
    /// `None` when the offset is not on that row.
    pub fn cursor_in_row(&self, row: usize, at: u64) -> Option<usize> {
        if self.row_of_expanded(self.expanded_of(at)) != row {
            return None;
        }
        let r = self.row(row);
        let exp = self.expanded_of(at);
        (exp >= r.start && exp <= r.end).then(|| exp.saturating_sub(r.start))
    }

    /// The byte range of the file range `[lo, hi)` inside row `row`'s own
    /// text - what [`super::Row::Text`]'s `sel` field carries.
    ///
    /// `None` when the row carries none of it.
    pub fn range_in_row(&self, row: usize, lo: u64, hi: u64) -> Option<Range<usize>> {
        let r = self.row(row);
        let from = self.expanded_of(lo).clamp(r.start, r.end);
        // The high end is exclusive in file bytes, so it becomes the expanded
        // offset of the first character **not** in it - the character starting
        // at or after `hi`, or the end of the row when the range runs past it.
        let to = self
            .chars
            .iter()
            .find(|c| c.at >= hi)
            .map_or(r.end, |c| c.exp)
            .clamp(r.start, r.end);
        (from < to).then(|| from.saturating_sub(r.start)..to.saturating_sub(r.start))
    }
}

/// Expand one line's bytes and record where each of its characters lands.
///
/// The bridge between the two coordinate systems, and the reason it can be
/// built at all is [`encoding::char_starts`]: the file offsets come from
/// there, the expanded offsets come from [`text::expand_tracked`], and the two
/// lists are the same characters in the same order.
///
/// `complete` is `!cut` - a line the layout gave up on part way through may end
/// in half a character, and the decoder holds those bytes back rather than
/// inventing a replacement glyph out of good ones.
pub fn cells(
    enc: TextEncoding,
    body: &[u8],
    start: u64,
    complete: bool,
    tab_width: u16,
    ascii: bool,
) -> (String, Vec<CharCell>) {
    let decoded = encoding::decode_window(enc, body, complete).text;
    cells_of(enc, body, &decoded, start, tab_width, ascii)
}

/// [`cells`] for a caller that has already decoded the line.
///
/// The layout has: it decodes every row it draws, and decoding the cursor's row
/// a second time to find out where the cursor is would be a read's worth of
/// work for an answer already paid for.
pub fn cells_of(
    enc: TextEncoding,
    body: &[u8],
    decoded: &str,
    start: u64,
    tab_width: u16,
    ascii: bool,
) -> (String, Vec<CharCell>) {
    let starts = encoding::char_starts(enc, body);
    let dec: Vec<usize> = decoded.char_indices().map(|(i, _)| i).collect();
    // A legacy encoding may map one input character to more than one Unicode
    // scalar. The two lists then stop being the same characters, and the
    // honest answer is the prefix over which they still are - which is every
    // encoding this viewer ships except in the rarest corner of a few of them.
    let n = starts.len().min(dec.len());
    let wants = dec.get(..n).unwrap_or(&[]);
    let (expanded, mapped) = text::expand_tracked(decoded, tab_width, ascii, wants);
    let chars = starts
        .iter()
        .take(n)
        .zip(mapped.iter())
        .map(|(raw, exp)| CharCell {
            at: start.saturating_add(*raw as u64),
            exp: *exp,
        })
        .collect();
    (expanded, chars)
}
