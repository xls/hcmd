//! Hex mode.
//!
//! > **Hex** - offset column, `viewer.hex_width` bytes per row (16 default),
//! > and an ASCII gutter. Same streaming path: seek to `row * width`, read the
//! > window, render it. Byte offsets are shown in both hex and decimal, and the
//! > current offset under the cursor is in the status line. `Ctrl+G` jumps to
//! > an offset, accepting `0x` notation.
//!
//! There is no decoding here and no index: a hex row's byte range is
//! arithmetic, which is why hex mode works identically on a file whose index
//! has not started. Everything in this module is a pure function of an offset
//! and a width.
//!
//! # The three things this module is careful about
//!
//! * **Both bases.** the design asks for offsets "in both hex and decimal".
//!   The cursor's offset is shown in both in the status line, always. The
//!   *per-row* offset column shows the decimal half **only when the terminal
//!   has room for it after the bytes and the gutter** - [`HexPlan`] decides,
//!   once per frame, so the answer cannot change from row to row. Two full
//!   offset columns cost 12-22 of an 80-column terminal, and the bytes are what
//!   the mode is for.
//! * **The last row is short.** A file rarely ends on a row boundary. The hex
//!   column pads with blanks so the gutter stays where the eye left it, and the
//!   ASCII gutter renders *only the bytes that exist* - no `.` for a byte that
//!   is not there.
//! * **A row that does not fit is cropped, never wrapped.** `viewer.hex_width`
//!   is the user's, and 64 bytes per row needs 220 columns. A row wider than
//!   the terminal loses its right-hand end rather than folding onto the next
//!   line and doubling every row's height. [`HexPlan::row`] does the cropping,
//!   so it is one place and it is testable without a terminal.
//!
//! # Binary files
//!
//! "A file detected as binary opens in hex automatically unless
//! overridden." The detection itself lives in [`super::decode::looks_binary`]
//! (a NUL byte in the first 8 KiB, or more than 5% odd C0 controls, with a
//! two-byte-unit encoding excused because UTF-16 is half NULs and is text); the
//! *consequence* is [`initial_mode`] here.

use crate::config::ViewerMode;
use crate::config::{Endian, HexConfig, HexFormat};

use super::decode::TextEncoding;
use super::encoding;
use super::select::HexSide;

/// The widest `viewer.hex_width` that is honoured.
///
/// Beyond this a row cannot fit any terminal this application supports
/// (the 60-column minimum is the floor, not the ceiling, but a row of
/// 64 bytes already needs 220 columns).
pub const MAX_HEX_WIDTH: u16 = 64;

/// How the bytes of a file are laid out in rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexLayout {
    width: u16,
}

impl HexLayout {
    /// Clamp `viewer.hex_width` into something renderable. Zero would divide by
    /// zero and a huge value would allocate a row nothing can show, so both are
    /// pulled into range rather than rejected.
    pub const fn new(width: u16) -> Self {
        let width = if width == 0 {
            16
        } else if width > MAX_HEX_WIDTH {
            MAX_HEX_WIDTH
        } else {
            width
        };
        Self { width }
    }

    /// [`HexLayout::new`] with the rounding applied.
    ///
    /// > `width` is bytes per row throughout, so changing the word size
    /// > regroups the same row rather than changing how much of the file each
    /// > line covers. A `width` that is not a whole number of words is rounded
    /// > down to one, and says so.
    ///
    /// So a row is always a whole number of columns, whatever `g` has been
    /// pressed since the file was opened. That is what makes a row's own cell
    /// boundaries and the file's word boundaries the same boundaries: a row
    /// starts at a multiple of the width, the width is a multiple of the word,
    /// so every cell begins on a word. Without it the two disagree from the
    /// second row on, and a selection the arithmetic calls aligned lands in
    /// the middle of a drawn column ([`super::select::word_aligned`]).
    ///
    /// Never below one whole word: rounding a width of 2 at `group = 64` down
    /// to nothing would leave no row to draw. The **saying so** belongs to the
    /// caller: [`super::Viewer`] keeps the configured width beside this one and
    /// the status line reports the pair whenever they differ.
    pub const fn grouped(width: u16, cfg: HexConfig) -> Self {
        let asked = Self::new(width).width;
        let step = cfg.group.bytes() as u16;
        if step == 0 {
            return Self { width: asked };
        }
        let whole = asked.saturating_sub(asked % step);
        Self {
            width: if whole == 0 { step } else { whole },
        }
    }

    /// Bytes per row.
    pub const fn width(self) -> u16 {
        self.width
    }

    /// Bytes per row, as the `u64` the arithmetic wants.
    pub const fn stride(self) -> u64 {
        self.width as u64
    }

    /// The file offset row `row` starts at. Saturating, so a row number past
    /// the end of a `u64` clamps rather than wrapping into the file.
    pub const fn row_offset(self, row: u64) -> u64 {
        row.saturating_mul(self.stride())
    }

    /// Which row an offset falls in.
    pub const fn row_of(self, offset: u64) -> u64 {
        offset / self.stride()
    }

    /// How many rows a file of `len` bytes takes. A zero-length file is one
    /// empty row, so there is always something to put a cursor on.
    pub const fn rows(self, len: u64) -> u64 {
        if len == 0 {
            return 1;
        }
        len.div_ceil(self.stride())
    }

    /// Which column within its row a byte sits in, 0-based.
    ///
    /// Always below [`MAX_HEX_WIDTH`], so it fits a `u16` by construction.
    pub const fn column_of(self, offset: u64) -> u16 {
        (offset % self.stride()) as u16
    }

    /// Snap an offset down to the start of its row.
    pub const fn snap(self, offset: u64) -> u64 {
        offset.saturating_sub(offset % self.stride())
    }

    /// How wide the offset column has to be to hold every offset in a file of
    /// `len` bytes, in hex. Eight is the floor, so a small file's column does
    /// not jitter as the index grows.
    pub fn offset_column(self, len: Option<u64>) -> usize {
        let digits = len.map_or(8, |l| format!("{l:X}").len());
        digits.max(8)
    }

    /// How wide the *decimal* offset column has to be for a file of `len`
    /// bytes (the "both hex and decimal").
    ///
    /// A file whose size is not yet known gets ten digits, which is what the
    /// eight-hex-digit floor of [`HexLayout::offset_column`] describes: 4 GiB.
    /// Following the size once it is known keeps a small file's rows narrow -
    /// the width the decimal half needs is exactly the reason it is worth
    /// showing at all.
    pub fn decimal_column(self, len: Option<u64>) -> usize {
        len.map_or(10, |l| format!("{l}").len()).max(1)
    }
}

impl Default for HexLayout {
    fn default() -> Self {
        Self::new(16)
    }
}

/// What the ASCII gutter shows for one byte.
///
/// Printable ASCII as itself, everything else as `.`. Deliberately not the
/// active text encoding: the gutter's job is to make a byte's *identity*
/// legible next to its hex, and a gutter that changed under `F8` would stop
/// lining up with the hex it annotates.
pub const fn ascii_glyph(b: u8) -> char {
    if b >= 0x20 && b < 0x7F {
        b as char
    } else {
        '.'
    }
}

/// One row's hex column, as text.
pub fn hex_column(bytes: &[u8], width: u16) -> String {
    value_column(bytes, width, HexConfig::default())
}

/// The byte column, grouped and formatted per.
///
/// At `group = 8` and `format = hex` this is the familiar dump and
/// [`hex_column`] is the shorthand for it. Above one byte, each column is a
/// word read with `cfg.endian`, written as hex digits or as decimal, and
/// padded to the widest value the word size can hold so a column of numbers
/// reads down.
///
/// **A trailing word short of its bytes is rendered from the bytes it has, not
/// padded with zeros it does not have** - padding would be inventing file
/// content. It is written as hex for that column alone, because a partial word
/// has no value to print, and the caller can see it is short from the ASCII
/// gutter.
pub fn value_column(bytes: &[u8], width: u16, cfg: HexConfig) -> String {
    let width = usize::from(width);
    let step = cfg.group.bytes();
    let cell = cell_width(cfg);
    let mut out = String::with_capacity(width.saturating_mul(cell.saturating_add(1)));

    let mut at = 0usize;
    let mut first = true;
    while at < width {
        if !first {
            out.push(' ');
        }
        first = false;
        let end = at.saturating_add(step).min(width);
        match bytes.get(at..end) {
            // A whole word: read and format it.
            Some(word) if word.len() == step => {
                let text = format_word(word, cfg);
                for _ in text.chars().count()..cell {
                    out.push(' ');
                }
                out.push_str(&text);
            }
            // The file ended inside this word. Show the bytes that exist as
            // hex; there is no value here to print.
            Some(word) => {
                let mut text = String::new();
                for b in word {
                    text.push(nibble(b >> 4));
                    text.push(nibble(b & 0x0F));
                }
                for _ in text.chars().count()..cell {
                    out.push(' ');
                }
                out.push_str(&text);
            }
            // Past the end entirely: blanks, so the gutter still lines up.
            None => {
                for _ in 0..cell {
                    out.push(' ');
                }
            }
        }
        at = end.max(at.saturating_add(1));
    }
    out
}

/// How wide the whole byte column is, for `width` bytes at this grouping.
pub const fn grouped_col_width(width: u16, cfg: HexConfig) -> usize {
    let width = width as usize;
    let step = cfg.group.bytes();
    // A width that is not a whole number of words rounds down to one, plus a
    // short trailing column for whatever bytes are left over.
    let whole = width / step;
    let rest = width % step;
    let cols = whole + if rest > 0 { 1 } else { 0 };
    if cols == 0 {
        return 0;
    }
    cols * cell_width(cfg) + (cols - 1)
}

/// How wide one column is, in characters.
pub const fn cell_width(cfg: HexConfig) -> usize {
    let bytes = cfg.group.bytes();
    match cfg.format {
        // Two digits a byte.
        HexFormat::Hex => bytes * 2,
        // The widest decimal the word can hold, plus a sign column.
        HexFormat::Unsigned => match bytes {
            1 => 3,
            2 => 5,
            4 => 10,
            _ => 20,
        },
        HexFormat::Signed => match bytes {
            1 => 4,
            2 => 6,
            4 => 11,
            _ => 20,
        },
    }
}

/// One whole word, read with the configured byte order.
fn format_word(word: &[u8], cfg: HexConfig) -> String {
    let value = word_value(word, cfg.endian);
    match cfg.format {
        HexFormat::Hex => {
            let mut out = String::with_capacity(word.len() * 2);
            // Hex is written most-significant first whatever the byte order:
            // the digits are the *value*, and the order has already been
            // applied in reading it.
            for i in (0..word.len()).rev() {
                let b = ((value >> (i * 8)) & 0xFF) as u8;
                out.push(nibble(b >> 4));
                out.push(nibble(b & 0x0F));
            }
            out
        }
        HexFormat::Unsigned => value.to_string(),
        HexFormat::Signed => signed(value, word.len()).to_string(),
    }
}

/// Reinterpret the low `bytes` of `value` as two's complement.
const fn signed(value: u64, bytes: usize) -> i64 {
    let bits = (bytes as u32) * 8;
    if bits >= 64 {
        return value as i64;
    }
    let shift = 64 - bits;
    ((value << shift) as i64) >> shift
}

/// One row's ASCII gutter, as text.
pub fn ascii_column(bytes: &[u8]) -> String {
    bytes.iter().map(|b| ascii_glyph(*b)).collect()
}

const fn nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => '?',
    }
}

/// Parse a `Ctrl+G` offset ("accepting `0x` notation").
///
/// Accepts `0x1f00`, `1f00h`, `$1f00`, plain decimal, `_` and `,` as digit
/// separators, and a leading `+`. Returns `None` for anything else, which the
/// caller reports rather than guessing at.
pub fn parse_offset(input: &str) -> Option<u64> {
    let raw: String = input
        .trim()
        .chars()
        .filter(|c| *c != '_' && *c != ',' && *c != ' ')
        .collect();
    let raw = raw.strip_prefix('+').unwrap_or(&raw);
    if raw.is_empty() {
        return None;
    }
    let (body, radix) = if let Some(rest) = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .or_else(|| raw.strip_prefix('$'))
    {
        (rest, 16)
    } else if let Some(rest) = raw.strip_suffix('h').or_else(|| raw.strip_suffix('H')) {
        (rest, 16)
    } else {
        (raw, 10)
    };
    if body.is_empty() {
        return None;
    }
    u64::from_str_radix(body, radix).ok()
}

// ------------------------------------------------------- both bases ---------

/// The two spaces between a hex row's columns.
///
/// One would let a run of `ff` bytes touch the gutter; three wastes a column
/// per gap on a mode that is already the widest thing the viewer draws.
const GAP: &str = "  ";

/// Which part of a hex row a piece of text is, so the renderer can colour it
/// (the `viewer.hex_offset` / `viewer.hex_ascii`).
///
/// The row is handed over as pieces rather than as one string precisely because
/// the colours are the renderer's business and the geometry is this module's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HexPart {
    /// The offset, in hex.
    Offset,
    /// The same offset, in decimal (the "both hex and decimal").
    Decimal,
    /// Whitespace between the columns.
    Gap,
    /// The bytes.
    Bytes,
    /// The ASCII gutter.
    Ascii,
}

/// One piece of a laid-out hex row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexPiece {
    /// Which column this is.
    pub part: HexPart,
    /// Its text, already cropped to what the terminal can show.
    pub text: String,
}

/// A hex row's geometry for one terminal width.
///
/// Built once per frame, not once per row, so every row of a screen agrees
/// about how wide the offset column is and about whether the decimal half of
/// it is being shown. Rebuilding it per row would let a file whose length
/// becomes known mid-screen draw two different geometries at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexPlan {
    layout: HexLayout,
    hex_digits: usize,
    dec_digits: Option<usize>,
    cols: usize,
    /// Grouping, format and byte order. Part of the plan
    /// rather than read at paint time, for the same reason the widths are: a
    /// row must not be measured under one setting and drawn under another.
    hex: HexConfig,
}

impl HexPlan {
    /// Plan a row of `layout` bytes for a file of `len` bytes on a terminal
    /// `cols` columns wide.
    ///
    /// The decimal offset column is included only when the whole row -
    /// offsets, bytes and gutter - still fits. That is the "both
    /// hex and decimal" honoured where it is free and dropped where it would
    /// cost the bytes it annotates; the status line shows both unconditionally,
    /// so the information is never actually lost.
    pub fn new(layout: HexLayout, len: Option<u64>, cols: u16) -> Self {
        Self::with_hex(layout, len, cols, HexConfig::default())
    }

    /// [`HexPlan::new`] at a chosen grouping.
    pub fn with_hex(layout: HexLayout, len: Option<u64>, cols: u16, hex: HexConfig) -> Self {
        let hex_digits = layout.offset_column(len);
        let dec_digits = layout.decimal_column(len);
        let cols = usize::from(cols);
        let bytes_and_gutter = GAP
            .len()
            .saturating_add(grouped_col_width(layout.width(), hex))
            .saturating_add(GAP.len())
            .saturating_add(usize::from(layout.width()));
        let with_decimal = hex_digits
            .saturating_add(GAP.len())
            .saturating_add(dec_digits.saturating_add(2))
            .saturating_add(bytes_and_gutter);
        Self {
            layout,
            hex_digits,
            dec_digits: (with_decimal <= cols).then_some(dec_digits),
            cols,
            hex,
        }
    }

    /// The geometry the rows are cut from.
    pub const fn layout(self) -> HexLayout {
        self.layout
    }

    /// How many hex digits the offset column holds.
    pub const fn hex_digits(self) -> usize {
        self.hex_digits
    }

    /// How many decimal digits the offset column holds, or `None` when the
    /// decimal half did not fit and was dropped.
    pub const fn decimal_digits(self) -> Option<usize> {
        self.dec_digits
    }

    /// How many columns the offset column and its trailing gap take - what a
    /// renderer needs to know to reserve the gutter.
    pub const fn gutter_width(self) -> usize {
        let mut w = self.hex_digits;
        if let Some(d) = self.dec_digits {
            // `(` + digits + `)`, after a gap.
            w = w
                .saturating_add(GAP.len())
                .saturating_add(d.saturating_add(2));
        }
        w.saturating_add(GAP.len())
    }

    /// How many columns a full row wants, before any cropping.
    pub const fn full_width(self) -> usize {
        self.gutter_width()
            .saturating_add(grouped_col_width(self.layout.width(), self.hex))
            .saturating_add(GAP.len())
            .saturating_add(self.layout.width() as usize)
    }

    /// True when a full row fits the terminal it was planned for.
    ///
    /// False means the rows are cropped, which is deliberate: the design
    /// fixes the bytes per row at `viewer.hex_width`, so a width the terminal
    /// cannot hold loses its right-hand end rather than re-flowing.
    pub const fn fits(self) -> bool {
        self.full_width() <= self.cols
    }

    /// Lay out one row, cropped to the terminal, as coloured pieces.
    ///
    /// `bytes` is what the window actually held for this row - **fewer than
    /// `width` on the last row of the file, and that is not padded**: the hex
    /// column pads with blanks so the gutter does not move, and the ASCII
    /// gutter simply stops. A phantom `.` for a byte that does not exist would
    /// be a lie about the file's contents.
    pub fn row(self, offset: u64, bytes: &[u8]) -> Vec<HexPiece> {
        let mut out: Vec<HexPiece> = Vec::with_capacity(6);
        let mut budget = self.cols;
        let width = self.layout.width();
        let mut push = |part: HexPart, text: String| {
            if budget == 0 {
                return;
            }
            let text = crop(&text, budget);
            budget = budget.saturating_sub(text.chars().count());
            if !text.is_empty() {
                out.push(HexPiece { part, text });
            }
        };
        push(
            HexPart::Offset,
            format!("{offset:0width$X}", width = self.hex_digits),
        );
        if let Some(digits) = self.dec_digits {
            push(HexPart::Gap, GAP.to_string());
            push(HexPart::Decimal, format!("({offset:>digits$})"));
        }
        push(HexPart::Gap, GAP.to_string());
        push(HexPart::Bytes, value_column(bytes, width, self.hex));
        push(HexPart::Gap, GAP.to_string());
        push(HexPart::Ascii, ascii_column(bytes));
        out
    }

    /// [`HexPlan::row`] as one string, for tests and for anything that does not
    /// paint.
    pub fn row_text(self, offset: u64, bytes: &[u8]) -> String {
        self.row(offset, bytes)
            .into_iter()
            .map(|p| p.text)
            .collect()
    }
}

/// Take at most `columns` characters. Every character a hex row can contain is
/// one column wide - hex digits, spaces, and [`ascii_glyph`]'s printable ASCII
/// or `.` - so this is the crop and there is no width arithmetic to get wrong.
fn crop(text: &str, columns: usize) -> String {
    text.chars().take(columns).collect()
}

// --------------------------------------------------------- the cursor -------

/// Clamp a byte cursor into a file of `len` bytes.
///
/// The last legal cursor is the last *byte*, not one past it. An empty file has
/// no bytes and the cursor sits at 0 anyway - the same reason
/// [`HexLayout::rows`] gives an empty file one row.
pub const fn clamp_cursor(offset: u64, len: Option<u64>) -> u64 {
    match len {
        Some(0) => 0,
        Some(l) if offset >= l => l.saturating_sub(1),
        Some(_) | None => offset,
    }
}

/// Move a byte cursor by `delta` bytes, clamped to the file.
///
/// the design puts "the current offset under the cursor" in the status line,
/// which only means something if the cursor can move *within* a row. This is
/// that arithmetic; it saturates at both ends rather than wrapping.
pub const fn step_cursor(offset: u64, delta: i64, len: Option<u64>) -> u64 {
    let moved = if delta < 0 {
        offset.saturating_sub(delta.unsigned_abs())
    } else {
        offset.saturating_add(delta.unsigned_abs())
    };
    clamp_cursor(moved, len)
}

// ------------------------------------------------------------ Ctrl+G --------

/// Why a `Ctrl+G` offset was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotoError {
    /// Not a number in any notation [`parse_offset`] accepts.
    Unparsable(String),
    /// Past the end of the file.
    ///
    /// **Refused, not clamped.** Silently landing on the last row would answer
    /// a question the user did not ask and hide the typo that produced it, so
    /// the refusal carries the size - which is usually the number they wanted
    /// to know anyway.
    PastEnd {
        /// What was asked for.
        offset: u64,
        /// What the file actually holds.
        len: u64,
    },
}

impl std::fmt::Display for GotoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparsable(raw) => {
                write!(
                    f,
                    "{raw}: not a position (try 0x1f00, 1f00h, 7936, 50% or :500)"
                )
            }
            Self::PastEnd { offset, len } => write!(
                f,
                "0x{offset:X} ({offset}) is past the end: the file is {len} bytes (0x{len:X})"
            ),
        }
    }
}

impl std::error::Error for GotoError {}

/// Where a `Ctrl+G` answer says to go.
///
/// Three forms, because the design names three seeks and only one of them had a
/// key: the "`Ctrl+G` jumps to an offset, accepting `0x` notation", and the
/// "`End` and **percentage seeks** are marked approximate in the status line
/// rather than blocked" - a rule about a seek the user could not ask for until
/// this prompt learned to spell it. The line form comes with it, because a line
/// number is the other thing a reader of a text file knows the position of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GotoTarget {
    /// A byte offset: `0x1f00`, `1f00h`, `$1f00`, `7936`.
    Offset(u64),
    /// A percentage of the file: `50%`.
    Percent(u8),
    /// A 0-based line number, from a 1-based `:500` or `L500`.
    Line(u64),
}

/// Turn what was typed into `Ctrl+G` into a position.
///
/// Legal offsets are the file's bytes: `0..len`, plus 0 in an empty file, which
/// is the one position an empty file has. A length of `None` is a source that
/// has not proven its own size - a forward-only stream that has not reached the
/// end - and nothing can be refused against a size nobody knows, so the offset
/// is accepted and the caller lands where it can (the approximate
/// answer rather than a blocked one).
///
/// A percentage and a line number are **never** refused against the size: both
/// are answered approximately by the viewer while the index is still running,
/// which is the rule the design states for exactly these two seeks.
pub fn resolve_goto(input: &str, len: Option<u64>) -> std::result::Result<GotoTarget, GotoError> {
    let raw = input.trim();
    // A percentage, written the way it is read out: `50%`.
    if let Some(body) = raw.strip_suffix('%')
        && let Some(n) = parse_offset(body)
    {
        return Ok(GotoTarget::Percent(u8::try_from(n.min(100)).unwrap_or(100)));
    }
    // A line number. `:500` is what every editor's go-to-line looks like and
    // `L500` is what a pager calls it; both are 1-based, as printed in the
    // gutter, and line 0 is read as line 1 rather than refused.
    let line = raw
        .strip_prefix(':')
        .or_else(|| raw.strip_prefix('L'))
        .or_else(|| raw.strip_prefix('l'));
    if let Some(body) = line
        && let Some(n) = parse_decimal(body)
    {
        return Ok(GotoTarget::Line(n.saturating_sub(1)));
    }
    let Some(offset) = parse_offset(raw) else {
        return Err(GotoError::Unparsable(raw.to_string()));
    };
    match len {
        Some(len) if offset >= len.max(1) => Err(GotoError::PastEnd { offset, len }),
        Some(_) | None => Ok(GotoTarget::Offset(offset)),
    }
}

/// A plain decimal count, with the same digit separators [`parse_offset`]
/// accepts.
///
/// Deliberately not [`parse_offset`]: `:1000` is line one thousand, and reading
/// a `0x` or a trailing `h` there would make a line number mean an offset.
fn parse_decimal(input: &str) -> Option<u64> {
    let raw: String = input
        .trim()
        .chars()
        .filter(|c| *c != '_' && *c != ',' && *c != ' ')
        .collect();
    let raw = raw.strip_prefix('+').unwrap_or(&raw);
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

// ------------------------------------------------------ the initial mode ----

/// Which mode a freshly opened file starts in.
///
/// > Mode is remembered per session and `viewer.default_mode` sets the initial
/// > one. A file detected as binary opens in hex automatically unless
/// > overridden.
///
/// Read as a precedence, outermost first:
///
/// 1. **A binary file opens in hex.** This wins, because the alternative is a
///    screenful of replacement glyphs and the `1` key is the override - one
///    keystroke, and per file rather than for the session.
/// 2. **The mode remembered from this session**, which is whatever the user
///    last chose with `1` / `2` / `F4`. "Per session" is read as the
///    application session, not per file: a user who switched to hex once means
///    it for the next file too, and a viewer that forgot between files would
///    make the sentence say nothing.
/// 3. **`viewer.default_mode`**, before anything has been chosen.
pub const fn initial_mode(
    default_mode: ViewerMode,
    remembered: Option<ViewerMode>,
    binary: bool,
) -> ViewerMode {
    if binary {
        return ViewerMode::Hex;
    }
    match remembered {
        Some(mode) => mode,
        None => default_mode,
    }
}

// ------------------------------------------------------ the two sides -------
//
// the design gives a hex row two sides and `Tab` between them:
//
// > `Tab` switches focus between the bytes and the characters. Selecting five
// > bytes on the left and pressing `Tab` leaves five characters selected on the
// > right.
//
// The switch itself is one field on the viewer and one enum here
// ([`super::select::HexSide`]); what makes the two sides *different* is the
// unit one press moves by, and that is the geometry below. The bytes side moves
// a whole column, so a selection made there is word-aligned by construction;
// the characters side moves one character of the active encoding, which under a
// multi-byte encoding may be several bytes and may therefore stop half way
// through a word. Both are pure functions of an offset, a configuration and -
// for the characters side - the window the caller already read.

/// Where byte `index` of a row is written in [`value_column`]'s output, as a
/// **character** range.
///
/// The one mapping from a byte to the characters that byte is drawn as, at any
/// grouping, format and byte order. It is what lets a match, and a selection,
/// be painted in the right place without the renderer doing arithmetic of its
/// own: "three characters per byte" is right only at `group = 8`.
///
/// * `format = hex`: the byte's own two digits inside its word's cell,
///   **honouring `endian`**. A little-endian word writes byte 0 last, so byte
///   0's digits are the cell's last two. That is what makes the design's
///   "partly-covered columns partly highlighted" exact rather than approximate.
/// * `format = unsigned | signed`: a decimal cell has no digits belonging to
///   individual bytes, so the whole cell comes back and a partial cover paints
///   the cell whole. A value cannot be half-printed for a word only half held.
///
/// `None` when `index` is past `width`.
///
/// # The last row of a file
///
/// A cell's character position does not depend on `width` at all, only on the
/// grouping; `width` decides which cells are *whole*. [`value_column`] writes a
/// column it has only some of the bytes for as those bytes' own digits in file
/// order, so a caller laying out the file's short final row passes the number
/// of bytes that row actually holds and gets the same answer the row was drawn
/// with.
pub fn value_span(index: usize, width: u16, cfg: HexConfig) -> Option<std::ops::Range<usize>> {
    let width = usize::from(width);
    if index >= width {
        return None;
    }
    let step = cfg.group.bytes();
    if step == 0 {
        return None;
    }
    let cell = cell_width(cfg);
    // Cells are `cell` characters with one space between them, at every width.
    let column = index / step;
    let at = column.saturating_mul(step);
    let start = column.saturating_mul(cell.saturating_add(1));
    match cfg.format {
        // The whole cell, because there is nothing smaller to point at.
        HexFormat::Unsigned | HexFormat::Signed => Some(start..start.saturating_add(cell)),
        HexFormat::Hex => {
            let held = step.min(width.saturating_sub(at));
            let within = index.saturating_sub(at);
            // Which pair of digits this byte is, counting from the left of the
            // digits actually written.
            let pair = if held == step {
                match cfg.endian {
                    // The digits are the word's value, most significant first,
                    // so a little-endian word writes byte 0 last.
                    Endian::Little => held.saturating_sub(1).saturating_sub(within),
                    Endian::Big => within,
                }
            } else {
                // A short trailing column has no value to read, so
                // `value_column` writes its bytes in file order.
                within
            };
            // Values are right-aligned in their cell.
            let pad = cell.saturating_sub(held.saturating_mul(2));
            let from = start
                .saturating_add(pad)
                .saturating_add(pair.saturating_mul(2));
            Some(from..from.saturating_add(2))
        }
    }
}

/// [`value_span`] over a run of bytes, merged into the fewest ranges.
///
///
/// Sorted, because a little-endian word numbers its digits backwards and a
/// selection running forwards through it therefore produces character ranges
/// running back; and merged, because touching ranges painted separately are
/// three attribute runs where one will do.
pub fn value_spans(
    range: std::ops::Range<usize>,
    width: u16,
    cfg: HexConfig,
) -> Vec<std::ops::Range<usize>> {
    let hi = range.end.min(usize::from(width));
    let mut spans: Vec<std::ops::Range<usize>> = (range.start..hi)
        .filter_map(|index| value_span(index, width, cfg))
        .collect();
    spans.sort_by_key(|span| span.start);
    let mut out: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            // Touching or overlapping: one run.
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            Some(_) | None => out.push(span),
        }
    }
    out
}

/// One press on the bytes side: a whole column, snapped to the grouping.
///
///
/// > One press is one column - one byte at `group = 8`, one *word* above it.
///
/// Saturating at both ends, and never landing inside a word, so a selection
/// made entirely on this side is word-aligned without anyone having to think
/// about it ([`super::select::word_aligned`]).
///
/// A cursor that arrived from the characters side can be inside a word. Moving
/// back from there lands on that word's own start rather than skipping past it,
/// which is the reading that keeps `Left` then `Right` where it began.
pub const fn column_step(offset: u64, delta: i64, cfg: HexConfig, len: Option<u64>) -> u64 {
    let step = cfg.group.bytes() as u64;
    if step == 0 {
        return clamp_cursor(offset, len);
    }
    let base = offset.saturating_sub(offset % step);
    let magnitude = delta.unsigned_abs().saturating_mul(step);
    let moved = if delta >= 0 {
        base.saturating_add(magnitude)
    } else {
        // An offset inside a word spends the first press reaching its start.
        let back = if offset == base {
            magnitude
        } else {
            magnitude.saturating_sub(step)
        };
        base.saturating_sub(back)
    };
    // The file's last word may be short; its start is still a word boundary,
    // and it is where the cursor belongs.
    let capped = clamp_cursor(moved, len);
    capped.saturating_sub(capped % step)
}

/// The bytes a character step is measured in.
///
/// The characters side moves by "one character, which under a multi-byte
/// encoding may be several bytes", and how many bytes that is can only be
/// answered by looking at the bytes. This is the window the caller already read
/// for the frame, so a step costs no read of its own and the rule
/// holds: the cursor's memory is the window's, never the file's.
#[derive(Debug, Clone, Copy)]
pub struct CharWindow<'a> {
    /// Which encoding the characters are read with.
    pub enc: TextEncoding,
    /// The window's bytes.
    pub bytes: &'a [u8],
    /// The file offset `bytes` begins at.
    pub start: u64,
}

impl CharWindow<'_> {
    /// A window with nothing in it, for a caller that has not read one.
    ///
    /// A step through it falls back to a byte step, which is the honest answer
    /// when there are no bytes to measure a character with.
    pub const fn empty() -> Self {
        Self {
            enc: TextEncoding::UTF8,
            bytes: &[],
            start: 0,
        }
    }
}

/// One press on the focused hex side.
///
/// The two sides are two views of one cursor, so this is one function and not
/// two cursors: `side` chooses the unit, and everything else about the movement
/// is the same. The vertical moves are not here at all, because one row is
/// `hex_width` bytes whichever side has focus - a row is a row.
///
/// * [`HexSide::Bytes`]: [`column_step`], a whole column.
/// * [`HexSide::Chars`]: one character of `chars.enc`, so five presses over
///   UTF-8 text can cover more than five bytes. The byte count in the status
///   line is what says how many.
pub fn side_step(
    side: HexSide,
    chars: CharWindow<'_>,
    cfg: HexConfig,
    len: Option<u64>,
    offset: u64,
    delta: i64,
) -> u64 {
    match side {
        HexSide::Bytes => column_step(offset, delta, cfg, len),
        HexSide::Chars => char_step(chars, offset, delta, len),
    }
}

/// [`side_step`]'s characters half.
///
/// Window-local by construction: a cursor outside the window, or a window that
/// stops at the cursor, has no character to measure, and the answer is then a
/// single byte step rather than a read. The caller lays out again and the next
/// press measures properly, which is the same bargain the rest of the viewer
/// makes with its window.
fn char_step(chars: CharWindow<'_>, offset: u64, delta: i64, len: Option<u64>) -> u64 {
    let Some(local) = offset.checked_sub(chars.start) else {
        return step_cursor(offset, delta, len);
    };
    let Ok(mut at) = usize::try_from(local) else {
        return step_cursor(offset, delta, len);
    };
    if at > chars.bytes.len() {
        return step_cursor(offset, delta, len);
    }
    let mut left = delta.unsigned_abs();
    if delta >= 0 {
        while left > 0 && at < chars.bytes.len() {
            // `next_char` is at least 1, so this cannot fail to advance.
            at = at
                .saturating_add(encoding::next_char(chars.enc, chars.bytes, at))
                .min(chars.bytes.len());
            left = left.saturating_sub(1);
        }
    } else {
        while left > 0 && at > 0 {
            // `prev_char` is at most `before`, so this cannot fail to retreat.
            at = encoding::prev_char(chars.enc, chars.bytes, at).min(at.saturating_sub(1));
            left = left.saturating_sub(1);
        }
    }
    clamp_cursor(chars.start.saturating_add(at as u64), len)
}

/// Up to eight bytes read as one unsigned value, with the given byte order.
///
/// The one place the byte order is applied, so a column, a match highlight and
/// an interpretation cannot disagree about which end a word starts at.
fn word_value(bytes: &[u8], endian: Endian) -> u64 {
    let mut value: u64 = 0;
    match endian {
        Endian::Little => {
            for (i, b) in bytes.iter().take(8).enumerate() {
                value |= u64::from(*b) << (i * 8);
            }
        }
        Endian::Big => {
            for b in bytes.iter().take(8) {
                value = (value << 8) | u64::from(*b);
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HexGroup, ViewerConfig};
    use crate::viewer::select::word_aligned;
    use crate::viewer::source::{Opener, Stream};
    use crate::viewer::{Viewer, ViewerId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    #[test]
    fn a_width_is_always_renderable() {
        assert_eq!(HexLayout::new(0).width(), 16, "never divides by zero");
        assert_eq!(HexLayout::new(9_999).width(), MAX_HEX_WIDTH);
        assert_eq!(HexLayout::new(8).width(), 8);
        assert_eq!(HexLayout::default().width(), 16);
    }

    #[test]
    fn rows_and_offsets_are_arithmetic_and_nothing_else() {
        let l = HexLayout::new(16);
        assert_eq!(l.row_offset(0), 0);
        assert_eq!(l.row_offset(1_000_000_000), 16_000_000_000);
        assert_eq!(l.row_of(31), 1);
        assert_eq!(l.snap(31), 16);
        assert_eq!(l.rows(0), 1, "an empty file still has a row");
        assert_eq!(l.rows(1), 1);
        assert_eq!(l.rows(16), 1);
        assert_eq!(l.rows(17), 2);
        // A 40 GB file is one multiplication, not one read.
        assert_eq!(l.rows(40 * 1024 * 1024 * 1024), 2_684_354_560);
    }

    #[test]
    fn row_arithmetic_saturates_rather_than_wrapping() {
        let l = HexLayout::new(16);
        assert_eq!(l.row_offset(u64::MAX), u64::MAX);
        assert_eq!(l.snap(0), 0);
    }

    #[test]
    fn a_short_last_row_keeps_the_columns_aligned() {
        assert_eq!(hex_column(b"\x00\xff", 4), "00 ff      ");
        assert_eq!(hex_column(b"\x00\xff", 2), "00 ff");
        assert_eq!(ascii_column(b"ab\x00~\x7f"), "ab.~.");
    }

    #[test]
    fn the_offset_column_never_narrows_below_eight() {
        let l = HexLayout::new(16);
        assert_eq!(l.offset_column(Some(10)), 8);
        assert_eq!(l.offset_column(None), 8);
        assert_eq!(l.offset_column(Some(0x1_0000_0000)), 9);
    }

    #[test]
    fn ctrl_g_accepts_the_notations_a_person_actually_types() {
        assert_eq!(parse_offset("0x1f"), Some(31));
        assert_eq!(parse_offset("0X1F"), Some(31));
        assert_eq!(parse_offset("$1f"), Some(31));
        assert_eq!(parse_offset("1Fh"), Some(31));
        assert_eq!(parse_offset("31"), Some(31));
        assert_eq!(parse_offset(" 1_000_000 "), Some(1_000_000));
        assert_eq!(parse_offset("1,024"), Some(1_024));
        assert_eq!(parse_offset("+8"), Some(8));
        assert_eq!(parse_offset(""), None);
        assert_eq!(parse_offset("0x"), None);
        assert_eq!(parse_offset("nope"), None);
        assert_eq!(parse_offset("-1"), None);
        assert_eq!(parse_offset("99999999999999999999999"), None);
    }

    // ------------------------------------------------------- both bases -----

    #[test]
    fn a_row_shows_both_bases_when_they_fit_and_only_hex_when_they_do_not() {
        // "Byte offsets are shown in both hex and decimal".
        let l = HexLayout::new(16);
        let wide = HexPlan::new(l, Some(4096), 120);
        assert_eq!(wide.decimal_digits(), Some(4), "4096 is four digits");
        let row = wide.row_text(0x2F, b"hi");
        assert!(row.starts_with("0000002F  (  47)"), "{row}");

        // 80 columns cannot hold 16 bytes, a gutter and two offset columns, so
        // the decimal half is dropped rather than the bytes.
        let narrow = HexPlan::new(l, Some(4096), 80);
        assert_eq!(narrow.decimal_digits(), None);
        let row = narrow.row_text(0x2F, b"hi");
        assert!(row.starts_with("0000002F  68 69"), "{row}");
        assert!(narrow.fits(), "80 columns still fit a plain row: {row}");
    }

    #[test]
    fn the_decimal_column_never_changes_width_between_rows_of_one_screen() {
        // One plan per frame is the whole point: two rows of the same screen
        // cannot disagree about the geometry.
        let plan = HexPlan::new(HexLayout::new(16), Some(1_000_000), 200);
        let a = plan.row_text(0, b"a");
        let b = plan.row_text(999_999, b"b");
        let head = |s: &str| s.chars().take(plan.gutter_width()).collect::<String>();
        assert_eq!(head(&a).chars().count(), head(&b).chars().count());
        assert!(head(&a).contains("(      0)"), "{a}");
        assert!(head(&b).contains("( 999999)"), "{b}");
    }

    // -------------------------------------------------- the partial row -----

    #[test]
    fn the_last_row_pads_the_hex_but_never_the_gutter() {
        // The hex column keeps its width so the gutter does not
        // walk left on the last row; the gutter itself shows the bytes that
        // exist and stops.
        let plan = HexPlan::new(HexLayout::new(8), Some(19), 120);
        let full = plan.row_text(0, b"abcdefgh");
        let last = plan.row_text(16, b"stu");
        assert!(full.ends_with("abcdefgh"), "{full}");
        assert!(last.ends_with("stu"), "no phantom bytes: {last}");
        assert_eq!(
            full.find("abcdefgh"),
            last.find("stu"),
            "the gutter starts in the same column on both rows:\n{full}\n{last}"
        );
        assert!(
            !last.contains("stu."),
            "a missing byte is not a dot: {last}"
        );
    }

    #[test]
    fn an_empty_last_row_is_offsets_and_blanks_and_not_a_panic() {
        let plan = HexPlan::new(HexLayout::new(4), Some(8), 60);
        let row = plan.row_text(8, &[]);
        assert!(row.starts_with("00000008"), "{row}");
        assert_eq!(row.trim_end(), "00000008  (8)", "{row}");
    }

    // ------------------------------------------------------- the crop -------

    #[test]
    fn a_row_wider_than_the_terminal_is_cropped_and_never_wrapped() {
        // the design fixes the bytes per row at `viewer.hex_width`, so a
        // width the terminal cannot hold loses its right-hand end.
        let plan = HexPlan::new(HexLayout::new(MAX_HEX_WIDTH), Some(1024), 40);
        assert!(!plan.fits());
        let row = plan.row_text(0, &[0xAB; 64]);
        assert_eq!(row.chars().count(), 40, "cropped to the terminal: {row}");
        assert!(!row.contains('\n'), "cropped, not wrapped: {row}");
    }

    #[test]
    fn no_width_and_no_terminal_produces_a_row_that_does_not_fit_or_panics() {
        // Every `viewer.hex_width` against every plausible terminal, including
        // the ones that cannot hold a single column.
        for width in 1..=MAX_HEX_WIDTH {
            let layout = HexLayout::new(width);
            for cols in [0_u16, 1, 2, 7, 8, 9, 40, 60, 80, 120, 200, u16::MAX] {
                for len in [None, Some(0), Some(1), Some(u64::MAX)] {
                    let plan = HexPlan::new(layout, len, cols);
                    let bytes: Vec<u8> = (0..width).map(|b| b as u8).collect();
                    for row in [
                        plan.row_text(0, &bytes),
                        plan.row_text(u64::MAX, &bytes),
                        plan.row_text(7, &[]),
                        plan.row_text(7, &bytes[..bytes.len() / 2]),
                    ] {
                        assert!(
                            row.chars().count() <= usize::from(cols),
                            "width {width} cols {cols}: {row:?}"
                        );
                        assert!(!row.contains('\n'), "{row:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_zero_column_terminal_produces_nothing_rather_than_a_panic() {
        let plan = HexPlan::new(HexLayout::default(), Some(64), 0);
        assert!(plan.row(0, b"abc").is_empty());
        assert_eq!(plan.row_text(0, b"abc"), "");
    }

    #[test]
    fn the_pieces_carry_what_to_colour_and_join_back_into_the_row() {
        let plan = HexPlan::new(HexLayout::new(4), Some(64), 80);
        let pieces = plan.row(0, b"Hi\n\x00");
        let parts: Vec<HexPart> = pieces.iter().map(|p| p.part).collect();
        assert_eq!(
            parts,
            vec![
                HexPart::Offset,
                HexPart::Gap,
                HexPart::Decimal,
                HexPart::Gap,
                HexPart::Bytes,
                HexPart::Gap,
                HexPart::Ascii,
            ]
        );
        let joined: String = pieces.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(joined, plan.row_text(0, b"Hi\n\x00"));
        assert!(joined.ends_with("Hi.."), "{joined}");
    }

    // ------------------------------------------------------- the cursor -----

    #[test]
    fn the_cursor_moves_within_a_row_and_stops_at_both_ends() {
        // the "the current offset under the cursor" only means
        // something if the cursor can move by one byte.
        let l = HexLayout::new(16);
        assert_eq!(step_cursor(0, 1, Some(64)), 1);
        assert_eq!(l.column_of(17), 1);
        assert_eq!(step_cursor(0, -1, Some(64)), 0, "the file starts at 0");
        assert_eq!(step_cursor(63, 1, Some(64)), 63, "the last byte is the end");
        assert_eq!(step_cursor(10, 0, Some(64)), 10);
        assert_eq!(step_cursor(10, i64::MIN, Some(64)), 0, "no wrap");
        assert_eq!(step_cursor(10, i64::MAX, Some(64)), 63, "no wrap");
        assert_eq!(step_cursor(u64::MAX, 5, None), u64::MAX, "no wrap");
        assert_eq!(clamp_cursor(9, Some(0)), 0, "an empty file has one place");
        assert_eq!(clamp_cursor(9, None), 9, "an unknown size clamps nothing");
    }

    // -------------------------------------------------------- Ctrl+G --------

    #[test]
    fn ctrl_g_refuses_a_past_the_end_offset_with_the_size_rather_than_clamping() {
        // Landing silently on the last row would hide the typo
        // and answer a question nobody asked.
        let err = resolve_goto("0x2000", Some(4096)).expect_err("past the end");
        assert_eq!(
            err,
            GotoError::PastEnd {
                offset: 0x2000,
                len: 4096
            }
        );
        let said = err.to_string();
        assert!(said.contains("4096"), "the size, in decimal: {said}");
        assert!(said.contains("0x1000"), "and in hex: {said}");

        assert_eq!(
            resolve_goto("0xFFF", Some(4096)),
            Ok(GotoTarget::Offset(0xFFF)),
            "the last byte"
        );
        assert_eq!(
            resolve_goto("4096", Some(4096)),
            Err(GotoError::PastEnd {
                offset: 4096,
                len: 4096
            }),
            "one past the last byte is past the end"
        );
        assert_eq!(
            resolve_goto("0", Some(0)),
            Ok(GotoTarget::Offset(0)),
            "an empty file has 0"
        );
        assert_eq!(
            resolve_goto("1", Some(0)),
            Err(GotoError::PastEnd { offset: 1, len: 0 })
        );
    }

    #[test]
    fn ctrl_g_cannot_refuse_against_a_size_nobody_knows() {
        // A forward-only stream that has not reached its end has no size to
        // refuse against, and the design answers approximately rather than
        // blocking.
        assert_eq!(resolve_goto("0x1f", None), Ok(GotoTarget::Offset(31)));
    }

    #[test]
    fn ctrl_g_takes_the_percentage_and_line_seeks_spec_10_1_promises() {
        // the design states the honesty rule for "`End` and percentage
        // seeks", and the viewer's own help page repeats it - so there has to
        // be a way to ask for one.
        assert_eq!(resolve_goto("50%", Some(4096)), Ok(GotoTarget::Percent(50)));
        assert_eq!(resolve_goto(" 100 % ", None), Ok(GotoTarget::Percent(100)));
        assert_eq!(
            resolve_goto("400%", Some(10)),
            Ok(GotoTarget::Percent(100)),
            "a percentage is clamped, not refused"
        );
        // 1-based in, 0-based out, because the gutter prints 1-based.
        assert_eq!(resolve_goto(":500", Some(10)), Ok(GotoTarget::Line(499)));
        assert_eq!(resolve_goto("L500", None), Ok(GotoTarget::Line(499)));
        assert_eq!(resolve_goto("l1", None), Ok(GotoTarget::Line(0)));
        assert_eq!(resolve_goto(":0", None), Ok(GotoTarget::Line(0)));
        // Neither is refused against the size: both are answered approximately
        // while the index is still running.
        assert_eq!(resolve_goto(":9000", Some(4)), Ok(GotoTarget::Line(8999)));
        // A line number is decimal. `:1000` is line one thousand.
        assert_eq!(resolve_goto(":1000", None), Ok(GotoTarget::Line(999)));
        assert!(resolve_goto(":0x10", None).is_err());
        assert!(resolve_goto("%", None).is_err());
    }

    #[test]
    fn an_unparsable_offset_says_so_and_suggests_the_notations() {
        let err = resolve_goto(" nonsense ", Some(10)).expect_err("refused");
        let said = err.to_string();
        assert!(said.starts_with("nonsense: not a position"), "{said}");
        assert!(said.contains("0x1f00"), "{said}");
    }

    // ---------------------------------------------------- the initial mode --

    #[test]
    fn a_binary_file_opens_in_hex_and_the_session_is_remembered_otherwise() {
        use ViewerMode::{Hex, Text};
        // "A file detected as binary opens in hex automatically."
        assert_eq!(initial_mode(Text, None, true), Hex);
        assert_eq!(initial_mode(Text, Some(Text), true), Hex, "hex still wins");
        // "Mode is remembered per session and `viewer.default_mode` sets the
        // initial one."
        assert_eq!(initial_mode(Text, Some(Hex), false), Hex);
        assert_eq!(initial_mode(Hex, Some(Text), false), Text);
        assert_eq!(initial_mode(Hex, None, false), Hex);
        assert_eq!(initial_mode(Text, None, false), Text);
    }

    // ------------------------------------------------------ the streaming ---

    /// A file that does not exist, of any size, that counts what is read from
    /// it. The only way to assert the memory rule about a 40 GB
    /// file without having 40 GB.
    struct Sparse {
        pos: u64,
        len: u64,
        read: Arc<AtomicU64>,
    }

    impl std::io::Read for Sparse {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = self.len.saturating_sub(self.pos);
            let n = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
            for (i, slot) in buf.iter_mut().take(n).enumerate() {
                // Contains NULs, so this is a binary file - which is the other
                // half of what this fixture is for.
                *slot = (self.pos.saturating_add(i as u64) % 251) as u8;
            }
            self.pos = self.pos.saturating_add(n as u64);
            self.read.fetch_add(n as u64, AtomicOrdering::Relaxed);
            Ok(n)
        }
    }

    impl std::io::Seek for Sparse {
        fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
            use std::io::SeekFrom;
            self.pos = match from {
                SeekFrom::Start(at) => at,
                SeekFrom::End(d) => {
                    if d < 0 {
                        self.len.saturating_sub(d.unsigned_abs())
                    } else {
                        self.len.saturating_add(d.unsigned_abs())
                    }
                }
                SeekFrom::Current(d) => {
                    if d < 0 {
                        self.pos.saturating_sub(d.unsigned_abs())
                    } else {
                        self.pos.saturating_add(d.unsigned_abs())
                    }
                }
            };
            Ok(self.pos)
        }
    }

    fn sparse_opener(len: u64, read: &Arc<AtomicU64>) -> Opener {
        let read = Arc::clone(read);
        Arc::new(move || {
            Ok(Stream::Seekable(Box::new(Sparse {
                pos: 0,
                len,
                read: Arc::clone(&read),
            })))
        })
    }

    const FORTY_GB: u64 = 40 * 1024 * 1024 * 1024;

    #[test]
    fn a_forty_gigabyte_binary_file_opens_in_hex_having_read_one_window() {
        // "A 40 GB file must open as fast as a 4 KB one": "A file
        // detected as binary opens in hex automatically".
        let read = Arc::new(AtomicU64::new(0));
        let cfg = ViewerConfig::default();
        let v = Viewer::open(
            ViewerId(1),
            "huge.bin",
            None,
            sparse_opener(FORTY_GB, &read),
            Some(FORTY_GB),
            &cfg,
        )
        .expect("open");
        assert_eq!(v.mode(), ViewerMode::Hex, "binary opens in hex");
        assert_eq!(v.len(), Some(FORTY_GB));
        let bytes = read.load(AtomicOrdering::Relaxed);
        assert!(
            bytes <= 64 * 1024,
            "opening read {bytes} bytes; it may only read the detection prefix"
        );
    }

    #[test]
    fn hex_mode_reads_the_visible_window_and_nothing_else() {
        // "Same streaming path: seek to `row * width`, read the
        // window, render it." The assertion is on the *bytes read*, because
        // that is the property, and it holds at the far end of a 40 GB file
        // exactly as it does at the start.
        let read = Arc::new(AtomicU64::new(0));
        let cfg = ViewerConfig::default();
        let mut v = Viewer::open(
            ViewerId(2),
            "huge.bin",
            None,
            sparse_opener(FORTY_GB, &read),
            Some(FORTY_GB),
            &cfg,
        )
        .expect("open");
        v.set_mode(ViewerMode::Hex).expect("mode");

        let rows = 40_u16;
        let per_screen = u64::from(rows).saturating_mul(v.hex().stride());
        for at in [0_u64, 1, FORTY_GB - 3, FORTY_GB / 2 + 7] {
            read.store(0, AtomicOrdering::Relaxed);
            v.goto_offset(at).expect("goto");
            v.layout(rows, 200).expect("layout");
            let bytes = read.load(AtomicOrdering::Relaxed);
            assert!(
                bytes <= per_screen,
                "a screen of {rows} rows read {bytes} bytes at offset {at}; \
                 the window is {per_screen}"
            );
            assert!(!v.rows().is_empty(), "something was laid out at {at}");
        }
    }

    #[test]
    fn the_last_screen_of_a_huge_file_stops_at_the_end_without_phantom_bytes() {
        let read = Arc::new(AtomicU64::new(0));
        let cfg = ViewerConfig::default();
        let len = 4096_u64 + 3;
        let mut v = Viewer::open(
            ViewerId(3),
            "odd.bin",
            None,
            sparse_opener(len, &read),
            Some(len),
            &cfg,
        )
        .expect("open");
        v.set_mode(ViewerMode::Hex).expect("mode");
        v.layout(10, 200).expect("layout");
        v.goto_end().expect("end");
        v.layout(10, 200).expect("layout");
        let last = v.rows().last().expect("a row");
        let crate::viewer::Row::Hex { offset, bytes, .. } = last else {
            panic!("hex mode lays out hex rows");
        };
        assert_eq!(*offset, 4096, "the last row starts on a row boundary");
        assert_eq!(bytes.len(), 3, "and holds only the bytes that exist");
        let plan = HexPlan::new(v.hex(), Some(len), 120);
        let row = plan.row_text(*offset, bytes);
        assert!(row.ends_with(&ascii_column(bytes)), "{row}");
        assert_eq!(
            row.chars().filter(|c| *c == '.').count(),
            ascii_column(bytes).chars().filter(|c| *c == '.').count(),
            "no phantom byte reached the gutter: {row}"
        );
    }

    // -------------------------------------------------- the two sides -------

    /// Every grouping, so a test can say "at every group setting" and mean it.
    const GROUPS: [HexGroup; 4] = [
        HexGroup::Bits8,
        HexGroup::Bits16,
        HexGroup::Bits32,
        HexGroup::Bits64,
    ];

    fn hex_at(group: HexGroup, format: HexFormat, endian: Endian) -> HexConfig {
        HexConfig {
            group,
            format,
            endian,
        }
    }

    #[test]
    fn one_press_on_the_bytes_side_is_one_column_at_every_group_setting() {
        for group in GROUPS {
            let step = group.bytes() as u64;
            for format in [HexFormat::Hex, HexFormat::Unsigned, HexFormat::Signed] {
                for endian in [Endian::Little, Endian::Big] {
                    let cfg = hex_at(group, format, endian);
                    assert_eq!(
                        column_step(0, 1, cfg, None),
                        step,
                        "one press is one column at {group:?}"
                    );
                    assert_eq!(column_step(0, 3, cfg, None), step * 3);
                    assert_eq!(column_step(step * 4, -1, cfg, None), step * 3);
                    assert_eq!(column_step(0, -1, cfg, None), 0, "saturates at the start");
                    assert_eq!(column_step(0, 0, cfg, None), 0);
                    // The unit is the grouping and nothing else: the format and
                    // the byte order change how a column is *written*, not how
                    // many bytes it is.
                    assert_eq!(column_step(step * 2, 1, cfg, None), step * 3);
                }
            }
        }
    }

    #[test]
    fn a_press_from_inside_a_word_lands_on_the_word_rather_than_past_it() {
        // Arriving from the characters side is the only way to be here, and
        // going back should reach the word the cursor is in.
        let cfg = hex_at(HexGroup::Bits32, HexFormat::Hex, Endian::Little);
        assert_eq!(column_step(5, -1, cfg, None), 4);
        assert_eq!(column_step(5, 1, cfg, None), 8);
        assert_eq!(column_step(5, -2, cfg, None), 0);
        assert_eq!(column_step(4, -1, cfg, None), 0);
    }

    #[test]
    fn a_width_that_is_not_a_whole_number_of_words_is_rounded_down_to_one() {
        // A row that is a whole number of columns is what
        // makes a row's own cells and the file's word boundaries the same
        // boundaries, which is what `select::word_aligned` measures.
        for group in GROUPS {
            let cfg = hex_at(group, HexFormat::Hex, Endian::Little);
            let step = group.bytes() as u16;
            for width in 1..=MAX_HEX_WIDTH {
                let got = HexLayout::grouped(width, cfg).width();
                assert!(
                    got.is_multiple_of(step),
                    "{width} at {group:?} came out {got}, which is not whole columns"
                );
                assert!(got <= width.max(step), "{width} at {group:?} grew to {got}");
                assert!(got >= step, "{width} at {group:?} left no column at all");
            }
        }
        // The default width is a whole number of columns at every grouping, so
        // an unconfigured viewer never sees the rounding at all.
        for group in GROUPS {
            let cfg = hex_at(group, HexFormat::Hex, Endian::Little);
            assert_eq!(HexLayout::grouped(16, cfg).width(), 16);
        }
        let words = hex_at(HexGroup::Bits64, HexFormat::Hex, Endian::Little);
        assert_eq!(HexLayout::grouped(12, words).width(), 8);
        assert_eq!(HexLayout::grouped(2, words).width(), 8, "never below one");
    }

    #[test]
    fn moving_on_the_bytes_side_cannot_land_inside_a_word() {
        // the design invariant 9: any sequence of movements
        // confined to the bytes side leaves the cursor word-aligned.
        let len = Some(1_000u64);
        for group in GROUPS {
            let cfg = hex_at(group, HexFormat::Hex, Endian::Big);
            let step = group.bytes() as u64;
            let mut at = 3;
            for delta in [
                1,
                1,
                -1,
                9,
                -100,
                7,
                2,
                -1,
                1_000_000,
                -3,
                i64::MIN,
                i64::MAX,
            ] {
                at = column_step(at, delta, cfg, len);
                assert!(
                    at.is_multiple_of(step),
                    "{at} is inside a word at {group:?}"
                );
                assert!(at < 1_000, "{at} is off the file");
                assert!(
                    word_aligned(0, at, len, cfg),
                    "a selection from 0 to {at} is aligned at {group:?}"
                );
            }
        }
    }

    #[test]
    fn value_span_points_at_the_digits_the_column_actually_wrote() {
        let bytes: Vec<u8> = (0u8..16)
            .map(|i| i.wrapping_mul(17).wrapping_add(3))
            .collect();
        for group in GROUPS {
            for endian in [Endian::Little, Endian::Big] {
                let cfg = hex_at(group, HexFormat::Hex, endian);
                let text = value_column(&bytes, 16, cfg);
                for (i, b) in bytes.iter().enumerate() {
                    let span = value_span(i, 16, cfg).expect("a byte inside the row");
                    let want = format!("{b:02x}");
                    assert_eq!(
                        text.get(span.clone()).map(str::to_string),
                        Some(want),
                        "byte {i} at {group:?} {endian:?} in {text:?}"
                    );
                }
                assert_eq!(value_span(16, 16, cfg), None, "past the row");
                assert_eq!(value_span(usize::MAX, 16, cfg), None);
            }
        }
    }

    #[test]
    fn a_decimal_cell_has_no_per_byte_digits_and_comes_back_whole() {
        // a value cannot be half-printed for a
        // word only half held, so the cell is the answer for every byte in it.
        let bytes: Vec<u8> = (0u8..16).collect();
        for format in [HexFormat::Unsigned, HexFormat::Signed] {
            let cfg = hex_at(HexGroup::Bits32, format, Endian::Little);
            let text = value_column(&bytes, 16, cfg);
            let first = value_span(0, 16, cfg).expect("the first cell");
            for i in 0..4 {
                assert_eq!(value_span(i, 16, cfg), Some(first.clone()), "byte {i}");
            }
            let cell = text.get(first).expect("the cell is in the row");
            assert_eq!(cell.chars().count(), cell_width(cfg));
            assert_eq!(cell.trim(), format_word(&bytes[0..4], cfg));
        }
    }

    #[test]
    fn a_short_trailing_column_is_pointed_at_in_file_order() {
        // A width that is not a whole number of words ends in a column with no
        // value to read, and `value_column` writes its bytes as they are.
        let cfg = hex_at(HexGroup::Bits32, HexFormat::Hex, Endian::Little);
        let bytes: Vec<u8> = vec![0x11, 0x22, 0x33, 0x44, 0xAA, 0xBB];
        let text = value_column(&bytes, 6, cfg);
        for (i, b) in bytes.iter().enumerate() {
            let span = value_span(i, 6, cfg).expect("a byte inside the row");
            let want = format!("{b:02x}");
            assert_eq!(text.get(span).map(str::to_string), Some(want), "byte {i}");
        }
    }

    #[test]
    fn value_spans_merge_into_the_fewest_ranges() {
        let cfg = hex_at(HexGroup::Bits32, HexFormat::Hex, Endian::Little);
        // A little-endian word writes byte 0 last, so a run forwards through
        // the bytes is a run backwards through the digits, and it still comes
        // back as one range.
        assert_eq!(value_spans(0..2, 16, cfg), vec![4..8]);
        assert_eq!(value_spans(0..4, 16, cfg), vec![0..8], "a whole word");
        assert_eq!(
            value_spans(0..8, 16, cfg),
            vec![0..8, 9..17],
            "two words, and the space between them is not selected"
        );
        assert_eq!(value_spans(0..99, 16, cfg).len(), 4, "clamped to the row");
        assert!(value_spans(16..20, 16, cfg).is_empty());
        // Under a decimal format every byte of a word gives the same cell, and
        // the duplicates collapse.
        let dec = hex_at(HexGroup::Bits32, HexFormat::Unsigned, Endian::Little);
        assert_eq!(value_spans(0..4, 16, dec).len(), 1);
    }

    #[test]
    fn the_characters_side_moves_by_characters_and_the_bytes_side_by_columns() {
        let text = "aébcd";
        let window = CharWindow {
            enc: TextEncoding::UTF8,
            bytes: text.as_bytes(),
            start: 0,
        };
        let len = Some(text.len() as u64);
        let cfg = hex_at(HexGroup::Bits32, HexFormat::Hex, Endian::Little);
        // Two characters, one of which is two bytes.
        assert_eq!(
            side_step(HexSide::Chars, window, cfg, len, 0, 2),
            3,
            "five presses over UTF-8 can cover more than five bytes"
        );
        assert_eq!(side_step(HexSide::Chars, window, cfg, len, 3, -1), 1);
        assert_eq!(side_step(HexSide::Chars, window, cfg, len, 0, -1), 0);
        assert_eq!(
            side_step(HexSide::Chars, window, cfg, len, 0, 99),
            5,
            "clamped to the file"
        );
        // The same press on the bytes side is a whole column, and the window is
        // not consulted at all.
        assert_eq!(side_step(HexSide::Bytes, window, cfg, len, 0, 1), 4);
        assert_eq!(
            side_step(HexSide::Bytes, CharWindow::empty(), cfg, len, 0, 1),
            4
        );
        // A cursor outside the window has no character to measure and falls
        // back to a byte, rather than to a read.
        let far = CharWindow {
            enc: TextEncoding::UTF8,
            bytes: text.as_bytes(),
            start: 4_096,
        };
        assert_eq!(side_step(HexSide::Chars, far, cfg, None, 0, 1), 1);
    }

    #[test]
    fn a_character_side_selection_can_half_cover_a_word() {
        // the characters side selects characters, and a
        // character is not a word. Nothing is rounded; the bytes side then
        // draws the covered columns partly highlighted.
        let text = "aébcd";
        let window = CharWindow {
            enc: TextEncoding::UTF8,
            bytes: text.as_bytes(),
            start: 0,
        };
        let len = Some(text.len() as u64);
        let cfg = hex_at(HexGroup::Bits32, HexFormat::Hex, Endian::Little);
        let anchor = 0;
        let head = side_step(HexSide::Chars, window, cfg, len, anchor, 2);
        assert_eq!(head, 3);
        assert!(
            !word_aligned(anchor, head, len, cfg),
            "three bytes stop inside a 32-bit word"
        );
        // Three of the word's four bytes: six of the cell's eight digits, and
        // the leading pair (byte 3, which a little-endian word writes first)
        // stays plain.
        let covered = value_spans(0..3, 16, cfg);
        assert_eq!(covered, vec![2..8]);
        assert_ne!(covered, value_spans(0..4, 16, cfg), "not the whole column");
        // The same two presses on the bytes side could not have done it.
        let aligned = column_step(anchor, 1, cfg, len);
        assert_eq!(aligned, 4);
        assert!(word_aligned(anchor, aligned, len, cfg));
    }
}
