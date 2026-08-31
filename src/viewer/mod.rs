//! The internal viewer.
//!
//! > Internal, read-only, and **fully streaming**. No mode and no file size
//! > ever loads the whole thing into memory - this holds for local files, files
//! > inside archives, and files on a remote host alike, because all three
//! > arrive through the same `Vfs` reader.
//!
//! # The shape
//!
//! ```text
//!   Vfs::open_seek ──▶ Opener ──┬─▶ Source (rendering)  ──▶ Window ──▶ rows
//!                               └─▶ Source (index task) ──▶ IndexBatch ──▶ LineIndex
//! ```
//!
//! Two cursors over one file, taken from the same [`source::Opener`]. The
//! rendering cursor reads **one window at a time** and holds nothing else; the
//! index cursor walks the file forward in `viewer.index_chunk` steps on a
//! blocking-pool thread and posts what it finds. Neither waits for the other,
//! which is the "the file opens instantly; it does not wait for the
//! index".
//!
//! # Position is a byte offset
//!
//! [`Viewer::top`] is a **file byte offset**, not a line number, in both modes.
//! That is the single decision the rest of the design falls out of:
//!
//! * Scrolling is local. A line down is "find the next line break"; a line up is
//!   "find the previous one". Both are bounded reads near the current position
//!   and both are correct *before the index has reached here*, so a 40 GB file
//!   scrolls the moment it opens.
//! * Hex mode is arithmetic: row *n* is at `n * hex_width`, so hex mode needs no
//!   index at all.
//! * The [`index::LineIndex`] is then only needed for the three questions that
//!   genuinely span the file - which line number is this, how many lines are
//!   there, where is line *n* - and every one of those is allowed to be
//!   *approximate* while the scan is running, and says so ([`Status`]).
//!
//! # The memory rule, stated so it can be checked
//!
//! Everything the viewer holds is a function of the terminal, not of the file:
//! one [`source::Window`] (≤ [`source::MAX_WINDOW`]), one screenful of laid-out
//! rows, and the index's checkpoint list (≤ [`index::MAX_CHECKPOINTS`] entries,
//! whatever the file size - it decimates). There is no path through this module
//! that accumulates the file.

pub mod copy;
pub mod cursor;
pub mod decode;
pub mod encoding;
pub mod fileinfo;
pub mod find;
pub mod find_render;
pub mod hex;
pub mod highlight;
pub mod index;
pub mod inspect;
pub mod layout;
pub mod modes;
pub mod navigate;
pub mod refusal;
pub mod render;
pub mod rendered;
pub mod select;
pub mod source;
pub mod stack;
pub mod status;
pub mod summary;
pub mod summary_render;
pub mod template;
pub mod template_data;
pub mod text;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_width::UnicodeWidthStr;

use crate::config::{ViewerConfig, ViewerMode};
use crate::error::Result;
use crate::vfs::{Vfs, VfsPath};

use decode::{Detected, LineTerm, Resync, TextEncoding};
use find::{Find, FindBatch, FindJob, FindQuery, Found};
use hex::HexLayout;
use highlight::{Checkpoints, Highlighter, Span};
use index::{IndexBatch, LineIndex};
use select::{Extend, HexSide, Motion, SelectKind, Selection, SelectionStatus};
use source::{Opener, Source, WindowLen};

/// Which open viewer a background message belongs to.
///
/// A batch for an id nobody is showing any more is dropped rather than applied -
/// the same rule [`crate::app::ReadRequest`]'s generation counter enforces for
/// a directory read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ViewerId(pub u64);

/// Where a search hit opens the viewer.
///
/// Two variants and not one offset, because a content search does not always
/// know a file offset. `grep_searcher` reports positions in the stream it
/// read, and for three of the four charsets that stream is the
/// *decoded* one: a UTF-16LE hit's offset is roughly half the file offset of
/// the line it names ([`crate::vfs::ContentHit::decoded`]). Seeking to it put
/// the viewer a hundred lines away from the line the status bar had just
/// reported.
///
/// The line number is what decoding preserves exactly, so a transcoded hit
/// travels as one and is resolved against the file by [`Viewer::goto_line`] -
/// which is also what makes the answer approximate while the index is still
/// building, and says so, rather than being confidently wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitStart {
    /// A byte offset into the file itself (the position).
    Offset(u64),
    /// A **1-based** line number, as the searcher counts them.
    Line(u64),
}

/// The ceiling on what one [`Viewer::layout`] call may materialise **and on
/// what it may read**.
///
/// A function of the terminal - rows × columns × the widest encoding of a
/// character - with this as the absolute guard. Nothing about the file size
/// enters into it.
///
/// Both halves matter and only one of them used to be counted. Materialising is
/// what a row costs in memory; *reading* is what looking for the next line
/// start costs on a file that has no line breaks in it, and a layout that
/// charged itself only for the bytes it kept could read tens of megabytes a
/// frame while believing it had spent five hundred bytes (the design's
/// "memory is a function of the window size, not the file size", and
/// the design I9's "a file with no line breaks in it must not turn
/// one keystroke into a scan").
pub const MAX_LAYOUT_BYTES: u64 = 1024 * 1024;

/// How many times a layout may scroll back and try again on the last page.
///
/// See [`Viewer::layout_to_the_end`]. Two is enough for ragged lines; the bound
/// exists so a pathological file cannot turn one frame into many layouts.
const MAX_EOF_PULLBACKS: usize = 3;

/// How many windows one navigation step may read before giving up and
/// answering approximately.
///
/// This is what stops "scroll up one line" becoming a linear search of a 40 GB
/// file with no line breaks in it. When the budget runs out the
/// viewer lands on a bounded offset and marks the position approximate rather
/// than reading further.
pub const NAV_READ_BUDGET: u32 = 8;

/// [`NAV_READ_BUDGET`] as a byte count.
///
/// The budget is spent in bytes rather than in windows because a line-break
/// search reads a small window first and grows it (see [`Viewer::scan_forward`]):
/// counting windows would let eight cheap reads stand for eight expensive ones.
pub const NAV_READ_BYTES: u64 = (NAV_READ_BUDGET as u64).saturating_mul(source::MAX_WINDOW as u64);

/// The first read of a line-break search, and the factor each read grows by.
///
/// A line break is nearly always a few hundred bytes away, so starting at
/// [`source::MAX_WINDOW`] would read a quarter of a megabyte to find something
/// four hundred bytes ahead - once per row, once per navigation step. Starting
/// small and growing keeps the ordinary case at one short read and leaves the
/// pathological case reaching just as far, because the budget is in bytes.
const SCAN_STEP: usize = 4 * 1024;
const SCAN_GROWTH: usize = 4;

/// How far back a highlighting checkpoint may be and still be worth catching
/// up from (the memory and bounded-work rules).
///
/// A jump lands somewhere with no saved parse state. Rather than start the
/// window fresh - which gets a block comment that opened off screen wrong - the
/// layout walks forward from the nearest [`highlight::Checkpoints`] entry.
/// That walk is a read, so it has a ceiling: past this many bytes the honest
/// answer is a fresh parser, which is the same trade `End` makes with the
/// index.
pub const HL_CATCH_UP: u64 = 256 * 1024;

/// How long the walk described by [`HL_CATCH_UP`] may spend parsing.
///
/// **Not** [`highlight::PARSE_BUDGET`], which belongs to the visible window.
/// A resumed highlighter arrives with a full 250 ms of its own, and a catch-up
/// that spent all of it would freeze the frame it was computed for and then
/// hand back a *stalled* parser - one whose every further line comes back
/// plain, so the screen the walk was done for is drawn with no colour at all.
/// The catch-up gets a slice of a frame; failing to get there inside it means
/// starting fresh at the window top, which is the fallback the design
/// already describes for a jump.
pub const HL_CATCH_UP_TIME: std::time::Duration = std::time::Duration::from_millis(50);

/// What the event loop must spawn for a freshly opened viewer.
///
/// The `Viewer` builds this but does not spawn it, for the same reason
/// [`crate::input::dispatch`] queues a directory read instead of performing
/// one: the state machine stays drivable with no runtime and no terminal, and
/// every test in this module runs without either.
pub struct ScanJob {
    /// Which viewer the batches belong to.
    pub id: ViewerId,
    /// The index task's own cursor over the file.
    pub source: Source,
    /// `viewer.index_chunk`.
    pub chunk: u64,
    /// Set when the viewer closes; the scan checks it between chunks.
    pub cancel: Arc<AtomicBool>,
}

impl std::fmt::Debug for ScanJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanJob")
            .field("id", &self.id)
            .field("chunk", &self.chunk)
            .finish_non_exhaustive()
    }
}

/// How the applied template was arrived at.
///
/// The distinction exists for one rule: **a decision the user made must not be
/// silently replaced.** Hex mode matches a template on its own, and without
/// this it would do so over the top of a choice already made - including over
/// the deliberate choice of no template at all, which is why `Refused` is a
/// state and not simply the absence of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplatePick {
    /// Nothing has chosen one yet. Hex mode may match one from the file.
    Unset,
    /// Matched from the file's own head, without being asked.
    Auto,
    /// The user picked it out of the picker.
    Chosen,
    /// The user picked `(none)`. No colouring, and no automatic match either:
    /// turning it off has to stay off.
    Refused,
}

/// One laid-out screen row.
///
/// Produced by [`Viewer::layout`] and consumed by `crate::ui::viewer`. The two
/// variants are the two modes, and both come out of the same window read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A text-mode row.
    Text {
        /// The 0-based line number, when it is known. `None` while the index
        /// has not reached this far and the number would be a guess.
        line: Option<u64>,
        /// The byte offset this row's text starts at.
        offset: u64,
        /// True for the first row of a wrapped line, so the line number is
        /// printed once rather than on every continuation.
        first: bool,
        /// The text, tabs already expanded and controls already made visible.
        text: String,
        /// Highlight runs, as byte ranges into **this row's** `text`.
        ///
        /// Row-local and post-expansion, so the renderer needs no coordinate
        /// arithmetic of its own: a tab-indented line is coloured where its
        /// colours actually are, and a wrapped line's continuation rows carry
        /// the part of the highlighting they can see. Empty when highlighting
        /// is off or no syntax matched.
        spans: Vec<Span>,
        /// Quick-find matches inside this row, in the same coordinates as
        /// `spans`.
        matches: Vec<MatchRun>,
        /// What of this row the selection covers, as a byte range into **this
        /// row's `text`**. `None` when the row carries none of
        /// it.
        ///
        /// Row-local and post-expansion, exactly as `spans` and `matches` are,
        /// and for the same reason: the renderer has the row and nothing else,
        /// and a rectangular block's column band is turned into a byte range
        /// here rather than there.
        sel: Option<std::ops::Range<usize>>,
        /// Where the cursor is, as a byte index into `text`, when it is on this
        /// row. At most one row of a layout has it.
        cursor: Option<usize>,
        /// True when the line was cut at [`text::MAX_LINE_BYTES`].
        cut: bool,
    },
    /// A hex-mode row.
    Hex {
        /// The offset of the row's first byte.
        offset: u64,
        /// The bytes, at most `viewer.hex_width` of them and fewer on the last
        /// row of the file.
        bytes: Vec<u8>,
        /// Quick-find matches inside this row, as byte **indices into
        /// `bytes`**.
        matches: Vec<MatchRun>,
        /// What of this row the selection covers, as byte **indices into
        /// `bytes`**, as `matches` is.
        sel: Option<std::ops::Range<usize>>,
        /// The cursor's byte index into `bytes`, when it is on this row.
        cursor: Option<usize>,
    },
}

/// One quick-find match, as a laid-out row carries it.
///
/// The range is row-local in whatever the row's unit is - bytes of the
/// expanded text for [`Row::Text`], byte indices for [`Row::Hex`] - because
/// the renderer has the row and nothing else. `current` is what separates
/// the `viewer.current_match` from `viewer.match`: exactly one match
/// One side of a diff, already read and decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSide {
    /// What the `---` line calls it.
    pub label: String,
    /// Its whole text.
    pub text: String,
}

/// on screen can be the one `n` / `Shift+N` is standing on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRun {
    /// Where the match is, from the start of the row.
    pub range: std::ops::Range<usize>,
    /// True for the match the cursor is on.
    pub current: bool,
}

/// Clip match runs to one row's range and rebase them onto it.
///
/// The mirror of [`text::slice_spans`], and for the same reason: a line is
/// matched once and drawn in pieces. Public because the renderer clips again -
/// once for horizontal scrolling, and once more per highlight span, since a
/// match and a syntax run may start in different places.
pub fn slice_row_matches(runs: &[MatchRun], range: &std::ops::Range<usize>) -> Vec<MatchRun> {
    runs.iter()
        .filter_map(|m| {
            let start = m.range.start.max(range.start);
            let end = m.range.end.min(range.end);
            (start < end).then(|| MatchRun {
                range: start.saturating_sub(range.start)..end.saturating_sub(range.start),
                current: m.current,
            })
        })
        .collect()
}

impl Row {
    /// The file offset this row starts at, whichever mode it is.
    pub const fn offset(&self) -> u64 {
        match self {
            Self::Text { offset, .. } | Self::Hex { offset, .. } => *offset,
        }
    }
}

/// Everything the status line needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// What is being viewed.
    pub title: String,
    /// Text or hex.
    pub mode: ViewerMode,
    /// The offset the cursor is at - the "the current offset under
    /// the cursor is in the status line".
    pub offset: u64,
    /// The file's size, when the backend or the completed index knows it.
    pub len: Option<u64>,
    /// How far through the file the top of the screen is, 0-100.
    pub percent: Option<u8>,
    /// The 0-based line number at the top of the screen, when known.
    pub line: Option<u64>,
    /// How many lines have been found so far.
    pub lines: u64,
    /// True once the whole file has been indexed; until then `lines` is a
    /// lower bound and `line` may be missing.
    pub indexed: bool,
    /// How far the index has got, 0-100, while it is still running.
    pub index_percent: Option<u8>,
    /// **the design**: set when the position was answered from an index that
    /// has not finished, or from a navigation step that ran out of read budget.
    /// The status line says so rather than the seek being refused.
    pub approximate: bool,
    /// The active encoding's label.
    pub encoding: &'static str,
    /// How the encoding was arrived at.
    pub encoding_how: Detected,
    /// Whether the last decode hit an invalid sequence.
    pub decode_errors: bool,
    /// Line wrapping.
    pub wrap: bool,
    /// True when the file was detected as binary.
    pub binary: bool,
    /// True when highlighting is on for this file.
    pub highlighted: bool,
    /// What mode 3 is showing: the renderer's name, or the template's where a
    /// template's summary is what is on screen.
    pub render: Option<String>,
    /// What git says about the file: `git modified` or `git unmodified`, and
    /// `None` where git has nothing to say about it at all.
    pub git: Option<&'static str>,
    /// The field the cursor is standing in, already decoded as
    /// `name: value`. `None` in every mode but hex, and in hex whenever the
    /// cursor is outside every field a template explains.
    pub field: Option<String>,
    /// The live selection, when there is one.
    ///
    /// A block reports a span and a width and never a byte count: counting a
    /// block's bytes means reading every line between its two ends, which is
    /// the rule broken by a status line.
    ///
    pub selection: Option<SelectionStatus>,
    /// the interpretations readout, present only for a selection of
    /// 1, 2, 4 or 8 bytes whose low end the last layout had in its window.
    ///
    /// Already formatted - the same string `Ctrl+Shift+C` copies, character for
    /// character, so what is copied is what was read.
    ///
    pub interpretation: Option<String>,
    /// Which hex side has focus. Meaningless in text mode and
    /// the status line does not print it there.
    pub side: HexSide,
    /// the "A `width` that is not a whole number of words is
    /// rounded down to one, and says so": `(configured, in force)`, and
    /// `None` whenever the two are the same and there is nothing to say.
    pub hex_width_rounded: Option<(u16, u16)>,
    /// Why the index stopped early, if it did.
    pub error: Option<String>,
}

/// A run of the file **proven** to hold no line terminator.
///
/// Two offsets and a flag, and deliberately not a cache of the file: the
/// expensive thing about a line longer than a read budget is *discovering* that
/// it is one, and rediscovering it on every frame is what turns a viewer
/// sitting still on a 40 GB log into a reader of megabytes a second. What is
/// kept is the proof, whose size does not depend on the file's.
///
/// A line terminator is a fact about an encoding, so this belongs to the
/// encoding that proved it and is dropped when `F8` changes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NoBreak {
    /// First offset covered.
    from: u64,
    /// One past the last offset covered.
    to: u64,
    /// True when `to` is the end of the file - there is no next line start at
    /// all, rather than none proven yet.
    eof: bool,
}

impl NoBreak {
    /// True when `at` is inside the proven run.
    const fn covers(&self, at: u64) -> bool {
        at >= self.from && at < self.to
    }
}

/// An offset the viewer moved to, and whether it is **proven** to be a line
/// start.
///
/// Everything downstream turns on the difference: a line number may only be
/// stepped across a proven boundary, and a gutter number printed beside a row
/// that is really the middle of a line is a fabricated fact - the design's
/// line numbers, held to the honesty rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Step {
    /// The offset.
    at: u64,
    /// True when a line terminator was proven to end just before it.
    line_start: bool,
}

/// How one line-break search ended (the bounded work).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanEnd {
    /// A terminator was found and consumed: this offset **is** a line start.
    LineStart(u64),
    /// The file ended first. There is no next line start.
    Eof(u64),
    /// The read budget ran out. Nothing between the start and this offset is a
    /// terminator; past it, nothing is known.
    Budget(u64),
}

/// One open viewer.
pub struct Viewer {
    id: ViewerId,
    title: String,
    path: Option<VfsPath>,
    is_help: bool,

    source: Source,
    scan: Option<ScanJob>,
    cancel: Arc<AtomicBool>,
    idx: LineIndex,

    mode: ViewerMode,
    wrap: bool,
    line_numbers: bool,
    tab_width: u16,
    hex: HexLayout,
    /// `viewer.hex_width` as it was configured, before the design's
    /// rounding.
    ///
    /// Kept because the rounding depends on the grouping and `g` changes the
    /// grouping while the file is open: rounding the already-rounded width
    /// again would shrink the row every time the key was pressed. This is what
    /// [`HexLayout::grouped`] is re-applied to, and the pair is what the
    /// status line reports when they differ - the "and says so".
    hex_width_cfg: u16,
    /// Grouping, display format and byte order.
    hex_cfg: crate::config::HexConfig,
    /// The decimal reading last in use, so `d` back into decimal restores the
    /// sign rather than resetting it.
    hex_sign: crate::config::HexFormat,
    highlight_limit: u64,
    highlighting: bool,
    /// Whether the reading panel is open. Hex mode only, and remembered
    /// across a switch to text and back so `i` does not have to be pressed
    /// again after looking at something as text.
    inspect: bool,

    encoding: TextEncoding,
    encoding_how: Detected,
    bom_len: u64,
    shortlist: Vec<TextEncoding>,
    binary: bool,

    /// The byte offset of the top of the screen. A line start in text mode, a
    /// row start in hex mode.
    top: u64,
    /// The cursor's byte offset (the status line).
    cursor: u64,
    /// `viewer.cursor`. False restores pure scrolling: the
    /// arrows move the window, the cursor follows `top`, and there is no
    /// selection to be had.
    cursor_enabled: bool,
    /// Which row of the window the cursor is on, `0..view_rows`.
    ///
    /// Maintained **incrementally** by every move rather than searched for:
    /// `Down` increments it or scrolls when it is already on the last row,
    /// `Up` decrements it or scrolls, a page keeps it, a jump sets it. Nothing
    /// in the movement path reads [`Viewer::rows`], because
    /// `main::drain_input` applies every waiting key before the next layout and
    /// the rows are therefore N keystrokes stale.
    ///
    cursor_row: usize,
    /// Whether `cursor_row` still describes where the cursor is.
    ///
    /// It is maintained incrementally as the cursor moves, which is what keeps
    /// it cheap. A **view-only** scroll (the `Ctrl` with a
    /// movement) moves the window out from under it without the cursor moving
    /// at all, and there is no bounded way to fix it up in the general case:
    /// the cursor may now be any number of rows off the top or the bottom, and
    /// counting them means walking the file. So it is marked instead, and the
    /// next cursor movement pays for [`Viewer::reveal_cursor`] once, which is
    /// also the moment the view is meant to come back to the cursor.
    cursor_row_stale: bool,
    /// The display column vertical moves aim at.
    ///
    /// A short line puts the cursor at its end and leaves this alone, so moving
    /// on down a long line returns to the column it started in - which is what
    /// every editor does and what the design is silent about.
    /// [`Motion::keeps_goal_column`] is the one place the split lives.
    goal_col: usize,
    /// Which hex side has focus. `Tab` moves it and nothing
    /// else changes.
    side: HexSide,
    /// The live selection: two offsets, two columns and a kind, whatever the
    /// span (the memory rule, applied to a selection).
    sel: Option<Selection>,
    /// Up to eight bytes at the selection's low end, refreshed by
    /// [`Viewer::layout`] **from the window it already read**, so
    /// [`Viewer::status`] can format the readout without reading
    /// anything itself (the design, invariant 4).
    sel_preview: Option<Vec<u8>>,
    /// The rendered document, when mode 3 has built one.
    rendered: Option<render::Rendered>,
    /// Every foldable line of it, as `line -> last line inside`.
    render_regions: std::collections::BTreeMap<usize, usize>,
    /// Which of those are collapsed.
    render_folds: std::collections::BTreeSet<usize>,
    /// The first rendered line on screen. A line index, not a byte offset.
    render_top: usize,
    /// The rendered cursor's line.
    render_cursor: usize,
    /// Why mode 3 is showing something other than what was asked for.
    render_note: Option<String>,
    /// `viewer.render.max_size`, taken at open so the mode switch does not
    /// need the configuration.
    render_max: u64,
    /// Where [`Viewer::template`] came from, which is what keeps an
    /// automatic match from overwriting a choice.
    template_pick: TemplatePick,
    /// Set on entering hex mode with nothing chosen, and cleared by the first
    /// layout that has the front of the file in the window it already read.
    template_match_pending: bool,
    /// The field the cursor is standing in, decoded, refreshed by
    /// [`Viewer::layout`] from the window it already read - the same rule
    /// `sel_preview` follows, and for the same reason.
    field_reading: Option<String>,
    /// The binary struct template applied to the hex dump, or `None`.
    ///
    /// Chosen by hand from the picker, or matched automatically. The file information dialog matches a
    /// template to a file on its own; this is the other question - "read these
    /// bytes as this" - and the answer to it has to be the user's, because the
    /// bytes being asked about are usually ones no magic claims.
    template: Option<template::Template>,
    /// Where each of the active template's fields falls, as file offsets.
    ///
    /// Held rather than recomputed per row because the renderer asks for it
    /// once per hex row per frame, and rebuilt only when the answer can have
    /// changed - which is when the template changes or the cursor moves, since
    /// the template is applied **at the cursor**.
    template_spans: Vec<template::FieldSpan>,
    /// Where the applied template starts in the file.
    ///
    /// Fixed when the template is applied and **not** recomputed as the cursor
    /// moves. An earlier version anchored it to the cursor, which made a nice
    /// demonstration and was wrong twice over: the colouring slid sideways
    /// under the arrow keys, and the status line could never say `width: 1920`
    /// because the field under the cursor was whatever the arithmetic made it.
    /// A structure is somewhere; that is what makes it a structure.
    template_at: Option<u64>,
    /// The copy `Ctrl+C` queued, waiting for the event loop.
    ///
    /// Queued rather than performed, because copying reads the file and
    /// `dispatch` may not. See [`copy`] for the
    /// whole of it.
    copy_request: Option<copy::CopyRequest>,
    /// The line number of `top`, when it is known exactly.
    top_line: Option<u64>,
    /// True while `top` is a proven line start. False when it is the middle of
    /// a line too long to have been walked to the start of, which is what stops
    /// the row there being numbered as a line of its own.
    top_at_line_start: bool,
    /// Where the last movement *pointed*, which is not always where the cursor
    /// could land.
    ///
    /// A head is exclusive going forward, so `End` puts the cursor on the row's
    /// last character and points one past it. A selection started from there
    /// has to anchor on the point, not on the character: anchoring on the
    /// character meant `End` then `Shift+Left` selected the one before the last
    /// and could never reach the last at all.
    cursor_head: u64,
    /// The offset just past the last byte the window shows.
    ///
    /// The status line's percentage is measured from here rather than from
    /// `top`, because "how far through the file am I" is a question about what
    /// you can see, not about where the screen begins. Measuring the top makes
    /// the last page read 98% on a 50-row terminal and never reach 100 at all,
    /// which is what `less` gets right by reporting its bottom line.
    window_end: u64,
    /// How many wrapped rows of the line at `top` are scrolled off the top.
    ///
    /// Always zero with wrapping off, where a line is a row and `top` says
    /// everything. With wrapping **on** a line is several rows and `Down` moves
    /// one *row*, so the window can begin part-way through a line; this is how
    /// far in.
    ///
    /// A row index rather than a byte offset into the line, because where a row
    /// breaks is not a property of the bytes: tab stops are counted from the
    /// start of the line, so a line has to be expanded from its
    /// beginning for its rows to fall where the renderer will put them. Laying
    /// out from a mid-line offset would put a tab in a different column and
    /// break the rows somewhere else.
    top_row: usize,
    /// Horizontal scroll in display columns, with wrap off.
    hscroll: usize,
    /// `ui.ascii_borders`, so the glyphs this module *chooses* - the control
    /// pictures, not the file's own text - degrade with everything else.
    ///
    ascii: bool,
    /// What has been proven about the line the screen is inside, so it is not
    /// proven again every frame. See [`NoBreak`].
    no_break: Option<NoBreak>,

    /// The laid-out screen. Rebuilt by [`Viewer::layout`] once a frame.
    rows: Vec<Row>,
    view_rows: u16,
    view_cols: u16,

    /// Resumable highlighting states by row offset, from the last layout - the
    /// checkpoint the design resumes the visible window from. Bounded by the
    /// number of rows on screen.
    hl_states: Vec<(u64, Highlighter)>,
    /// Parse states kept beside the index's checkpoints, so a jump back into a
    /// part of the file that has been rendered before resumes rather than
    /// starting fresh. Capped for every file size, like the index's own list.
    hl_marks: Checkpoints,

    /// The find bar and its search.
    find: Find,
    /// Mode 3's own hits, over the rendered text rather than the file's bytes.
    /// See [`find_render`].
    render_hits: Vec<find_render::RenderHit>,
    /// Which of `render_hits` the cursor is on.
    render_hit: Option<usize>,
    /// The other side of a diff, when this viewer is showing one.
    ///
    /// The `---` side: the file the viewer holds is the `+++` one, so `1` and
    /// `2` still show its own text and bytes. Held as decoded text rather than
    /// as a path, because it was read once by the event loop and re-reading it
    /// on every mode switch would be I/O on a keystroke.
    diff_old: Option<DiffSide>,
    /// Whether mode 3 is currently showing the diff rather than the document.
    ///
    /// The file's own format wins by default: a modified `.md` opens as
    /// markdown, because that is what the file *is* and the diff is a question
    /// about it. A file no renderer claims - which is most source code - opens
    /// on its diff instead, there being no document for the diff to displace.
    /// Either way `toggle_diff` moves between them.
    diff_shown: bool,
    /// What git says about this file, for the status line. `None` when git
    /// has nothing to say: no repository, no commits, or an untracked file.
    git_state: Option<crate::git::State>,
    /// Whether `render_hits` has been built for the pattern now in the bar.
    ///
    /// A seeded pattern - the session's last search, installed when the viewer
    /// opens - compiles a matcher without running it, so mode 3 can hold a
    /// live pattern and an empty hit list at the same time. Without this the
    /// first `n` stepped an empty list and said "not found" about a word that
    /// is on screen.
    render_hits_built: bool,
    /// Where the bar was opened, which is where an incremental search starts
    /// from - so typing walks forward from where you were rather than from the
    /// top of the file on every character.
    find_origin: u64,
    /// The background match counter this viewer owes the event loop.
    find_job: Option<FindJob>,
    /// the "cancellable with `Esc`", and by the next keystroke.
    find_cancel: Option<Arc<AtomicBool>>,
    /// Where a search that ran out of read budget got to, forwards and
    /// backwards, so `n` picks the walk back up instead of restarting it
    /// (the "it starts returning hits on a huge file immediately").
    find_resume: (Option<u64>, Option<u64>),
    /// `viewer.index_chunk`, kept because the counter is spawned with it too.
    chunk: u64,

    approximate: bool,
    decode_errors: bool,
}

impl std::fmt::Debug for Viewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Viewer")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("mode", &self.mode)
            .field("top", &self.top)
            .field("cursor", &self.cursor)
            .field("encoding", &self.encoding.label())
            .finish_non_exhaustive()
    }
}

impl Drop for Viewer {
    /// Closing the viewer stops the scan (the index is a
    /// background task, and a background task outliving what it was for is a
    /// leak).
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        // And the match counter, for the same reason: a blocking-pool thread
        // reading 40 GB that nobody is looking at is a leak with a long fuse.
        if let Some(flag) = self.find_cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

impl Viewer {
    /// Open a viewer over an already-built [`Opener`].
    ///
    /// Reads exactly one window - the detection prefix - and returns. Nothing
    /// here is a function of the file's size, which is what makes "a 40 GB file
    /// must open as fast as a 4 KB one" true by construction rather than by
    /// measurement.
    pub fn open(
        id: ViewerId,
        title: impl Into<String>,
        path: Option<VfsPath>,
        opener: Opener,
        len: Option<u64>,
        cfg: &ViewerConfig,
    ) -> Result<Self> {
        let mut source = Source::open(Arc::clone(&opener), len)?;
        let probe = source.read_window(
            0,
            WindowLen::new(decode::SNIFF_BYTES.max(decode::BINARY_PROBE_BYTES)),
        )?;
        let complete = probe.hit_eof();
        let found = decode::resolve(&cfg.encoding, probe.bytes(), complete);
        let binary = decode::looks_binary(probe.bytes(), found.encoding);

        let chunk = cfg.index_chunk.bytes().max(index::MIN_CHUNK);
        let cancel = Arc::new(AtomicBool::new(false));
        let scan_source = Source::open(Arc::clone(&opener), source.len())?;
        let scan = ScanJob {
            id,
            source: scan_source,
            chunk,
            cancel: Arc::clone(&cancel),
        };

        // "A file detected as binary opens in hex automatically
        // unless overridden." The override is `viewer.default_mode = "hex"`
        // pointing the other way, and the `1`/`2` keys once it is open.
        let mode = if binary {
            ViewerMode::Hex
        } else {
            cfg.default_mode
        };
        // a width that is not a whole number of words is
        // rounded down to one, so that a row is a whole number of columns and
        // the cells a row draws begin on the file's own word boundaries.
        let hex = HexLayout::grouped(cfg.hex_width, cfg.hex);
        // the hex grid is the *file's* grid. Snapping here rather
        // than in `set_mode` is what makes a file that opens in hex land on
        // row 0 - `set_mode` returns early when the mode is already the one
        // being asked for, so it never runs for a file that never switched.
        let top = match mode {
            ViewerMode::Hex => hex.snap(found.bom_len),
            // Mode 3 keeps its position in its own rendered-line index and
            // leaves the byte window where text mode would have put it, so
            // falling back to text lands where the file starts.
            ViewerMode::Text | ViewerMode::Render => found.bom_len,
        };
        // Taken before the source moves into the struct.
        let known_len = source.len();
        let highlighting = cfg.highlight.engine == crate::config::HighlightEngine::Syntect
            && source
                .len()
                .is_some_and(|l| l <= cfg.highlight.max_size.bytes());

        Ok(Self {
            id,
            title: title.into(),
            path,
            is_help: false,
            source,
            scan: Some(scan),
            cancel,
            idx: LineIndex::new(chunk),
            mode,
            hex_cfg: cfg.hex,
            // Whatever the config asked for, if it asked for a decimal
            // reading; otherwise the reading `d` lands on first.
            hex_sign: if cfg.hex.format.is_decimal() {
                cfg.hex.format
            } else {
                crate::config::HexFormat::Unsigned
            },
            wrap: cfg.wrap,
            line_numbers: cfg.line_numbers,
            // Clamped at the door, so `tab_width()` reports what is actually
            // honoured rather than what was asked for (`text::MAX_TAB_WIDTH`).
            tab_width: cfg.tab_width.clamp(1, text::MAX_TAB_WIDTH),
            hex,
            hex_width_cfg: cfg.hex_width,
            highlight_limit: cfg.highlight.max_size.bytes(),
            highlighting,
            inspect: false,
            encoding: found.encoding,
            encoding_how: found.how,
            bom_len: found.bom_len,
            // "`F8` cycles through a **configurable** shortlist".
            // `encoding::shortlist` falls back to the own list when the
            // configured one is empty or wholly unrecognised, so a typo makes
            // `F8` ordinary rather than broken.
            shortlist: encoding::shortlist(&cfg.encoding.shortlist, found.encoding),
            binary,
            // A file that opens straight into hex mode starts on the byte
            // grid: the hex rows are "seek to `row * width`", and a
            // BOM is content like any other byte there. Text mode starts past
            // the BOM, which is the one thing that is not content.
            top,
            // A file that is nothing but a byte-order mark has no byte the
            // cursor can legally be on, and `top` is then past its end. The
            // cursor is a byte of the file before it is anything else
            // (the design invariant 1).
            cursor: hex::clamp_cursor(top, known_len),
            // the `viewer.cursor`, taken at the door so that every
            // key that would move a cursor can ask one field whether there is
            // one to move.
            cursor_enabled: cfg.cursor,
            cursor_row: 0,
            cursor_row_stale: false,
            goal_col: 0,
            side: HexSide::default(),
            sel: None,
            sel_preview: None,
            rendered: None,
            render_regions: std::collections::BTreeMap::new(),
            render_folds: std::collections::BTreeSet::new(),
            render_top: 0,
            render_cursor: 0,
            render_note: None,
            render_max: cfg.render.max_size.bytes(),
            template_pick: TemplatePick::Unset,
            template_match_pending: mode == ViewerMode::Hex,
            field_reading: None,
            template: None,
            template_spans: Vec::new(),
            template_at: None,
            copy_request: None,
            top_line: Some(0),
            top_at_line_start: true,
            top_row: 0,
            cursor_head: 0,
            window_end: 0,
            hscroll: 0,
            ascii: false,
            no_break: None,
            rows: Vec::new(),
            view_rows: 0,
            view_cols: 0,
            hl_states: Vec::new(),
            hl_marks: Checkpoints::new(),
            // "case-insensitive by default". There is no
            // `[viewer]` key for the other half of that sentence yet; see the
            // v0.4 notes.
            find: Find::default(),
            render_hits: Vec::new(),
            render_hit: None,
            render_hits_built: false,
            diff_old: None,
            diff_shown: false,
            git_state: None,
            find_origin: found.bom_len,
            find_job: None,
            find_cancel: None,
            find_resume: (None, None),
            chunk,
            approximate: false,
            decode_errors: false,
        })
    }

    /// Open a viewer over a path on a [`Vfs`].
    pub fn open_path(
        id: ViewerId,
        vfs: Arc<dyn Vfs>,
        path: VfsPath,
        cfg: &ViewerConfig,
    ) -> Result<Self> {
        let len = vfs.stat(&path).ok().map(|e| e.size);
        let title = path.to_string();
        let opener = source::vfs_opener(Arc::clone(&vfs), path.clone());
        Self::open(id, title, Some(path), opener, len, cfg)
    }

    /// Open a viewer over text that is already in memory.
    ///
    /// "The help view uses the same viewer machinery, so quick find (`F7`, `/`)
    /// works in it." This is that door, and it is the only one - see
    /// [`source::Source::from_memory`]. A help page additionally calls
    /// [`Viewer::mark_help`].
    pub fn open_memory(
        id: ViewerId,
        title: impl Into<String>,
        body: String,
        cfg: &ViewerConfig,
    ) -> Result<Self> {
        let bytes = Arc::new(body.into_bytes());
        let len = bytes.len() as u64;
        // A help page is UTF-8 by construction, so it is opened with the
        // encoding settled rather than sniffed.
        let mut v = Self::open(
            id,
            title,
            None,
            source::memory_opener(bytes),
            Some(len),
            cfg,
        )?;
        v.encoding = TextEncoding::UTF8;
        v.encoding_how = Detected::Configured;
        v.bom_len = 0;
        v.binary = false;
        v.mode = ViewerMode::Text;
        v.top = 0;
        v.cursor = 0;
        Ok(v)
    }

    /// Mark this viewer as a help page.
    ///
    /// Its only job is to make `F1` idempotent: pressing it on the help page
    /// must not stack a second copy of the help page. Deliberately *not* set by
    /// [`Viewer::open_memory`] - generated text is not necessarily help, and
    /// the design has several pages besides this one to come.
    pub const fn mark_help(&mut self) {
        self.is_help = true;
    }

    /// Take the background scan the caller owes this viewer. `None` after the
    /// first call - a viewer is indexed once.
    pub fn take_scan(&mut self) -> Option<ScanJob> {
        self.scan.take()
    }

    // ------------------------------------------------------------ facts ----

    /// Which viewer this is.
    pub const fn id(&self) -> ViewerId {
        self.id
    }

    /// What is being viewed.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The path, for a viewer over a file.
    pub const fn path(&self) -> Option<&VfsPath> {
        self.path.as_ref()
    }

    /// True for the `F1` help page, so `F1` inside it does not stack another.
    pub const fn is_help(&self) -> bool {
        self.is_help
    }

    /// The file's size, when it is known.
    pub const fn len(&self) -> Option<u64> {
        self.source.len()
    }

    /// True only when the source is known to hold nothing.
    pub const fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// The line index as it stands.
    pub const fn index(&self) -> &LineIndex {
        &self.idx
    }

    /// The active mode.
    pub const fn mode(&self) -> ViewerMode {
        self.mode
    }

    /// Line wrapping (`F2`).
    pub const fn wrap(&self) -> bool {
        self.wrap
    }

    /// Whether line numbers are drawn.
    pub const fn line_numbers(&self) -> bool {
        self.line_numbers
    }

    /// The tab stop (`viewer.tab_width`).
    pub const fn tab_width(&self) -> u16 {
        self.tab_width
    }

    /// The hex geometry.
    pub const fn hex(&self) -> HexLayout {
        self.hex
    }

    /// The active encoding.
    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    /// The top of the screen, as a byte offset.
    pub const fn top(&self) -> u64 {
        self.top
    }

    /// The cursor's byte offset.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// `viewer.cursor`. False restores pure scrolling.
    pub const fn cursor_enabled(&self) -> bool {
        self.cursor_enabled
    }

    /// Which row of the window the cursor is on, `0..view_rows`.
    ///
    /// Maintained incrementally by every move and every scroll - never
    /// recomputed from [`Viewer::rows`], which `main::drain_input` leaves N
    /// keystrokes stale.
    pub const fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// The display column the cursor is aiming at across vertical moves.
    ///
    pub const fn goal_column(&self) -> usize {
        self.goal_col
    }

    /// Which hex side has focus.
    pub const fn hex_side(&self) -> HexSide {
        self.side
    }

    /// `Tab`. Focus only: **nothing else changes**.
    ///
    /// > Selecting five bytes on the left and pressing `Tab` leaves five
    /// > characters selected on the right.
    ///
    /// One field, and deliberately one field: the two sides are two views of
    /// one cursor and one selection, not two of each, so a `Tab` that moved
    /// anything else would be the exact thing the two sides exist to avoid.
    pub const fn switch_hex_side(&mut self) {
        self.side = self.side.other();
    }

    /// The live selection, when there is one.
    pub const fn selection(&self) -> Option<Selection> {
        self.sel
    }

    /// Up to eight bytes at the selection's low end, as the last layout saw
    /// them.
    ///
    /// Filled from the window the layout already read, so a selection costs no
    /// read of its own however far it spans (invariant 4).
    pub fn selection_preview(&self) -> Option<&[u8]> {
        self.sel_preview.as_deref()
    }

    /// The binary struct template applied to the hex dump, or `None`.
    #[must_use]
    pub const fn template(&self) -> Option<&template::Template> {
        self.template.as_ref()
    }

    /// The name of the applied template, for the title bar.
    ///
    /// Shown because a colouring with nothing to explain it is a rendering
    /// bug as far as the person looking at it is concerned, and because `Esc`
    /// out of the picker deliberately leaves the previous choice in place -
    /// which is only safe if the choice is visible.
    #[must_use]
    pub fn template_name(&self) -> Option<&str> {
        self.template.as_ref().map(|t| t.name.as_str())
    }

    /// Where the applied template's fields fall, in file offsets.
    ///
    /// Ascending and non-overlapping, which is what
    /// [`template::field_at`] and [`template::coverage`] both rely on.
    #[must_use]
    pub fn template_spans(&self) -> &[template::FieldSpan] {
        &self.template_spans
    }

    /// Where the applied template came from.
    #[must_use]
    pub const fn template_pick(&self) -> TemplatePick {
        self.template_pick
    }

    /// The field the cursor is standing in, decoded, or `None` outside one.
    #[must_use]
    pub fn field_reading(&self) -> Option<&str> {
        self.field_reading.as_deref()
    }

    /// Apply a template by hand, or take the applied one away.
    ///
    /// This is the picker's entry point and it always records a decision: a
    /// `None` here is the user asking for no colouring, which is not the same
    /// as not having asked, and hex mode must not match over the top of it.
    ///
    /// The spans are not built here: they are a function of the cursor as well
    /// as of the template, and the next [`Viewer::layout`] builds them for
    /// wherever the cursor is by then. Clearing the key is what makes that
    /// happen.
    pub fn set_template(&mut self, template: Option<template::Template>) {
        self.template_pick = if template.is_some() {
            TemplatePick::Chosen
        } else {
            TemplatePick::Refused
        };
        self.template_match_pending = false;
        self.template = template;
        // A hand-picked template is anchored at the cursor, because picking
        // one is the act of saying "read the bytes *here* as this". An
        // automatic match is anchored where the template says its structure
        // lives, which is what `apply_matched_template` does instead.
        let at = self.cursor;
        self.anchor_template(at);
    }

    /// Apply a template the file matched, at the offset the template declares.
    pub(crate) fn apply_matched_template(&mut self, template: template::Template) {
        let at = template.offset as u64;
        self.template = Some(template);
        self.template_pick = TemplatePick::Auto;
        self.anchor_template(at);
    }

    /// Where the applied template is anchored, for tests.
    #[must_use]
    pub const fn template_at(&self) -> Option<u64> {
        self.template_at
    }

    /// Anchor the applied template at `at` and lay its fields out.
    ///
    /// Done once, when the template is applied, because the answer is a
    /// function of the template and the anchor and of nothing that changes
    /// while looking at it. Nothing per frame and nothing per byte.
    fn anchor_template(&mut self, at: u64) {
        let Some(template) = self.template.as_ref() else {
            self.template_spans.clear();
            self.template_at = None;
            return;
        };
        let len = self.source.len().unwrap_or(u64::MAX);
        self.template_spans = template::extents(template, at, len);
        self.template_at = Some(at);
    }

    /// Horizontal scroll in display columns (wrap off).
    pub const fn hscroll(&self) -> usize {
        self.hscroll
    }

    /// How many wrapped rows of the line at [`Viewer::top`] are above the
    /// window. Always zero with wrapping off.
    pub const fn top_row(&self) -> usize {
        self.top_row
    }

    /// The rows [`Viewer::layout`] last produced.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// How many rows the last layout was given.
    pub const fn view_rows(&self) -> u16 {
        self.view_rows
    }

    /// The line terminator for the active encoding.
    pub fn line_term(&self) -> LineTerm {
        self.encoding.line_term()
    }

    // ------------------------------------------------------ the cursor -----

    /// One movement, with or without an extension.
    ///
    /// The single entry point for every cursor key, so that the ten motions and
    /// the three extensions cannot be combined in ten different places and come
    /// out differently. With `viewer.cursor = false` it is the v0.4 page scroll
    /// and an extension is refused with a message rather than ignored.
    ///
    ///
    /// **Nothing under here reads [`Viewer::rows`].** `main::drain_input`
    /// applies every waiting key event before the next layout, so on a held
    /// `Down` this runs N times against rows that describe the screen as it was
    /// N keystrokes ago; anything computed from them lags by N and the cursor
    /// visibly trails the key. Where the row below is comes from
    /// [`Viewer::step_down`], the same function the layout uses; where the row
    /// above is comes from [`Viewer::prev_line_start`]; what is in a row comes
    /// from [`Viewer::read_line`].
    pub fn move_cursor(&mut self, motion: Motion, extend: Extend) -> Result<()> {
        if !self.cursor_enabled {
            return self.scroll_instead(motion, extend);
        }
        // A view-only scroll left the window somewhere else.
        // Bring it back to the cursor before moving, which is both what
        // `cursor_row` needs to be true again and what the user means by
        // pressing an arrow after looking away.
        if self.cursor_row_stale {
            self.reveal_cursor()?;
            self.cursor_row_stale = false;
        }
        // The anchor's column is only wanted when there is an extension, and
        // finding it costs a read - so it is not paid for on a bare arrow.
        let before = match extend {
            Extend::None => (self.cursor, 0),
            // `cursor_head`, not `cursor`: see the field. They differ exactly
            // where a movement pointed somewhere the cursor cannot sit.
            Extend::Linear | Extend::Rectangular => {
                (self.cursor_head.max(self.cursor), self.cursor_column()?)
            }
        };
        // Pointing past the cursor and asked to go back: the first step brings
        // the point onto the cursor rather than moving the cursor. `End` leaves
        // the cursor on the row's last character pointing one past it, and
        // `Shift+Left` there means "select that character" - moving the cursor
        // as well would skip it and select the one before instead.
        if matches!(extend, Extend::Linear | Extend::Rectangular)
            && matches!(motion, Motion::Left)
            && self.cursor_head > self.cursor
        {
            let col = self.cursor_column()?;
            self.sel = select::apply_extend(self.sel, extend, before, (self.cursor, col));
            self.cursor_head = self.cursor;
            self.follow_cursor(col);
            return Ok(());
        }
        if self.mode == ViewerMode::Render {
            // A rendered document has no byte cursor to move: its unit is a
            // drawn line, and the folds mean the line after this one is not
            // always the next one. `move_render` is that walk, and returning
            // here is what keeps every byte-offset path below out of a mode
            // that has no byte offsets.
            self.move_render(motion);
            return Ok(());
        }
        let moved = match self.mode {
            ViewerMode::Hex => self.move_hex(motion)?,
            ViewerMode::Text | ViewerMode::Render => self.move_text(motion)?,
        };
        if !motion.keeps_goal_column() {
            self.goal_col = moved.col;
        }
        // The **head**, not the cursor: they part company at the end of the
        // file and at the end of a row, where the cursor has to stop on a byte
        // the screen draws and a selection has to be able to cover it
        // ([`Moved`]).
        self.sel = select::apply_extend(self.sel, extend, before, (moved.head, moved.col));
        self.cursor_head = moved.head;
        self.follow_cursor(moved.col);
        Ok(())
    }

    /// Put the cursor at an offset without a key having moved it: a `Ctrl+G`
    /// jump, a find hit, a reload.
    ///
    /// Does **not** touch the selection. These are not movement keys and their
    /// whole purpose is to look somewhere else; making that a property of the
    /// entry point rather than of four call sites is what stops one of them
    /// forgetting.
    pub fn place_cursor(&mut self, at: u64) -> Result<()> {
        self.cursor_head = at;
        let floor = match self.mode {
            ViewerMode::Hex => 0,
            ViewerMode::Text | ViewerMode::Render => self.bom_len,
        };
        self.cursor = hex::clamp_cursor(at.max(floor), self.source.len());
        self.reveal_cursor()?;
        self.goal_col = self.cursor_column()?;
        Ok(())
    }

    /// `Ctrl+A`: `0..len`, **without reading a byte**.
    ///
    /// > selecting 40 GB is instant, and copying it is refused with the size
    ///
    /// The cursor and the window do not move: scrolling to the end of a 40 GB
    /// file would be a surprise and would throw away where the user was reading.
    ///
    pub fn select_all(&mut self) {
        if !self.cursor_enabled {
            return;
        }
        let len = self.source.len().unwrap_or_else(|| self.idx.scanned());
        let mut sel = Selection::new(0, 0, SelectKind::Linear);
        sel.extend_to(len, 0, SelectKind::Linear);
        self.sel = Some(sel);
    }

    /// `Esc`, stage one (the two-stage rule). True when there was a
    /// selection to clear, which is what tells the caller not to close.
    ///
    /// **A selection walked back onto its own anchor is not one.** `Shift+Right`
    /// then `Shift+Left` leaves the anchor live so that carrying on selects the
    /// other way round ([`select::apply_extend`]), but it covers no byte,
    /// nothing is painted, and [`Viewer::copy`] already answers
    /// `nothing selected` for it. Swallowing an `Esc` for it would make the two
    /// keys disagree about whether anything is selected, and the design's
    /// reason for the two-stage `Esc` - "losing a selection to a stray `Esc` is
    /// a nuisance" - has nothing to lose here. It is still taken: the anchor
    /// goes, and only the extra keypress does not.
    pub fn clear_selection(&mut self) -> bool {
        self.sel.take().is_some_and(|sel| !sel.is_empty())
    }

    /// `Alt+B`: turn the live selection into a block, or back.
    ///
    ///
    /// The anchor and the head do not move, so a selection already made can be
    /// re-read as a column block without being made again - which is what makes
    /// this useful on its own and not only as the fallback for a
    /// `Ctrl+Shift+arrow` a legacy terminal cannot deliver.
    pub fn toggle_selection_kind(&mut self) -> Option<SelectKind> {
        let sel = self.sel.as_mut()?;
        sel.kind = match sel.kind {
            SelectKind::Linear => SelectKind::Rectangular,
            SelectKind::Rectangular => SelectKind::Linear,
        };
        Some(sel.kind)
    }

    /// What a movement key does with `viewer.cursor = false`.
    ///
    ///
    /// v0.4's behaviour to the byte, and every selection key reports why it
    /// cannot act rather than doing nothing - the "never a panic and
    /// never silence".
    fn scroll_instead(&mut self, motion: Motion, extend: Extend) -> Result<()> {
        match extend {
            Extend::None => {}
            Extend::Linear | Extend::Rectangular => {
                return Err(crate::error::Error::Msg(copy::NO_CURSOR.to_string()));
            }
        }
        match motion {
            Motion::Up => self.scroll(-1),
            Motion::Down => self.scroll(1),
            Motion::PageUp => self.page(false),
            Motion::PageDown => self.page(true),
            Motion::Left => self.scroll_horizontal(-1),
            Motion::Right => self.scroll_horizontal(1),
            Motion::RowStart => self.line_start(),
            Motion::RowEnd => self.line_end(),
            Motion::FileStart => self.goto_start(),
            Motion::FileEnd => self.goto_end(),
        }
    }

    /// The display column the cursor is in: a **display** column of the
    /// expanded row in text mode, a byte column in hex.
    ///
    fn cursor_column(&mut self) -> Result<usize> {
        match self.mode {
            ViewerMode::Hex => Ok(usize::from(self.hex.column_of(self.cursor))),
            // Nothing in mode 3 has a column: the cursor is a whole line and
            // there is no horizontal axis for it to keep.
            ViewerMode::Render => Ok(0),
            ViewerMode::Text => {
                let step = self.cursor_line()?;
                let map = self.line_map(step)?;
                Ok(map.locate(self.cursor).1)
            }
        }
    }

    /// The cursor's row within the hex window, which is arithmetic: a hex row
    /// is a fixed stride, so there is nothing to walk.
    fn hex_cursor_row(&self) -> usize {
        let last = usize::from(self.view_rows.max(1)).saturating_sub(1);
        let rows = self
            .hex
            .row_of(self.cursor)
            .saturating_sub(self.hex.row_of(self.top));
        usize::try_from(rows).unwrap_or(last).min(last)
    }

    /// Bring the window to the cursor after a move.
    ///
    ///
    /// The vertical half is already done: [`Viewer::cursor_row_down`] and
    /// [`Viewer::cursor_row_up`] scroll one row at a time as the cursor leaves
    /// the window, which is what keeps `cursor_row` inside `0..view_rows`
    /// without anything being searched for. What is left is the horizontal
    /// axis, which only exists in text mode with wrapping off.
    fn follow_cursor(&mut self, col: usize) {
        if self.mode == ViewerMode::Render {
            // Mode 3 keeps its own window; `move_render` has already brought
            // it to the cursor and there is no column to scroll to.
            return;
        }
        match self.mode {
            ViewerMode::Hex => {
                self.follow_hex_cursor();
                self.cursor_row = self.hex_cursor_row();
            }
            // Unreachable in mode 3, which returned above; grouped so that a
            // fourth mode has to decide rather than fall to one side.
            ViewerMode::Text | ViewerMode::Render => {
                let last = usize::from(self.view_rows.max(1)).saturating_sub(1);
                self.cursor_row = self.cursor_row.min(last);
                if self.wrap {
                    // A wrapped view has no horizontal axis: every row already
                    // begins at column zero.
                    self.hscroll = 0;
                    return;
                }
                let cols = usize::from(self.view_cols.max(1));
                if col < self.hscroll {
                    self.hscroll = col;
                } else if col >= self.hscroll.saturating_add(cols) {
                    self.hscroll = col.saturating_sub(cols).saturating_add(1);
                }
            }
        }
    }

    /// Bring the window to the cursor, scrolling only when it has left it.
    ///
    /// The jump path, so a `Ctrl+G` or a find hit that is already on screen
    /// moves nothing - which is the reason for
    /// [`Viewer::reveal`] and holds for every jump.
    fn reveal_cursor(&mut self) -> Result<()> {
        self.cursor_row_stale = false;
        if self.mode == ViewerMode::Render {
            self.reveal_render();
            return Ok(());
        }
        match self.mode {
            ViewerMode::Hex => {
                self.follow_hex_cursor();
                self.cursor_row = self.hex_cursor_row();
                Ok(())
            }
            ViewerMode::Text | ViewerMode::Render => {
                if let Some(row) = self.window_row_of(self.cursor)? {
                    self.cursor_row = row;
                    return Ok(());
                }
                let at = self.cursor;
                self.goto_offset(at)?;
                self.cursor_row = self.window_row_of(self.cursor)?.unwrap_or(0);
                Ok(())
            }
        }
    }

    /// Move the cursor one screen row down, scrolling when it is already on the
    /// window's last row.
    fn cursor_row_down(&mut self) -> Result<()> {
        let last = usize::from(self.view_rows.max(1)).saturating_sub(1);
        if self.cursor_row >= last {
            return self.scroll(1);
        }
        self.cursor_row = self.cursor_row.saturating_add(1);
        Ok(())
    }

    /// Move the cursor one screen row up, scrolling when it is already on the
    /// window's first row.
    fn cursor_row_up(&mut self) -> Result<()> {
        if self.cursor_row == 0 {
            return self.scroll(-1);
        }
        self.cursor_row = self.cursor_row.saturating_sub(1);
        Ok(())
    }

    /// [`Viewer::cursor_row_down`] `n` times, bounded by the window.
    fn cursor_rows_down(&mut self, n: usize) -> Result<()> {
        for _ in 0..n.min(usize::from(self.view_rows.max(1))) {
            self.cursor_row_down()?;
        }
        Ok(())
    }

    /// [`Viewer::cursor_row_up`] `n` times, bounded by the window.
    fn cursor_rows_up(&mut self, n: usize) -> Result<()> {
        for _ in 0..n.min(usize::from(self.view_rows.max(1))) {
            self.cursor_row_up()?;
        }
        Ok(())
    }

    /// One movement in hex mode.
    ///
    /// The vertical moves **carry the column**, which changes v0.4's behaviour
    /// deliberately: the design recorded that a vertical move
    /// took the cursor to its row's first byte, because `scroll` ended with
    /// `cursor = top`. With a cursor that can select, `Shift+Down` has to take
    /// a column with it or a rectangular selection cannot be made at all.
    ///
    ///
    /// Every arm answers with two offsets (see [`Moved`]): where the cursor is
    /// to land, and where the movement *pointed*, which is where a `Shift`
    /// takes the head. They differ at the end of a row and at the end of the
    /// file, the two places a cursor has to stop on a byte and a selection has
    /// to be able to cover it.
    fn move_hex(&mut self, motion: Motion) -> Result<Moved> {
        let len = self.source.len();
        let stride = self.hex.stride();
        let (at, head) = match motion {
            // Both are seeks, and both already know how to put a window where
            // the cursor is going.
            Motion::FileStart => {
                self.goto_start()?;
                (self.cursor, self.cursor)
            }
            Motion::FileEnd => {
                self.goto_end()?;
                // `Ctrl+Shift+End` is "linear extend to the end of the file",
                // and the end of the file is one
                // past its last byte - which is where the cursor may not go.
                (self.cursor, len.unwrap_or(self.cursor))
            }
            Motion::PageUp | Motion::PageDown => {
                // The window moves by the page and the cursor keeps its row
                // index, which is what makes a page a page rather than a jump
                // to an edge.
                let row = u64::try_from(self.cursor_row).unwrap_or(0);
                let by = isize::try_from(self.view_rows.max(1))
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .max(1);
                self.scroll(if matches!(motion, Motion::PageDown) {
                    by
                } else {
                    -by
                })?;
                let col = u64::try_from(self.goal_col.min(usize::from(self.hex.width())))
                    .unwrap_or(0)
                    .min(stride.saturating_sub(1));
                let want = self
                    .top
                    .saturating_add(row.saturating_mul(stride))
                    .saturating_add(col);
                (want, want)
            }
            Motion::Up => {
                let up = self.cursor.checked_sub(stride).unwrap_or(self.cursor);
                (up, up)
            }
            Motion::Down => {
                let next = self.cursor.saturating_add(stride);
                let landing = match len {
                    // The last row of a file is usually short. Landing on its
                    // last byte is a move; refusing to move at all is not.
                    Some(l) if next >= l => {
                        if self.hex.row_of(self.cursor) < self.hex.rows(l).saturating_sub(1) {
                            l.saturating_sub(1)
                        } else {
                            self.cursor
                        }
                    }
                    Some(_) | None => next,
                };
                // The head follows the movement rather than the landing, so a
                // `Shift+Down` off the bottom of a short last row selects to
                // the end of the file instead of stopping a byte inside it.
                (landing, next)
            }
            // Each side moves by its own unit: a whole column
            // on the bytes side, snapped to the grouping, and one character -
            // which may be several bytes - on the characters side.
            // [`hex::side_step`] is the one place that split lives.
            Motion::Left | Motion::Right => {
                let delta = if matches!(motion, Motion::Left) {
                    -1
                } else {
                    1
                };
                let (start, bytes) = self.char_window(self.cursor)?;
                let stepped = hex::side_step(
                    self.side,
                    hex::CharWindow {
                        enc: self.encoding,
                        bytes: &bytes,
                        start,
                    },
                    self.hex_cfg,
                    len,
                    self.cursor,
                    delta,
                );
                // A forward step that could not move is a step held at the
                // file's last byte, whichever side is focused. The head still
                // has somewhere to go: the end of the file, which is what makes
                // that byte selectable at all.
                let head = if matches!(motion, Motion::Right) && stepped <= self.cursor {
                    len.unwrap_or(stepped)
                } else {
                    stepped
                };
                (stepped, head)
            }
            Motion::RowStart => {
                let row = self.hex.snap(self.cursor);
                (row, row)
            }
            // `End` is the row's last byte and the head is the row's end, one
            // past it, so `Shift+End` selects the whole row rather than all of
            // it but the last column.
            Motion::RowEnd => {
                let end = self.hex.snap(self.cursor).saturating_add(stride);
                (end.saturating_sub(1), end)
            }
        };
        self.cursor = hex::clamp_cursor(at, len);
        self.snap_hex_cursor();
        let head = match len {
            Some(l) => head.min(l),
            None => head,
        };
        Ok(Moved {
            col: usize::from(self.hex.column_of(self.cursor)),
            head,
        })
    }

    /// Pull the cursor back onto a word boundary on the bytes side.
    ///
    ///
    /// > Moving on the bytes side itself always advances a whole column, so a
    /// > selection made there is word-aligned without anyone having to think
    /// > about it.
    ///
    /// [`hex::column_step`] snaps the two motions that step a column;
    /// this is the same rule for the eight that do not. `End`, `Ctrl+End` and a
    /// `Down` onto a short last row all land on a byte chosen for being the
    /// last one rather than for being a column, and a selection anchored there
    /// afterwards would begin inside a word the user never touched the
    /// characters side for (the design invariant 9).
    ///
    /// The characters side is left alone: there one press is one character, and
    /// a character is not a word.
    fn snap_hex_cursor(&mut self) {
        if self.side != HexSide::Bytes {
            return;
        }
        let step = self.hex_cfg.group.bytes() as u64;
        if step <= 1 {
            return;
        }
        self.cursor = self.cursor.saturating_sub(self.cursor % step);
    }

    /// One movement in text mode.
    ///
    /// Returns the display column the cursor landed in, so the caller does not
    /// pay for a second walk of the file to find out.
    fn move_text(&mut self, motion: Motion) -> Result<Moved> {
        match motion {
            Motion::FileStart => {
                self.goto_start()?;
                let col = self.cursor_column()?;
                return Ok(Moved {
                    col,
                    head: self.cursor,
                });
            }
            Motion::FileEnd => {
                self.goto_end()?;
                let col = self.cursor_column()?;
                return Ok(Moved {
                    col,
                    // The end of the file, not its last byte: a head is
                    // exclusive going forward, so `Ctrl+Shift+End` reaches the
                    // last byte only by pointing one past it ([`Moved`]).
                    head: self.source.len().unwrap_or(self.cursor),
                });
            }
            Motion::PageUp | Motion::PageDown => {
                let row = self.cursor_row;
                let col = self.goal_col;
                let by = isize::try_from(self.view_rows.max(1))
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .max(1);
                self.scroll(if matches!(motion, Motion::PageDown) {
                    by
                } else {
                    -by
                })?;
                let col = self.place_on_window_row(row, col)?;
                return Ok(Moved {
                    col,
                    head: self.cursor,
                });
            }
            Motion::Up
            | Motion::Down
            | Motion::Left
            | Motion::Right
            | Motion::RowStart
            | Motion::RowEnd => {}
        }
        let step = self.cursor_line()?;
        let map = self.line_map(step)?;
        let (row, _) = map.locate(self.cursor);
        let goal = self.goal_col;
        // Where the movement pointed, when that is not where the cursor could
        // land ([`Moved`]). `None` means the two are the same.
        let mut head: Option<u64> = None;
        let col = match motion {
            Motion::Up => {
                if row > 0 {
                    self.cursor = map.place(row.saturating_sub(1), goal);
                    self.cursor_row_up()?;
                    map.locate(self.cursor).1
                } else if let Some(prev) = self.prev_map(&map)? {
                    let last = prev.row_count().saturating_sub(1);
                    self.cursor = prev.place(last, goal);
                    self.cursor_row_up()?;
                    prev.locate(self.cursor).1
                } else {
                    map.locate(self.cursor).1
                }
            }
            Motion::Down => {
                if row.saturating_add(1) < map.row_count() {
                    self.cursor = map.place(row.saturating_add(1), goal);
                    self.cursor_row_down()?;
                    map.locate(self.cursor).1
                } else if let Some(next) = self.next_map(&map)? {
                    self.cursor = next.place(0, goal);
                    self.cursor_row_down()?;
                    next.locate(self.cursor).1
                } else {
                    map.locate(self.cursor).1
                }
            }
            // One **character**, across a line boundary at either end of a
            // line. The terminator itself is skipped: the cursor is a byte
            // offset and every offset it takes has to be a byte the screen
            // draws, or the renderer has no cell to put it in.
            Motion::Left => {
                if let Some(at) = map.before(self.cursor) {
                    let (nrow, ncol) = map.locate(at);
                    self.cursor = at;
                    self.cursor_rows_up(row.saturating_sub(nrow))?;
                    ncol
                } else if let Some(prev) = self.prev_map(&map)? {
                    let last = prev.row_count().saturating_sub(1);
                    self.cursor = prev.row_end(last);
                    self.cursor_row_up()?;
                    prev.locate(self.cursor).1
                } else {
                    map.locate(self.cursor).1
                }
            }
            Motion::Right => {
                if let Some(at) = map.after(self.cursor) {
                    let (nrow, ncol) = map.locate(at);
                    self.cursor = at;
                    self.cursor_rows_down(nrow.saturating_sub(row))?;
                    ncol
                } else if let Some(next) = self.next_map(&map)? {
                    self.cursor = next.row_offset(0);
                    self.cursor_row_down()?;
                    next.locate(self.cursor).1
                } else {
                    // No character after this one anywhere in the file: the
                    // cursor stays on the last byte and the head goes past it,
                    // which is the only way that byte is ever selected.
                    head = self.source.len();
                    map.locate(self.cursor).1
                }
            }
            Motion::RowStart => {
                self.cursor = map.row_offset(row);
                0
            }
            Motion::RowEnd => {
                self.cursor = map.row_end(row);
                // `End` lands on the row's last character and `Shift+End`
                // selects through it, so the head is one past that character -
                // the row's own text, never its terminator.
                head = Some(map.row_end_head(row));
                map.locate(self.cursor).1
            }
            Motion::PageUp | Motion::PageDown | Motion::FileStart | Motion::FileEnd => goal,
        };
        // A line whose start is the end of the file has no character to land
        // on, and the cursor is a byte of the file whatever a row says
        // (the design invariant 1).
        self.cursor = hex::clamp_cursor(self.cursor, self.source.len());
        let head = match (head, self.source.len()) {
            (Some(want), Some(len)) => want.min(len),
            (Some(want), None) => want,
            (None, _) => self.cursor,
        };
        Ok(Moved {
            col,
            head: head.max(self.cursor),
        })
    }

    /// The bytes one character step on the hex characters side is measured in.
    ///
    ///
    /// A character's length is a fact about the bytes, so it takes bytes to
    /// answer - and this is the whole of what it takes: a lead-in of at most
    /// [`encoding::MAX_CHAR_BYTES`] so a backward step has a character to
    /// retreat over, and as much again ahead. A function of the encoding, never
    /// of the file.
    fn char_window(&mut self, at: u64) -> Result<(u64, Vec<u8>)> {
        let lead = at.min(encoding::MAX_CHAR_BYTES as u64);
        let from = at.saturating_sub(lead);
        let want = usize::try_from(lead)
            .unwrap_or(0)
            .saturating_add(encoding::MAX_CHAR_BYTES);
        let w = self.source.read_window(from, WindowLen::new(want))?;
        Ok((w.at(), w.bytes().to_vec()))
    }

    /// The line the cursor's screen row belongs to, walked from the **file**.
    ///
    /// One bounded backward scan for the ordinary case, which is every line
    /// short enough to be materialised whole. A line longer than that is laid
    /// out in chunks and the cursor's row is one of them, so the chunks are
    /// walked - bounded by [`NAV_READ_BUDGET`], past which the answer is the
    /// same grid [`Viewer::goto_offset`] uses and the position is marked
    /// approximate (the honesty rule).
    fn cursor_line(&mut self) -> Result<Step> {
        let start = self.line_start_at_or_before(self.cursor)?;
        let per_line = self.per_line().max(1);
        if self.cursor.saturating_sub(start.at) < per_line {
            return Ok(start);
        }
        let mut step = start;
        for _ in 0..NAV_READ_BUDGET {
            // No budget for chasing a line start: the chunk boundaries are what
            // `layout_text` walks, and chasing would be a read for an answer
            // that is not wanted here.
            let mut scan = 0_u64;
            let slice = self.read_line(step.at, per_line, &mut scan)?;
            let next = slice.next.max(step.at.saturating_add(1));
            if next > self.cursor || slice.eof {
                return Ok(step);
            }
            step = Step {
                at: next,
                line_start: slice.broke,
            };
        }
        self.approximate = true;
        Ok(step)
    }

    /// One line, expanded and wrapped exactly as [`Viewer::layout_text`] does
    /// it.
    fn line_map(&mut self, step: Step) -> Result<cursor::LineMap> {
        let per_line = self.per_line();
        let mut scan = NAV_READ_BYTES;
        let slice = self.read_line(step.at, per_line, &mut scan)?;
        let term = self.line_term();
        let body = term.trim_break(&slice.bytes);
        let (expanded, chars) = cursor::cells(
            self.encoding,
            body,
            step.at,
            !slice.cut,
            self.tab_width,
            self.ascii,
        );
        let rows = if self.wrap {
            text::wrap(&expanded, usize::from(self.view_cols))
        } else {
            // One row, the whole line; the window scrolls it sideways with
            // `hscroll`, exactly as the layout does.
            std::iter::once(0..expanded.len()).collect()
        };
        Ok(cursor::LineMap {
            start: step.at,
            line_start: step.line_start,
            body_end: step.at.saturating_add(body.len() as u64),
            next: slice.next.max(step.at.saturating_add(1)),
            broke: slice.broke,
            eof: slice.eof,
            empty: slice.bytes.is_empty() && slice.eof,
            expanded,
            chars,
            rows,
        })
    }

    /// The line below `map`, or `None` when it is the file's last.
    fn next_map(&mut self, map: &cursor::LineMap) -> Result<Option<cursor::LineMap>> {
        if map.eof && map.next <= map.start {
            return Ok(None);
        }
        let next = self.line_map(Step {
            at: map.next,
            line_start: map.broke,
        })?;
        Ok((!next.empty).then_some(next))
    }

    /// The line above `map`, or `None` when it is the file's first.
    ///
    /// The same two answers [`Viewer::scroll_wrapped`] takes, for the same
    /// reason: a proven line start steps to the previous line, and the inside
    /// of a line too long to have been walked to the start of steps back one
    /// row's worth of it rather than to a start megabytes above.
    fn prev_map(&mut self, map: &cursor::LineMap) -> Result<Option<cursor::LineMap>> {
        if map.start <= self.bom_len {
            return Ok(None);
        }
        let step = if map.line_start {
            self.prev_line_start(map.start)?
        } else {
            let per_line = self.per_line();
            let back = map.start.saturating_sub(per_line).max(self.bom_len);
            Step {
                at: self.resync_offset(back)?,
                line_start: false,
            }
        };
        Ok(Some(self.line_map(step)?))
    }

    /// Is `at` on the screen the window currently describes?
    ///
    /// Hex is arithmetic - a row is a fixed stride - and text is a walk of the
    /// rows the layout would produce, never of the rows it last did.
    ///
    fn window_shows(&mut self, at: u64) -> Result<bool> {
        match self.mode {
            ViewerMode::Hex => {
                let span = self
                    .hex
                    .stride()
                    .saturating_mul(u64::from(self.view_rows.max(1)));
                Ok(at >= self.top && at < self.top.saturating_add(span))
            }
            // Mode 3's window is over rendered lines, not over bytes, so no
            // byte offset is on its screen. Saying so keeps a mode switch from
            // trying to carry a position that does not exist here.
            ViewerMode::Render => Ok(false),
            ViewerMode::Text => Ok(self.window_row_of(at)?.is_some()),
        }
    }

    /// Which row of the current window `at` is on, walked from the file.
    /// `None` when it is not on screen.
    fn window_row_of(&mut self, at: u64) -> Result<Option<usize>> {
        let rows = usize::from(self.view_rows);
        if rows == 0 || at < self.top {
            return Ok(None);
        }
        let mut step = Step {
            at: self.top,
            line_start: self.top_at_line_start,
        };
        let mut sub = self.top_row;
        let mut seen = 0_usize;
        for _ in 0..rows.saturating_add(1) {
            let map = self.line_map(step)?;
            if map.empty {
                return Ok(None);
            }
            let count = map.row_count();
            let first = sub.min(count.saturating_sub(1));
            if at < map.next {
                let row = map.locate(at).0;
                // Above the window's own first row: the line is on screen and
                // this row of it is not, which `at < self.top` cannot catch
                // when the window begins part-way down a wrapped line.
                if row < first {
                    return Ok(None);
                }
                let found = seen.saturating_add(row.saturating_sub(first));
                return Ok((found < rows).then_some(found));
            }
            seen = seen.saturating_add(count.saturating_sub(first));
            if seen >= rows || (map.eof && map.next <= map.start) {
                return Ok(None);
            }
            step = Step {
                at: map.next,
                line_start: map.broke,
            };
            sub = 0;
        }
        Ok(None)
    }

    /// Put the cursor on window row `idx` at display column `col`, walking the
    /// file from `top` exactly as the layout walks it.
    ///
    /// A window with fewer rows than `idx` - the last page of a file - lands
    /// the cursor on the last row there is and says so through `cursor_row`.
    fn place_on_window_row(&mut self, idx: usize, col: usize) -> Result<usize> {
        let mut sub = self.top_row;
        let mut seen = 0_usize;
        let mut map = self.line_map(Step {
            at: self.top,
            line_start: self.top_at_line_start,
        })?;
        for _ in 0..usize::from(self.view_rows.max(1)).saturating_add(1) {
            let count = map.row_count();
            let first = sub.min(count.saturating_sub(1));
            let avail = count.saturating_sub(first).max(1);
            let want = idx.saturating_sub(seen);
            let ended = map.empty || (map.eof && map.next <= map.start);
            if !map.empty && want >= avail && !ended {
                let next = self.line_map(Step {
                    at: map.next,
                    line_start: map.broke,
                })?;
                if !next.empty {
                    seen = seen.saturating_add(avail);
                    sub = 0;
                    map = next;
                    continue;
                }
            }
            let row = first.saturating_add(want.min(avail.saturating_sub(1)));
            self.cursor = hex::clamp_cursor(map.place(row, col), self.source.len());
            self.cursor_row = seen.saturating_add(row.saturating_sub(first));
            return Ok(map.locate(self.cursor).1);
        }
        Ok(col)
    }

    // ------------------------------------------------------- quick find ----

    /// The find bar, for the renderer.
    pub const fn find(&self) -> &Find {
        &self.find
    }

    /// `F7` / `/` / `Ctrl+F` - open the bar.
    ///
    /// Whatever was searched for last survives, so `F7` `n` steps on with no
    /// retyping. The incremental search that follows starts from **here**, not
    /// from the top of the file.
    pub fn open_find(&mut self) {
        self.find.show();
        self.find_origin = self.cursor;
    }

    /// `Ctrl+F` again, with the bar already open: empty it.
    ///
    /// A second press used to do nothing, so starting a different search meant
    /// holding `Backspace` down over the old one. Emptying the bar also drops
    /// the highlights, which is the honest picture: there is no pattern, so
    /// nothing on screen matches it.
    pub fn clear_find(&mut self) -> Result<()> {
        self.find.set_input("");
        self.clear_render_hits();
        self.find_origin = self.cursor;
        self.run_find()?;
        Ok(())
    }

    /// What is in the find bar, for the session pattern.
    pub const fn find_query(&self) -> &FindQuery {
        self.find.query()
    }

    /// Offer a diff against `other`, the `---` side.
    ///
    /// Set before the initial mode is chosen, because it decides what mode 3
    /// renders. Whether the diff is what mode 3 *starts* on depends on whether
    /// the file has a document of its own: see [`Viewer::diff_shown`].
    pub fn set_diff_side(&mut self, other: DiffSide) {
        self.diff_shown = render::RenderKind::of_name(&self.title).is_none();
        self.diff_old = Some(other);
    }

    /// Record what git says about this file, for the status line.
    ///
    /// Separate from [`Viewer::set_diff_side`] because the two answers are
    /// different: a file can be tracked and unchanged, which is worth saying
    /// and has no diff to show.
    pub fn set_git_state(&mut self, state: crate::git::State) {
        self.git_state = Some(state);
    }

    /// Whether mode 3 is showing the diff rather than the file's own document.
    #[must_use]
    pub const fn diff_shown(&self) -> bool {
        self.diff_shown
    }

    /// Swap mode 3 between the file's document and its diff.
    ///
    /// Answers `false` when there is no diff to swap to, which the caller
    /// reports: a key that silently does nothing is the thing being avoided.
    pub fn toggle_diff(&mut self) -> Result<bool> {
        if self.diff_old.is_none() {
            return Ok(false);
        }
        self.diff_shown = !self.diff_shown;
        // Rebuilt rather than kept: the two are different documents, with
        // different line counts and different folds, and a cursor into one is
        // not a position in the other.
        if matches!(self.mode, ViewerMode::Render) {
            let limit = self.render_max;
            let _ = self.build_render(limit, crate::viewer::fileinfo::MATCH_HEAD);
        } else {
            self.set_mode(ViewerMode::Render)?;
        }
        Ok(true)
    }

    /// The other side of the diff, when this viewer is showing one.
    #[must_use]
    pub const fn diff_side(&self) -> Option<&DiffSide> {
        self.diff_old.as_ref()
    }

    /// Install the session's last pattern without running it.
    ///
    /// Compiled, so [`Viewer::layout`] highlights the matches in the window it
    /// is about to draw - that costs the window and nothing more, which is
    /// the rule that highlighting only needs the hits on screen.
    /// Deliberately **not** scanned: the match counter is a pass over the file
    /// and belongs to the `F3` that asks for it, not to opening a file the
    /// user may only want to read.
    pub fn seed_find(&mut self, query: FindQuery) {
        if query.input.is_empty() {
            return;
        }
        self.find.set_query(query);
        let hex_mode = matches!(self.mode, ViewerMode::Hex);
        self.find.compile(self.encoding, hex_mode);
        self.render_hits_built = false;
    }

    /// Open on a search hit, with the pattern that found it installed.
    ///
    ///
    /// > For a content match, `Enter` opens the viewer at the matching line
    /// > with the hit already highlighted - by the same `grep-regex` matcher
    /// > that found it.
    ///
    /// The bar stays **closed**: the user asked to look at a file, not to type
    /// a search, and `n` / `Shift+N` step through the rest of the file either
    /// way. `query` is `None` for a name-only search, which has nothing to
    /// highlight, and for a regex one, which the find bar cannot compile
    /// ([`find::REGEX_MILESTONE`]); the cursor still lands on the hit.
    ///
    /// A byte offset is preferred because the design makes position one and
    /// a line number is approximate until the index has finished building -
    /// which it has not, here. But a hit found in a transcoded stream has no
    /// file offset to give, so [`HitStart`] carries whichever of the two the
    /// searcher actually knew, and this seeks by it.
    pub fn open_at_hit(&mut self, start: HitStart, query: Option<FindQuery>) -> Result<()> {
        if let Some(query) = query {
            self.find.set_query(query);
            let hex_mode = matches!(self.mode, ViewerMode::Hex);
            self.find.compile(self.encoding, hex_mode);
        }
        let at = match start {
            HitStart::Offset(offset) => {
                self.place_cursor(offset)?;
                offset
            }
            HitStart::Line(line) => {
                // The searcher counts from one and `goto_line` from zero.
                self.goto_line(line.saturating_sub(1))?;
                // Where it actually landed: the line start it proved, which is
                // the furthest known one when the index has not reached that
                // far yet. Anchoring the highlight on the line number's
                // *intended* offset would be anchoring it on a number nobody
                // has.
                self.cursor
            }
        };
        if self.find.matcher().is_some() {
            // The hit is *this* one, so `n` steps to the next rather than
            // re-finding this one, and the highlight is painted at once.
            self.find.set_current(Some(at));
            self.find_origin = at;
            self.queue_find_scan();
        }
        Ok(())
    }

    /// `Esc` - close the bar, keep the position and the matches.
    ///
    ///
    /// The background counter is cancelled here, which is the other half of
    /// the "cancellable with `Esc`".
    pub fn close_find(&mut self) {
        self.find.hide();
        self.cancel_find_scan();
    }

    /// One character typed into the bar. Searches immediately.
    pub fn find_type(&mut self, ch: char) -> Result<Found> {
        if !self.find.push(ch) {
            return Ok(Found::None);
        }
        self.run_find()
    }

    /// `Backspace` in the bar. Searches again from the same origin.
    pub fn find_backspace(&mut self) -> Result<Found> {
        if !self.find.backspace() {
            return Ok(Found::None);
        }
        self.run_find()
    }

    /// Re-run the incremental search after a change to the query or the mode.
    ///
    /// Mode 3 searches the document it drew rather than the file underneath
    /// it, in memory and in one pass; see [`find_render`] for why the two
    /// searches are different questions.
    fn run_find(&mut self) -> Result<Found> {
        self.cancel_find_scan();
        if matches!(self.mode, ViewerMode::Render) {
            // The matcher is normally compiled inside `Find::search`, which
            // this path does not reach; mode 3 needs it compiled just the
            // same, and against plain text rather than hex, because what it
            // searches is a `String` per line.
            self.find.recompile(self.encoding, false);
            return Ok(self.run_find_rendered());
        }
        let hex_mode = matches!(self.mode, ViewerMode::Hex);
        let enc = self.encoding;
        let from = self.find_origin;
        self.find_resume = (None, None);
        let found = self.find.search(
            &mut self.source,
            enc,
            hex_mode,
            from,
            find::FIND_READ_BUDGET,
        )?;
        self.settle(found, true)?;
        // The count is the background's; the first match was not.
        self.queue_find_scan();
        Ok(found)
    }

    /// `n` - the next match.
    ///
    /// **Resumes** where the last bounded step stopped. One keystroke reads
    /// [`find::FIND_READ_BUDGET`] windows and no more - that is what stops a
    /// search over 40 GB freezing the UI - so "no match yet" has to be a
    /// position the next `n` carries on from, or the far end of a huge file
    /// would be unreachable by the only key that goes there.
    pub fn find_next(&mut self) -> Result<Found> {
        if matches!(self.mode, ViewerMode::Render) {
            return Ok(self.find_step_rendered(true));
        }
        let Some(matcher) = self.find.matcher().cloned() else {
            return Ok(Found::None);
        };
        let from = match self.find_resume.0.take() {
            Some(at) => at,
            None => self
                .find
                .current()
                .map_or(self.cursor, |at| at.saturating_add(1)),
        };
        let found = find::find_forward(&mut self.source, &matcher, from, find::FIND_READ_BUDGET)?;
        self.settle(found, true)
    }

    /// `Shift+N` - the previous match. The mirror of [`Viewer::find_next`].
    pub fn find_prev(&mut self) -> Result<Found> {
        if matches!(self.mode, ViewerMode::Render) {
            return Ok(self.find_step_rendered(false));
        }
        let Some(matcher) = self.find.matcher().cloned() else {
            return Ok(Found::None);
        };
        let before = match self.find_resume.1.take() {
            Some(at) => at,
            None => self.find.current().unwrap_or(self.cursor),
        };
        let found =
            find::find_backward(&mut self.source, &matcher, before, find::FIND_READ_BUDGET)?;
        self.settle(found, false)
    }

    /// Record what one bounded search step found, and show it.
    fn settle(&mut self, found: Found, forward: bool) -> Result<Found> {
        match found {
            Found::Hit(at) => {
                self.find.set_current(Some(at));
                self.reveal(at)?;
            }
            // Not "there is no match" - "there is no match in the bytes just
            // read". The offset is where to carry on from, and keeping it is
            // the difference between a bounded search and a truncated one.
            Found::Budget(at) => {
                if forward {
                    self.find_resume.0 = Some(at);
                } else {
                    self.find_resume.1 = Some(at);
                }
            }
            Found::None => {}
        }
        Ok(found)
    }

    /// Put the cursor on `at` and scroll only if it is not already on screen.
    ///
    /// the design wants "the first match highlighted as you type", and a
    /// screen that jumped on every keystroke to put the match on the top row
    /// would make the surrounding text unreadable. So a match already visible
    /// moves nothing.
    fn reveal(&mut self, at: u64) -> Result<()> {
        if self.cursor_enabled {
            // A find hit is not a movement key, so it keeps the selection.
            // `place_cursor` is the entry point
            // that does not touch it, which is what makes that a property of
            // the code rather than of four call sites.
            return self.place_cursor(at);
        }
        let last = self.rows.last().map_or(self.top, Row::offset);
        if at >= self.top && at < last {
            self.cursor = at;
            return Ok(());
        }
        self.goto_offset(at)
    }

    /// The background match counter this viewer owes the event loop.
    ///
    ///
    /// `Some` once per query, then `None` - the same shape as
    /// [`Viewer::take_scan`], and for the same reason: `dispatch` may not spawn
    /// anything.
    pub fn take_find_job(&mut self) -> Option<FindJob> {
        self.find_job.take()
    }

    /// Fold in one batch from the counter. False when it is not this viewer's,
    /// or belongs to a search two keystrokes ago.
    pub fn apply_find(&mut self, batch: &FindBatch) -> bool {
        if batch.id != self.id {
            return false;
        }
        self.find.apply(batch)
    }

    /// Build the counter for the query as it now stands.
    fn queue_find_scan(&mut self) {
        if self.find.matcher().is_none() {
            return;
        }
        // The counter gets its own cursor over the file, taken from the same
        // opener as everything else (the two-cursors shape).
        let Ok(source) = Source::open(self.source.opener(), self.source.len()) else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.find_job = self
            .find
            .job(self.id, source, self.chunk, Arc::clone(&cancel));
        if self.find_job.is_some() {
            self.find_cancel = Some(cancel);
        }
    }

    /// Stop whatever counter is running. Idempotent.
    fn cancel_find_scan(&mut self) {
        if let Some(flag) = self.find_cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        self.find_job = None;
    }
}

/// Where one movement left things.
///
/// Two answers rather than one, because the cursor and a selection's head stop
/// in different places. The cursor is a byte of the file and the last one it
/// may sit on is `len - 1` ([`hex::clamp_cursor`]); a head is **exclusive**
/// going forward ([`Selection::range`]), so covering that last byte means a
/// head of `len`. Tying the two together is what made `Ctrl+Shift+End` stop one
/// byte short of the end of the file it names,
/// and `Shift+End` stop one column short of the end of its row.
///
/// So each motion says where it *pointed*: the cursor is that clamped onto a
/// byte, and the head is that clamped to the file's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Moved {
    /// The display column the cursor landed in - a display column of the
    /// expanded row in text mode, a byte column in hex.
    col: usize,
    /// Where a `Shift` held with this movement takes the selection's head.
    head: u64,
}

/// How many bytes the interpretations readout can ever want.
///
/// Eight, because the readings it gives are for 1, 2, 4 or 8 bytes and there is
/// no reading of three bytes this program could state without inventing a type
/// for them.
pub const SEL_PREVIEW_BYTES: usize = 8;

/// What of one hex row a selection covers, as byte indices into the row.
///
///
/// A byte column in hex is a column of the row's own `0..hex_width`, so a
/// rectangular selection takes the same byte of every row - the third byte of
/// every record, which is the hex equivalent of taking one field out of aligned
/// output.
fn hex_row_sel(sel: &Selection, at: u64, taken: u64, width: u16) -> Option<std::ops::Range<usize>> {
    let to = at.saturating_add(taken);
    if !sel.covers_row(at, to) {
        return None;
    }
    let idx = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
    match sel.kind {
        SelectKind::Linear => {
            let (lo, hi) = sel.range();
            let from = lo.max(at);
            let end = hi.min(to);
            (from < end).then(|| idx(from.saturating_sub(at))..idx(end.saturating_sub(at)))
        }
        SelectKind::Rectangular => {
            let (lo, hi) = sel.columns();
            let w = usize::from(width).min(idx(taken));
            let from = lo.min(w);
            let end = hi.min(w);
            (from < end).then_some(from..end)
        }
    }
}

/// What of one text row a selection covers, as a byte range into that row's own
/// expanded text.
///
/// A row wholly inside a linear selection needs no character table and is
/// answered from its own length, which is what keeps `Ctrl+A` free. Everything
/// else goes through the table, so a rectangular block's column band is turned
/// into a byte range **here** and the renderer is left with no coordinate
/// arithmetic of its own.
fn text_row_sel(
    sel: Option<Selection>,
    map: Option<&cursor::LineMap>,
    row: usize,
    piece: &str,
    whole: bool,
) -> Option<std::ops::Range<usize>> {
    let sel = sel?;
    if whole {
        return (!piece.is_empty()).then_some(0..piece.len());
    }
    let map = map?;
    match sel.kind {
        SelectKind::Linear => {
            let (lo, hi) = sel.range();
            map.range_in_row(row, lo, hi)
        }
        SelectKind::Rectangular => {
            let from = map.row_offset(row);
            let to = map.row_end_offset(row);
            if !sel.covers_row(from, to) {
                return None;
            }
            // A column in text mode is a **display** column of the expanded
            // row, so a block takes one field out of tab-aligned output
            // whatever mixture of tabs and wide characters produced the
            // alignment.
            let (lo, hi) = sel.columns();
            let range = text::column_range(piece, lo, hi.saturating_sub(lo));
            (range.start < range.end).then_some(range)
        }
    }
}

/// Expand one decoded line, carrying its spans **and** its matches across.
///
/// One pass, because both coordinate systems have to survive the same
/// expansion and expanding twice would be reading the line twice. Every
/// endpoint of every span and every match is offered to
/// [`text::expand_tracked`] as a want, in sorted order, and read back out of
/// the map it returns - so every range that comes back is on a character
/// boundary of the expanded string and is non-decreasing.
fn expand_row(
    decoded: &str,
    tab_width: u16,
    ascii: bool,
    spans: &[Span],
    hits: &[(std::ops::Range<usize>, bool)],
) -> (String, Vec<Span>, Vec<MatchRun>) {
    if spans.is_empty() && hits.is_empty() {
        return (
            text::expand(decoded, tab_width, ascii),
            Vec::new(),
            Vec::new(),
        );
    }
    let mut wants: Vec<usize> =
        Vec::with_capacity(spans.len().saturating_add(hits.len()).saturating_mul(2));
    for s in spans {
        wants.push(s.range.start);
        wants.push(s.range.end);
    }
    for (r, _) in hits {
        wants.push(r.start);
        wants.push(r.end);
    }
    wants.sort_unstable();
    wants.dedup();
    let (expanded, mapped) = text::expand_tracked(decoded, tab_width, ascii, &wants);
    let at = |b: usize| -> usize {
        wants
            .binary_search(&b)
            .ok()
            .and_then(|i| mapped.get(i).copied())
            .unwrap_or(expanded.len())
    };
    let spans = spans
        .iter()
        .filter_map(|s| {
            let range = at(s.range.start)..at(s.range.end);
            (range.start < range.end).then_some(Span {
                range,
                slot: s.slot,
            })
        })
        .collect();
    let matches = hits
        .iter()
        .filter_map(|(r, current)| {
            let range = at(r.start)..at(r.end);
            (range.start < range.end).then_some(MatchRun {
                range,
                current: *current,
            })
        })
        .collect();
    (expanded, spans, matches)
}

/// One line's bytes, as [`Viewer::read_line`] found them.
struct LineSlice {
    bytes: Vec<u8>,
    cut: bool,
    next: u64,
    eof: bool,
    /// True when `next` is a **proven** line start - a terminator was consumed,
    /// or the file ended. False when the row below is this same line
    /// continuing, which is what keeps the gutter from numbering a
    /// continuation as a line of its own.
    broke: bool,
}

#[cfg(test)]
mod tests;
