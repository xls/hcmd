//! What a selection **reads as**.
//!
//! > **The clipboard is always text**, so a byte selection has to be *rendered*
//! > as something. Rendering it the way it is already on screen is the answer:
//! > the user has said what these bytes are by choosing the grouping, and
//! > copying something else would be this program overruling them.
//!
//! # What lives here
//!
//! The free functions in this module are **pure**: each is handed bytes and a
//! configuration and returns a string. Nothing among them opens, seeks or reads
//! anything, which is what makes every rendering in the design testable
//! against a byte literal rather than against a file.
//!
//!
//! The one thing that is not pure is [`Viewer::copy`] at the foot of the file,
//! which is the read the copy needs. It sits here rather than in
//! [`super`] because it is the same subject - see "Why the read is here" below.
//!
//! # the design applies to a copy exactly as it applies to a layout
//!
//! > Nothing about this reads more of the file than the selection covers, which
//! > is the rule and applies to a selection spanning a hundred megabytes
//! > exactly as it applies to rendering.
//!
//! So the selection's **span** is checked against `viewer.copy_max` *before the
//! first read*, and a selection above it is refused rather than truncated. That
//! is what makes `Ctrl+A` on a 40 GB file and `Ctrl+C` after it both instant:
//! the refusal is arithmetic on two `u64`s and the file is never touched
//! (the "selecting 40 GB is instant, and copying it is refused with
//! the size").
//!
//! # Why the read is here
//!
//! [`Viewer::copy`] loops [`Source::read_window`] into an accumulator, which is
//! the **one** bounded exception to the design I2 and is written
//! down as such in the design. Keeping it in this file rather
//! than among the movement methods keeps that exception in one place, where the
//! bound is three lines above the loop it bounds. Nothing else in the crate may
//! accumulate windows.

use std::ops::Range;

use crate::config::{Endian, HexConfig, ViewerMode};
use crate::error::Result;

use super::decode::TextEncoding;
use super::select::{HexSide, SelectKind, word_aligned};
use super::source::{self, WindowLen};
use super::{Viewer, encoding, hex, text};

/// Which copy was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyRequest {
    /// `Ctrl+C`: the selection itself.
    Selection,
    /// `Ctrl+Shift+C`: the interpretations line instead - "copies
    /// that reading".
    Interpretation,
}

/// The answer to a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copied {
    /// Text for the clipboard.
    Text {
        /// What goes to `OSC 52` and to the internal clipboard.
        text: String,
        /// How many **file** bytes it came from, for the message. The span for
        /// a rectangular selection, which is what was covered and what was
        /// read.
        bytes: u64,
        /// Why it is not what the user might have expected: the
        /// half-covered-word fallback of the design, above all.
        note: Option<String>,
    },
    /// Refused, with the reason for the status line.
    ///
    /// **Never a truncation** - "Large selections are refused
    /// rather than truncated". A refusal is not an error either: an `Err` from
    /// [`Viewer::copy`] is a failed *read*, and the two must not be reported
    /// the same way.
    Refused(String),
}

/// What every selection key says when `viewer.cursor = false`
/// (the design item 4).
///
/// the rule - never a panic and never silence - so `Shift+Down`,
/// `Ctrl+A`, `Ctrl+C` and `Ctrl+Shift+C` all report this rather than doing
/// nothing. One constant so the four cannot drift apart.
pub const NO_CURSOR: &str = "the viewer has no cursor - set viewer.cursor = true to select";

/// What `Ctrl+C` says with nothing selected.
///
/// It does **not** copy the cursor's line: the design says "copy the
/// selection", and copying something that was never selected is the kind of
/// helpfulness that loses data on the next paste.
///
pub const NOTHING_SELECTED: &str = "nothing selected - hold Shift with the arrow keys";

/// What `Ctrl+C` says about a block that is zero columns wide.
///
/// A rectangular selection is the intersection of a row range with a column
/// band, and `Ctrl+Shift+Down` with no sideways movement makes that band empty:
/// the head's column is exclusive, exactly as the head's byte is.
/// Nothing is painted and the status line says
/// `0 cols`, so the honest answer here is the same one - not one column's worth
/// of bytes the user was never shown as selected, which is the failure mode
/// [`NOTHING_SELECTED`] is written against.
pub const EMPTY_BLOCK: &str = "the block is 0 columns wide - extend it sideways to select";

/// What `Ctrl+Shift+C` says about a block.
///
/// the readout is of "the selected bytes in file order", and a
/// block's bytes are not one run: between its two corners lie the bytes to the
/// left and right of the band, which `Ctrl+C` correctly does not copy. Reading
/// them anyway would put a number on the status line that describes bytes the
/// selection does not hold (the design invariant 14).
pub const NO_BLOCK_INTERPRETATION: &str =
    "no interpretation for a block - it is not one run of bytes (Alt+B makes it linear)";

/// What `Ctrl+C` says in mode 3.
///
/// A rendered line is not a run of the file's bytes - an HTML paragraph is
/// assembled from text scattered across a dozen tags - so there is no span to
/// copy and nothing honest to put on the clipboard. Naming the two modes that
/// can is what makes it an instruction rather than a refusal.
pub const NO_RENDER_COPY: &str =
    "mode 3 has no byte selection to copy - press 1 for text or 2 for hex, then select";

/// the `viewer.copy_max` refusal, with both numbers.
///
/// Both, because a refusal that names neither the selection nor the limit
/// leaves the user with nothing to do about it - and because the size is what
/// the design asks for: "copying it is refused with the size".
pub fn too_large(span: u64, copy_max: u64) -> String {
    format!(
        "selection is {span} bytes; viewer.copy_max is {copy_max} - refused rather than truncated"
    )
}

/// `Ctrl+Shift+C` on a selection that has no reading
/// (the design item 4).
///
/// It refuses with the length rather than falling back to copying the
/// selection, so `Ctrl+C` and `Ctrl+Shift+C` never do the same thing by
/// accident.
pub fn no_interpretation(span: u64) -> String {
    format!("no interpretation for a {span}-byte selection - 1, 2, 4 and 8 bytes have one")
}

/// The note a copy from the bytes side carries when the selection does not line
/// up with the grouping.
///
/// > a copy from that side falls back to **hex digits for the whole covered
/// > range**, saying so, rather than printing a value for a word it only holds
/// > half of.
pub fn unaligned_note(cfg: HexConfig) -> String {
    format!(
        "not aligned to the {}-bit columns - copied as hex digits",
        cfg.group.bits()
    )
}

/// The characters side, and text mode: the bytes decoded with the active
/// encoding.
///
/// > The **characters** side yields the characters, decoded with the active
/// > encoding - the same text the eye is reading.
///
/// **The file's own text, not the screen's glyphs**: tabs stay tabs, control
/// characters stay control characters and a lone `\r` is kept. The screen
/// substitutes for legibility and a clipboard is
/// not a screen.
///
/// `last` is true here because the selection's end is the end of what there is
/// to decode: an incomplete sequence at the tail of a *window* is carried into
/// the next window, but a selection has no next window, so the honest rendering
/// of a half character the user selected is the replacement glyph the design
/// prescribes.
pub fn as_text(enc: TextEncoding, bytes: &[u8]) -> String {
    encoding::decode_window(enc, bytes, true).text
}

/// The bytes side: the columns **as they are displayed**.
///
/// `start` is the file offset `bytes` begins at and `width` is
/// `viewer.hex_width`, so the rows break on the **file's own row grid** and a
/// copied run lines up with what was on the screen. The first line is therefore
/// short whenever the selection did not begin on a row boundary - short, not
/// padded with leading blanks, because blanks in a clipboard payload are noise
/// that no editor will thank anyone for.
///
/// A column the selection covers only part of is written as the hex digits of
/// the bytes it does cover, which is the same rule
/// [`hex::value_column`] applies to a trailing word short of its bytes: a value
/// cannot be printed for a word only half held.
pub fn as_columns(bytes: &[u8], start: u64, width: u16, cfg: HexConfig) -> String {
    let stride = u64::from(width.max(1));
    let step = cfg.group.bytes().max(1);
    let end = start.saturating_add(bytes.len() as u64);
    let mut out = String::new();
    // The grid row the first selected byte sits in, not the byte itself: the
    // rows have to break where the screen breaks them.
    let mut row = start.saturating_sub(start % stride);
    let mut first_row = true;
    while row < end {
        if !first_row {
            out.push('\n');
        }
        first_row = false;
        let mut at = 0_usize;
        let mut first_cell = true;
        while at < usize::from(width.max(1)) {
            let cell_end = at.saturating_add(step).min(usize::from(width.max(1)));
            let from = row.saturating_add(at as u64);
            let to = row.saturating_add(cell_end as u64);
            if to > start && from < end {
                if !first_cell {
                    out.push(' ');
                }
                first_cell = false;
                let lo = from.max(start).saturating_sub(start);
                let hi = to.min(end).saturating_sub(start);
                let word = slice(bytes, lo, hi);
                // One cell's worth. The width handed over is what the
                // selection actually covers, not the whole word: that is what
                // puts a half-covered column on `value_column`'s short-word
                // path, where it is written as the digits it has rather than
                // as a value it cannot have.
                out.push_str(&hex::value_column(word, cell_bytes(word.len()), cfg));
            }
            at = cell_end.max(at.saturating_add(1));
        }
        row = row.saturating_add(stride);
    }
    out
}

/// The half-covered-word fallback.
///
/// > a copy from that side falls back to **hex digits for the whole covered
/// > range**
///
/// Whatever the grouping, and in rows of `width` broken on the file's own grid,
/// exactly as [`as_columns`] breaks them - so the fallback lines up with the
/// rows the reading it replaced would have produced.
pub fn as_hex_digits(bytes: &[u8], start: u64, width: u16) -> String {
    let stride = u64::from(width.max(1));
    let end = start.saturating_add(bytes.len() as u64);
    let mut out = String::new();
    let mut row = start.saturating_sub(start % stride);
    let mut first_row = true;
    while row < end {
        if !first_row {
            out.push('\n');
        }
        first_row = false;
        let lo = row.max(start);
        let hi = row.saturating_add(stride).min(end);
        let mut first_byte = true;
        for b in slice(bytes, lo.saturating_sub(start), hi.saturating_sub(start)) {
            if !first_byte {
                out.push(' ');
            }
            first_byte = false;
            out.push(nibble(b >> 4));
            out.push(nibble(b & 0x0F));
        }
        row = row.saturating_add(stride);
    }
    out
}

/// A rectangular block of text (the "a column block").
///
/// `rows` is the **expanded** text of each covered row, in order, and `columns`
/// is the band. Expanded because a block in text mode is measured in display
/// columns of the row as drawn: bytes would not line up with what the eye is
/// following, and "one field out of aligned output" is a statement about
/// columns.
///
/// Slicing goes through [`text::column_range`], so a grapheme cluster
/// straddling either edge is dropped whole - half of a wide character is not a
/// character - and the block lines up with what was on screen. Joined with
/// `\n`, no trailing newline.
pub fn as_block(rows: &[String], columns: Range<usize>) -> String {
    let take = columns.end.saturating_sub(columns.start);
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let range = text::column_range(row, columns.start, take);
        out.push_str(row.get(range).unwrap_or(""));
    }
    out
}

/// the readout, for a selection of 1, 2, 4 or 8 bytes.
///
/// ```text
/// 4 bytes  2A 00 00 00  =  42 (LE)  ·  704643072 (BE)  ·  5.88e-44 (f32 LE)  ·  1.14e-13 (f32 BE)
/// ```
///
/// `None` for any other length: there is no reading of three bytes that this
/// program could state without inventing a type for them.
///
/// The grammar is fixed in the design so that the status line
/// and `Ctrl+Shift+C` cannot format it differently - they do not, because this
/// is the one function that produces it and both take the same string.
///
/// * the hex digits are the selected bytes in **file order**, always, whatever
///   `format` is set to: they are the identity of the bytes, not a reading of
///   them;
/// * the readings, in order: unsigned decimal, signed decimal where it differs
///   from the unsigned one, and the float the width admits - `f32` for four
///   bytes, `f64` for eight. A one-byte selection also gets the character
///   [`hex::ascii_glyph`] draws, quoted;
/// * **byte order is never guessed.** At `group = 8` nothing has been declared,
///   so every numeric reading is given both ways round, tagged `(LE)` and
///   `(BE)`; above `group = 8` the user has declared one and only that one
///   appears, under its own name. A one-byte selection has no byte order and is
///   tagged with neither;
/// * `ascii` degrades the separator with every other glyph this program
///   chooses.
pub fn interpretations(bytes: &[u8], cfg: HexConfig, ascii: bool) -> Option<String> {
    let n = bytes.len();
    if !matches!(n, 1 | 2 | 4 | 8) {
        return None;
    }
    // At the default grouping nothing has been declared about these bytes, so
    // both orders are stated. Above it the user has said which, and stating the
    // other would be offering a reading they have already rejected.
    let orders: &[Endian] = if cfg.group.bytes() <= 1 {
        &[Endian::Little, Endian::Big]
    } else {
        match cfg.endian {
            Endian::Little => &[Endian::Little],
            Endian::Big => &[Endian::Big],
        }
    };
    // One byte has no byte order at all, so it is tagged with neither and read
    // once.
    let tagged = n > 1;
    let single: &[Endian] = &[Endian::Little];
    let orders = if tagged { orders } else { single };

    let mut readings: Vec<String> = Vec::new();
    for order in orders {
        readings.push(tag(&word(bytes, *order).to_string(), *order, tagged, ""));
    }
    for order in orders {
        let unsigned = word(bytes, *order);
        let signed = as_signed(unsigned, n);
        // Only when it differs: `42` twice over says nothing, and the status
        // line has a 60-column terminal to fit on.
        if signed.to_string() != unsigned.to_string() {
            readings.push(tag(&signed.to_string(), *order, tagged, ""));
        }
    }
    match n {
        4 => {
            for order in orders {
                let v = f32::from_bits(word(bytes, *order) as u32);
                readings.push(tag(&float(f64::from(v)), *order, tagged, "f32 "));
            }
        }
        8 => {
            for order in orders {
                let v = f64::from_bits(word(bytes, *order));
                readings.push(tag(&float(v), *order, tagged, "f64 "));
            }
        }
        _ => {}
    }
    if n == 1 {
        // The gutter's glyph, so the reading agrees with what is drawn beside
        // the byte.
        let glyph = bytes.first().copied().map_or('.', hex::ascii_glyph);
        readings.push(format!("'{glyph}'"));
    }

    let sep = if ascii { SEP_ASCII } else { SEP };
    let mut out = String::new();
    out.push_str(&n.to_string());
    out.push_str(if n == 1 { " byte  " } else { " bytes  " });
    let mut first = true;
    for b in bytes {
        if !first {
            out.push(' ');
        }
        first = false;
        out.push(nibble(b >> 4));
        out.push(nibble(b & 0x0F));
    }
    out.push_str("  =  ");
    out.push_str(&readings.join(sep));
    Some(out)
}

/// The separator between readings.
///
/// Two spaces around it, so the line reads as a list rather than as one long
/// number, and so it sits inside a status field that is itself joined with two
/// spaces (`crate::ui::viewer`'s `SEPARATOR`).
const SEP: &str = "  ·  ";

/// [`SEP`] for a terminal that cannot draw one, on the rule for every
/// glyph this program chooses. A pipe is a stronger mark than a middle dot and
/// needs less air around it, which is why it is padded with one space rather
/// than two.
const SEP_ASCII: &str = " | ";

/// `42` → `42 (LE)`, or `42` when there is no byte order to state.
fn tag(reading: &str, order: Endian, tagged: bool, prefix: &str) -> String {
    if tagged {
        format!("{reading} ({prefix}{})", order.label())
    } else {
        reading.to_string()
    }
}

/// `{:e}` with two decimals, which is what the design fixes.
fn float(v: f64) -> String {
    format!("{v:.2e}")
}

/// Read up to eight bytes as one unsigned word, in `order`.
fn word(bytes: &[u8], order: Endian) -> u64 {
    let mut value = 0_u64;
    match order {
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

/// Reinterpret the low `bytes` of `value` as two's complement.
const fn as_signed(value: u64, bytes: usize) -> i64 {
    let bits = (bytes as u32).saturating_mul(8);
    if bits >= 64 {
        return value as i64;
    }
    let shift = 64_u32.saturating_sub(bits);
    ((value << shift) as i64) >> shift
}

/// One hex digit.
///
/// Lower case, because that is what [`hex::value_column`] draws and therefore
/// what is on the screen beside these bytes. the design writes its example
/// as `2A 00 00 00`; the normative sentence beside it is "the columns **as they
/// are displayed**", and one hex digit case in the program beats two that can
/// disagree.
const fn nibble(v: u8) -> char {
    match v & 0x0F {
        d @ 0..=9 => (b'0' + d) as char,
        d => (b'a' + d - 10) as char,
    }
}

/// A byte range of a slice, empty rather than panicking when it is not there.
fn slice(bytes: &[u8], from: u64, to: u64) -> &[u8] {
    let from = usize::try_from(from).unwrap_or(usize::MAX);
    let to = usize::try_from(to).unwrap_or(usize::MAX);
    bytes.get(from..to.max(from)).unwrap_or(&[])
}

/// One column's covered byte count as a [`hex::value_column`] width.
fn cell_bytes(covered: usize) -> u16 {
    u16::try_from(covered).unwrap_or(u16::MAX).max(1)
}

// ---------------------------------------------------------------------------
// The read
// ---------------------------------------------------------------------------

impl Viewer {
    /// `Ctrl+C` / `Ctrl+Shift+C`: **queue** a copy.
    ///
    /// Queued rather than performed, because copying reads the file and
    /// `dispatch` may not touch the filesystem. The
    /// event loop takes it back out with [`Viewer::take_copy_request`] and
    /// performs it before the next layout, so the message lands on the same
    /// frame as the keystroke.
    pub const fn request_copy(&mut self, what: CopyRequest) {
        self.copy_request = Some(what);
    }

    /// Take the queued copy, if there is one. The event loop calls this once a
    /// frame.
    pub const fn take_copy_request(&mut self) -> Option<CopyRequest> {
        self.copy_request.take()
    }

    /// Whether a copy is queued.
    ///
    /// For `App::input_route`, which stops `main::drain_input` on a queued copy
    /// exactly as it stops on a queued view: `Ctrl+C`, `Esc`, `Ctrl+C` must not
    /// be collapsed into one frame, or the second copy would be made from a
    /// selection the first one still had. The
    /// twin of `App::pending_view`, and there for the same reason.
    pub const fn copy_pending(&self) -> bool {
        self.copy_request.is_some()
    }

    /// Perform a queued copy. **Reads**; call it from the
    /// event loop.
    ///
    /// `copy_max` is `viewer.copy_max` in bytes. The selection's **span** is
    /// checked against it *before anything is read*, and a selection above it
    /// is refused rather than truncated - which is what makes `Ctrl+A` on a
    /// 40 GB file and the `Ctrl+C` after it both instant.
    /// The span is the right thing to check even for a rectangular selection:
    /// it is exact, free, and an upper bound on what a block copy can read.
    ///
    ///
    /// Every refusal comes back as [`Copied::Refused`], never as an `Err`. An
    /// `Err` here is a failed read and nothing else, so the caller can tell a
    /// rule from a fault.
    pub fn copy(&mut self, what: CopyRequest, copy_max: u64) -> Result<Copied> {
        // The refusals of the design, in order.
        if !self.cursor_enabled {
            return Ok(Copied::Refused(NO_CURSOR.to_string()));
        }
        let Some(sel) = self.sel else {
            return Ok(Copied::Refused(NOTHING_SELECTED.to_string()));
        };
        let (lo, hi) = sel.range();
        let span = hi.saturating_sub(lo);
        if span == 0 {
            return Ok(Copied::Refused(NOTHING_SELECTED.to_string()));
        }
        if span > copy_max {
            return Ok(Copied::Refused(too_large(span, copy_max)));
        }
        // A block with an empty column band covers no byte at all, whatever
        // its span says. The painter and the status line already answer it
        // that way; this is the third of the three agreeing.
        if matches!(sel.kind, SelectKind::Rectangular) {
            let (from, to) = sel.columns();
            if from >= to {
                return Ok(Copied::Refused(EMPTY_BLOCK.to_string()));
            }
        }
        match what {
            CopyRequest::Interpretation => match sel.kind {
                SelectKind::Linear => self.copy_interpretation(lo, hi, span),
                // The reading is of one run of bytes; a block is not one.
                SelectKind::Rectangular => Ok(Copied::Refused(NO_BLOCK_INTERPRETATION.to_string())),
            },
            // Mode 3 has no selection to copy: its cursor is a rendered line
            // and the lines are not a run of the file's bytes. Refusing by
            // name is the rule the block interpretation above follows.
            CopyRequest::Selection if self.mode == ViewerMode::Render => {
                Ok(Copied::Refused(NO_RENDER_COPY.to_string()))
            }
            CopyRequest::Selection => match (self.mode, sel.kind) {
                (ViewerMode::Text | ViewerMode::Render, SelectKind::Linear) => {
                    let bytes = self.read_span(lo, hi)?;
                    Ok(Copied::Text {
                        // What was read, not what was asked for: the file can
                        // have been truncated under a selection `F2` kept,
                        // and a message that
                        // overstates what reached the clipboard is worse than
                        // no message.
                        bytes: bytes.len() as u64,
                        text: as_text(self.encoding, &bytes),
                        note: None,
                    })
                }
                (ViewerMode::Text | ViewerMode::Render, SelectKind::Rectangular) => {
                    let rows = self.block_rows(lo, hi)?;
                    let (from, to) = sel.columns();
                    Ok(Copied::Text {
                        text: as_block(&rows, from..to),
                        bytes: span,
                        note: None,
                    })
                }
                (ViewerMode::Hex, SelectKind::Linear) => self.copy_hex_linear(lo, hi),
                (ViewerMode::Hex, SelectKind::Rectangular) => {
                    let (from, to) = sel.columns();
                    self.copy_hex_block(lo, hi, span, from, to)
                }
            },
        }
    }

    /// `Ctrl+Shift+C` ("copies that reading instead").
    ///
    /// Character for character the string the status line is showing, because
    /// it is the same function that produced it - which is the whole of
    /// the design invariant 15.
    fn copy_interpretation(&mut self, lo: u64, hi: u64, span: u64) -> Result<Copied> {
        if !matches!(span, 1 | 2 | 4 | 8) {
            return Ok(Copied::Refused(no_interpretation(span)));
        }
        let bytes = self.read_span(lo, hi)?;
        let ascii = self.ascii;
        match interpretations(&bytes, self.hex_cfg, ascii) {
            Some(text) => Ok(Copied::Text {
                text,
                bytes: span,
                note: None,
            }),
            // Only reachable when the read came back short - the file was
            // truncated underneath the selection. Report the length that is
            // actually there rather than inventing a reading for it.
            None => Ok(Copied::Refused(no_interpretation(bytes.len() as u64))),
        }
    }

    /// A linear selection in hex mode (the two sides).
    fn copy_hex_linear(&mut self, lo: u64, hi: u64) -> Result<Copied> {
        let bytes = self.read_span(lo, hi)?;
        match self.side {
            // "the same text the eye is reading" - and the same rendering text
            // mode gives, because it is the same question.
            HexSide::Chars => Ok(Copied::Text {
                bytes: bytes.len() as u64,
                text: as_text(self.encoding, &bytes),
                note: None,
            }),
            HexSide::Bytes => {
                let cfg = self.hex_cfg;
                let width = self.hex.width();
                let read = bytes.len() as u64;
                if word_aligned(lo, lo.saturating_add(read), self.source.len(), cfg) {
                    Ok(Copied::Text {
                        text: as_columns(&bytes, lo, width, cfg),
                        bytes: read,
                        note: None,
                    })
                } else {
                    // hex digits for the whole covered range,
                    // saying so, rather than a value for a word half held.
                    Ok(Copied::Text {
                        text: as_hex_digits(&bytes, lo, width),
                        bytes: read,
                        note: Some(unaligned_note(cfg)),
                    })
                }
            }
        }
    }

    /// A rectangular selection in hex mode: the same byte column of every row
    /// (the design - "the third byte of every record").
    fn copy_hex_block(
        &mut self,
        lo: u64,
        hi: u64,
        span: u64,
        from_col: usize,
        to_col: usize,
    ) -> Result<Copied> {
        let layout = self.hex;
        let cfg = self.hex_cfg;
        let width = layout.width();
        let stride = layout.stride();
        let band = self.hex_band(from_col, to_col);
        // One read over the covered rows rather than one per row: the rows are
        // contiguous, so this is the span rounded out to the row grid - at most
        // two rows more than the span `copy_max` already bounded.
        let first = layout.row_offset(layout.row_of(lo));
        let last = layout.row_offset(layout.row_of(hi)).saturating_add(stride);
        let window = self.read_span(first, last)?;
        let mut rows: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut at = first;
        while at <= layout.row_offset(layout.row_of(hi)) {
            let row_from = at.saturating_add(band.start as u64);
            let row_to = at.saturating_add(band.end as u64);
            let bytes = slice(
                &window,
                row_from.saturating_sub(first),
                row_to.saturating_sub(first),
            );
            if !bytes.is_empty() {
                rows.push((row_from, bytes.to_vec()));
            }
            at = at.saturating_add(stride);
        }
        match self.side {
            HexSide::Chars => {
                // The gutter's own glyphs, which are ASCII whatever the active
                // encoding is.
                let text = rows
                    .iter()
                    .map(|(_, bytes)| {
                        bytes
                            .iter()
                            .copied()
                            .map(hex::ascii_glyph)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Copied::Text {
                    text,
                    bytes: span,
                    note: None,
                })
            }
            HexSide::Bytes => {
                // If any row's band cuts a word, every row falls back together:
                // a block half in values and half in digits would be a worse
                // answer than either.
                let len = self.source.len();
                let aligned = rows.iter().all(|(at, bytes)| {
                    word_aligned(*at, at.saturating_add(bytes.len() as u64), len, cfg)
                });
                let text = rows
                    .iter()
                    .map(|(at, bytes)| {
                        if aligned {
                            as_columns(bytes, *at, width, cfg)
                        } else {
                            as_hex_digits(bytes, *at, width)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Copied::Text {
                    text,
                    bytes: span,
                    note: (!aligned).then(|| unaligned_note(cfg)),
                })
            }
        }
    }

    /// The band of byte columns a rectangular hex selection covers, clamped to
    /// the row.
    ///
    /// The band is **not** widened: a block whose two ends are in the same
    /// column is no column at all, which is what the painter draws
    /// (`Viewer::hex_row_sel`) and what the status line reports. A band widened
    /// here alone would put bytes on the clipboard that were never highlighted;
    /// [`EMPTY_BLOCK`] refuses that copy instead.
    fn hex_band(&self, from: usize, to: usize) -> Range<usize> {
        let width = usize::from(self.hex.width().max(1));
        let from = from.min(width);
        let to = to.min(width);
        from..to
    }

    /// The expanded text of every screen row a rectangular text selection
    /// covers, in order, for [`as_block`].
    ///
    /// **One read, then the lines are found inside it.** Walking line by line
    /// through [`Viewer::read_line`] would be the layout's own path, but a
    /// layout reads one screenful and this reads a selection: over a megabyte
    /// of forty-byte lines that is twenty-six thousand window reads of a few
    /// hundred bytes each, and the total would be a multiple of `copy_max`
    /// rather than `copy_max` itself. So the covered span is read once, through
    /// the same bounded [`Viewer::read_span`], and split on
    /// [`super::decode::LineTerm`] in memory - which is what
    /// the design invariant 12 asks for.
    ///
    /// What that costs above the span is two constants and neither is a
    /// function of the file: one row's worth past the end (so the last line the
    /// block touches is whole) and however far back the line containing `lo`
    /// began, which [`Viewer::line_start_at_or_before`] already bounds by
    /// [`super::NAV_READ_BYTES`].
    ///
    /// Each line is materialised at most `per_line` bytes deep, exactly as
    /// [`Viewer::layout`] materialises it - what the screen can show plus the
    /// horizontal scroll - so a line longer than any terminal costs a row, not
    /// a line.
    ///
    /// **With wrap on this takes every row of each covered line**, rather than
    /// only the wrapped rows the two ends fall in: mapping a wrapped row back
    /// to a file offset means inverting the tab expansion and walking the
    /// encoding, which would be a second implementation of the layout's own
    /// arithmetic living here. It is exact with wrap off, exact with wrap on
    /// for any line that fits its row, and over-inclusive only at the two ends
    /// of a block taken across a line that wrapped.
    /// Recorded rather than hidden.
    fn block_rows(&mut self, lo: u64, hi: u64) -> Result<Vec<String>> {
        let per_line = self.per_line();
        let cap = usize::try_from(per_line).unwrap_or(usize::MAX).max(1);
        let columns = usize::from(self.view_cols.max(1));
        let term = self.line_term();
        let enc = self.encoding;
        let tab_width = self.tab_width;
        let ascii = self.ascii;
        let wrap = self.wrap;
        let start = self.line_start_at_or_before(lo)?.at;
        let window = self.read_span(start, hi.saturating_add(per_line))?;

        let mut out: Vec<String> = Vec::new();
        let mut at = start;
        let mut idx = 0_usize;
        // A block is the rows from the anchor's to the head's *inclusive of
        // both*, so the walk stops once past `hi` rather than at it.
        //
        while at <= hi && idx < window.len() {
            let rest = window.get(idx..).unwrap_or(&[]);
            // The break is part of the line the way `read_line` counts it: the
            // next line starts `unit` bytes after it.
            let take = match term.find(rest, 0) {
                Some(found) => found.saturating_add(term.unit()).min(rest.len()),
                None => rest.len(),
            }
            .max(1);
            // Truncated: either the line runs past what any terminal could show
            // it of, or the read ended inside it. Either way the last bytes may
            // be half a character, and `decode_window` holds those back rather
            // than inventing a replacement glyph out of good bytes.
            //
            let cut = take > cap || (take == rest.len() && term.find(rest, 0).is_none());
            let line = rest.get(..take.min(cap)).unwrap_or(&[]);
            let decoded = encoding::decode_window(enc, term.trim_break(line), !cut).text;
            let expanded = text::expand(&decoded, tab_width, ascii);
            let next = at.saturating_add(take as u64);
            if next > lo {
                if wrap {
                    for r in text::wrap(&expanded, columns) {
                        out.push(expanded.get(r).unwrap_or("").to_string());
                    }
                } else {
                    out.push(expanded);
                }
            }
            at = next;
            idx = idx.saturating_add(take);
        }
        Ok(out)
    }

    /// Read `[from, to)` into memory, in windows.
    ///
    /// **The one bounded exception to the design I2**
    /// (the design, invariant 12): every other read in this
    /// crate is one window, and this one accumulates. It is called only after
    /// [`Viewer::copy`] has checked the span against `viewer.copy_max`, so what
    /// it holds is bounded by a configuration value at 1 MiB by default, and it
    /// runs in the event loop's copy service rather than on the frame path.
    /// Nothing else in the crate may loop [`super::source::Source::read_window`]
    /// into an accumulator.
    fn read_span(&mut self, from: u64, to: u64) -> Result<Vec<u8>> {
        let want = usize::try_from(to.saturating_sub(from)).unwrap_or(usize::MAX);
        let mut out: Vec<u8> = Vec::with_capacity(want.min(source::MAX_WINDOW));
        let mut at = from;
        while at < to {
            let left = usize::try_from(to.saturating_sub(at)).unwrap_or(source::MAX_WINDOW);
            let window = self.source.read_window(at, WindowLen::new(left))?;
            if window.is_empty() {
                // End of file inside the selection: what is there is what is
                // copied, and the message says how many bytes that was.
                break;
            }
            out.extend_from_slice(window.bytes());
            at = at.saturating_add(window.len() as u64);
        }
        out.truncate(want);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{HexFormat, HexGroup, ViewerConfig};
    use crate::viewer::ViewerId;
    use crate::viewer::select::Selection;

    fn cfg(group: HexGroup, format: HexFormat, endian: Endian) -> HexConfig {
        HexConfig {
            group,
            format,
            endian,
        }
    }

    const HEX8: HexConfig = HexConfig {
        group: HexGroup::Bits8,
        format: HexFormat::Hex,
        endian: Endian::Little,
    };

    // ------------------------------------------------ the characters side ---

    #[test]
    fn the_characters_side_is_the_files_own_text_not_the_screens() {
        // "the same text the eye is reading" - but a clipboard
        // is not a screen, so a tab is a tab and a bell is a bell.
        //
        let got = as_text(TextEncoding::UTF8, b"a\tb\x07c\r\n");
        assert_eq!(got, "a\tb\u{7}c\r\n");
    }

    #[test]
    fn a_half_character_at_the_end_of_a_selection_is_a_replacement_glyph() {
        // There is no next window for a selection to carry an incomplete
        // sequence into, so the rule applies: it renders rather
        // than failing the copy.
        let got = as_text(TextEncoding::UTF8, b"a\xc3");
        assert_eq!(got, "a\u{fffd}");
    }

    // ----------------------------------------------------- the bytes side ---

    #[test]
    fn the_bytes_side_copies_the_columns_as_they_are_displayed() {
        // "At the default `group = 8`, that is hex digits -
        // `2A 00 00 00` - which is the only honest rendering when nothing has
        // declared what the bytes mean."
        assert_eq!(as_columns(&[0x2A, 0, 0, 0], 0, 16, HEX8), "2a 00 00 00");
    }

    #[test]
    fn a_declared_word_size_is_copied_as_the_word_it_is() {
        // "a selection on the bytes side copies what the
        // columns say, in the format they are being shown in."
        let words = cfg(HexGroup::Bits32, HexFormat::Hex, Endian::Little);
        assert_eq!(as_columns(&[0x2A, 0, 0, 0], 0, 16, words), "0000002a");

        let decimal = cfg(HexGroup::Bits32, HexFormat::Unsigned, Endian::Little);
        assert_eq!(
            as_columns(&[0x2A, 0, 0, 0], 0, 16, decimal).trim(),
            "42",
            "a decimal column is padded to the widest the word can hold"
        );

        let big = cfg(HexGroup::Bits32, HexFormat::Unsigned, Endian::Big);
        assert_eq!(as_columns(&[0x2A, 0, 0, 0], 0, 16, big).trim(), "704643072");
    }

    #[test]
    fn the_rows_break_on_the_files_own_grid_and_the_first_may_be_short() {
        // the copied rows line up with the screen's,
        // so a selection starting mid-row starts mid-row in the clipboard too.
        let got = as_columns(&[0xAA, 0xBB, 0xCC, 0xDD], 14, 16, HEX8);
        assert_eq!(got, "aa bb\ncc dd");
    }

    #[test]
    fn a_column_the_selection_only_half_covers_is_written_as_its_digits() {
        // the rule for a trailing word short of its bytes, which
        // is the same situation seen from the other end: there is no value to
        // print for a word only half held.
        let words = cfg(HexGroup::Bits32, HexFormat::Unsigned, Endian::Little);
        let got = as_columns(&[0x11, 0x22], 0, 16, words);
        assert_eq!(got.trim(), "1122");
    }

    #[test]
    fn the_fallback_is_hex_digits_for_the_whole_covered_range() {
        // "a copy from that side falls back to hex digits for
        // the whole covered range" - whatever the grouping.
        assert_eq!(
            as_hex_digits(&[0x11, 0x22, 0x33], 0, 16),
            "11 22 33",
            "whatever `group` says, these are the bytes"
        );
        assert_eq!(
            as_hex_digits(&[0xAA, 0xBB, 0xCC, 0xDD], 14, 16),
            "aa bb\ncc dd",
            "and it breaks on the same grid the columns do"
        );
    }

    // ------------------------------------------------------- the block ------

    #[test]
    fn a_block_takes_one_field_out_of_aligned_output() {
        // the "a column block, which is how you take one field out
        // of aligned output".
        let rows = vec!["alpha  one".to_string(), "beta   two".to_string()];
        assert_eq!(as_block(&rows, 7..10), "one\ntwo");
    }

    #[test]
    fn a_block_drops_a_wide_character_straddling_an_edge_rather_than_halving_it() {
        // text::column_range's rule, which is the rule the screen scrolls by,
        // so the block is what the eye pointed at.
        let rows = vec!["ab\u{4e2d}cd".to_string()];
        // Columns 2..4 are the wide character itself.
        assert_eq!(as_block(&rows, 2..4), "\u{4e2d}");
        // A band beginning inside it drops it whole and takes the two columns
        // it can have instead, which is text::column_range's own rule and the
        // rule the screen scrolls by.
        assert_eq!(as_block(&rows, 3..5), "cd");
    }

    #[test]
    fn a_block_has_no_trailing_newline() {
        let rows = vec!["a".to_string(), "b".to_string()];
        assert_eq!(as_block(&rows, 0..1), "a\nb");
        assert_eq!(as_block(&[], 0..1), "");
    }

    // -------------------------------------------- the interpretations line ---

    #[test]
    fn four_bytes_read_both_ways_round_at_the_default_grouping() {
        // the own example line, and the design's
        // grammar. Invariant 16: byte order is never guessed.
        let got =
            interpretations(&[0x2A, 0, 0, 0], HEX8, false).expect("four bytes have a reading");
        assert!(got.starts_with("4 bytes  2a 00 00 00  =  "), "got {got:?}");
        assert!(got.contains("42 (LE)"), "got {got:?}");
        assert!(got.contains("704643072 (BE)"), "got {got:?}");
        assert!(got.contains("(f32 LE)"), "got {got:?}");
        assert!(got.contains("(f32 BE)"), "got {got:?}");
    }

    #[test]
    fn a_declared_byte_order_is_the_only_one_stated() {
        // Above `group = 8` the user has said which way round these bytes are;
        // offering the other would be offering a reading they have rejected.
        let big = cfg(HexGroup::Bits32, HexFormat::Hex, Endian::Big);
        let got = interpretations(&[0, 0, 0, 0x2A], big, false).expect("four bytes");
        assert_eq!(
            got,
            "4 bytes  00 00 00 2a  =  42 (BE)  ·  5.89e-44 (f32 BE)"
        );
        assert!(!got.contains("(LE)"), "the other order was not asked for");
    }

    #[test]
    fn one_byte_has_no_byte_order_and_gets_its_glyph() {
        assert_eq!(
            interpretations(&[0x41], HEX8, false).expect("one byte"),
            "1 byte  41  =  65  ·  'A'"
        );
        // The signed reading appears only where it says something different.
        assert_eq!(
            interpretations(&[0xD6], HEX8, false).expect("one byte"),
            "1 byte  d6  =  214  ·  -42  ·  '.'"
        );
    }

    #[test]
    fn two_bytes_have_no_float_to_read() {
        let got = interpretations(&[0xFF, 0xFF], HEX8, false).expect("two bytes");
        assert_eq!(
            got,
            "2 bytes  ff ff  =  65535 (LE)  ·  65535 (BE)  ·  -1 (LE)  ·  -1 (BE)"
        );
    }

    #[test]
    fn there_is_no_reading_of_three_bytes() {
        // there is no reading of three bytes this
        // program could state without inventing a type for them.
        assert_eq!(interpretations(&[1, 2, 3], HEX8, false), None);
        assert_eq!(interpretations(&[], HEX8, false), None);
        assert_eq!(interpretations(&[0; 5], HEX8, false), None);
    }

    #[test]
    fn only_one_two_four_and_eight_bytes_have_a_reading() {
        for n in [0_usize, 3, 5, 6, 7, 9, 16] {
            assert_eq!(
                interpretations(&vec![0_u8; n], HEX8, false),
                None,
                "there is no reading of {n} bytes to state"
            );
        }
        for n in [1_usize, 2, 4, 8] {
            assert!(
                interpretations(&vec![0_u8; n], HEX8, false).is_some(),
                "{n}"
            );
        }
    }

    #[test]
    fn an_interpretation_states_its_byte_order_or_gives_both() {
        // the design invariant 16, in one test under its own name.
        assert_eq!(
            interpretations(&[0x01, 0x02], HEX8, false).as_deref(),
            Some("2 bytes  01 02  =  513 (LE)  ·  258 (BE)"),
            "at group = 8 nothing has been declared, so both ways round"
        );
        let declared = cfg(HexGroup::Bits32, HexFormat::Hex, Endian::Big);
        let line = interpretations(&[0x01, 0x02, 0x03, 0x04], declared, false)
            .expect("four bytes have a reading");
        assert!(line.contains("16909060 (BE)"), "{line}");
        assert!(
            !line.contains("(LE)"),
            "the user declared big-endian: {line}"
        );
    }

    #[test]
    fn an_eight_byte_reading_is_a_double() {
        let at64 = cfg(HexGroup::Bits64, HexFormat::Hex, Endian::Little);
        let line = interpretations(&1.0_f64.to_le_bytes(), at64, false).expect("eight bytes");
        assert!(
            line.starts_with("8 bytes  00 00 00 00 00 00 f0 3f  ="),
            "{line}"
        );
        assert!(line.ends_with("1.00e0 (f64 LE)"), "{line}");
    }

    #[test]
    fn the_separator_degrades_with_every_other_glyph_this_program_chooses() {
        // `ui.ascii_borders` is one switch over every glyph the
        // program picks for itself.
        let got = interpretations(&[0x41], HEX8, true).expect("one byte");
        assert_eq!(got, "1 byte  41  =  65 | 'A'");
    }

    // --------------------------------------------------------- refusals ------

    #[test]
    fn a_copy_above_the_limit_is_refused_with_both_numbers() {
        // "copying it is refused with the size", and the design:
        // "refused rather than truncated".
        let got = too_large(42_949_672_960, 1_048_576);
        assert_eq!(
            got,
            "selection is 42949672960 bytes; viewer.copy_max is 1048576 - refused rather than truncated"
        );
    }

    #[test]
    fn the_interpretation_refusal_says_which_lengths_have_one() {
        assert_eq!(
            no_interpretation(3),
            "no interpretation for a 3-byte selection - 1, 2, 4 and 8 bytes have one"
        );
    }

    #[test]
    fn the_unaligned_note_names_the_grouping_it_did_not_line_up_with() {
        let words = cfg(HexGroup::Bits32, HexFormat::Hex, Endian::Little);
        assert_eq!(
            unaligned_note(words),
            "not aligned to the 32-bit columns - copied as hex digits"
        );
    }
    // ------------------------------------------------ the copy itself -------

    /// A viewer over bytes already in memory, which is a `Source` like any
    /// other and reads through the same window machinery
    /// (`crate::viewer::source`).
    fn open(bytes: &[u8]) -> Viewer {
        let cfg = ViewerConfig::default();
        let len = bytes.len() as u64;
        let opener = source::memory_opener(Arc::new(bytes.to_vec()));
        let mut v =
            Viewer::open(ViewerId(1), "t", None, opener, Some(len), &cfg).expect("open in memory");
        v.cursor_enabled = true;
        v
    }

    fn select(v: &mut Viewer, from: u64, to: u64, kind: SelectKind) {
        let mut sel = Selection::new(from, 0, kind);
        sel.head = to;
        v.sel = Some(sel);
    }

    /// A block: two offsets and the two columns their ends sit in.
    fn select_block(v: &mut Viewer, from: (u64, usize), to: (u64, usize)) {
        let mut sel = Selection::new(from.0, from.1, SelectKind::Rectangular);
        sel.extend_to(to.0, to.1, SelectKind::Rectangular);
        v.sel = Some(sel);
    }

    #[test]
    fn a_copy_with_no_cursor_says_why_it_cannot_select() {
        // the design item 4: never a panic and never silence.
        let mut v = open(b"hello");
        v.cursor_enabled = false;
        select(&mut v, 0, 5, SelectKind::Linear);
        assert_eq!(
            v.copy(CopyRequest::Selection, 1024)
                .expect("no read to fail"),
            Copied::Refused(NO_CURSOR.to_string())
        );
    }

    #[test]
    fn a_copy_with_nothing_selected_refuses_rather_than_copying_the_line() {
        // copying something that was never selected
        // is the kind of helpfulness that loses data on the next paste.
        let mut v = open(b"hello");
        assert_eq!(
            v.copy(CopyRequest::Selection, 1024)
                .expect("no read to fail"),
            Copied::Refused(NOTHING_SELECTED.to_string())
        );
        // An anchor the cursor never moved off is nothing selected too.
        select(&mut v, 3, 3, SelectKind::Linear);
        assert_eq!(
            v.copy(CopyRequest::Selection, 1024)
                .expect("no read to fail"),
            Copied::Refused(NOTHING_SELECTED.to_string())
        );
    }

    #[test]
    fn copying_forty_gigabytes_is_refused_with_the_size_and_reads_nothing() {
        // "selecting 40 GB is instant, and copying it is refused
        // with the size". The file here is five bytes long and the selection is
        // forty gigabytes: a refusal that read first could not come back at
        // all, let alone come back with both numbers.
        let mut v = open(b"hello");
        let span = 40_u64 * 1024 * 1024 * 1024;
        select(&mut v, 0, span, SelectKind::Linear);
        assert_eq!(
            v.copy(CopyRequest::Selection, 1024 * 1024)
                .expect("no read to fail"),
            Copied::Refused(too_large(span, 1024 * 1024)),
            "refused rather than truncated, and before any read"
        );
    }

    #[test]
    fn a_copy_of_exactly_the_limit_is_allowed() {
        // The bound is a limit, not a fence one short of it: `copy_max` bytes
        // is a copy `viewer.copy_max` permits.
        let mut v = open(b"hello");
        select(&mut v, 0, 5, SelectKind::Linear);
        let got = v.copy(CopyRequest::Selection, 5).expect("no read to fail");
        assert_eq!(
            got,
            Copied::Text {
                text: "hello".to_string(),
                bytes: 5,
                note: None,
            }
        );
    }

    #[test]
    fn a_text_copy_is_the_selection_and_only_the_selection() {
        let mut v = open(b"the quick brown fox");
        select(&mut v, 4, 9, SelectKind::Linear);
        let got = v.copy(CopyRequest::Selection, 1024).expect("read");
        assert_eq!(
            got,
            Copied::Text {
                text: "quick".to_string(),
                bytes: 5,
                note: None,
            }
        );
    }

    #[test]
    fn the_bytes_side_copies_the_columns_and_the_characters_side_the_text() {
        // the two sides are two views of one selection, and each
        // yields what that side is showing.
        let mut v = open(b"abcdefgh");
        v.mode = ViewerMode::Hex;
        select(&mut v, 0, 4, SelectKind::Linear);

        v.side = HexSide::Bytes;
        let bytes_side = v.copy(CopyRequest::Selection, 1024).expect("read");
        assert_eq!(
            bytes_side,
            Copied::Text {
                text: "61 62 63 64".to_string(),
                bytes: 4,
                note: None,
            }
        );

        v.side = HexSide::Chars;
        let chars_side = v.copy(CopyRequest::Selection, 1024).expect("read");
        assert_eq!(
            chars_side,
            Copied::Text {
                text: "abcd".to_string(),
                bytes: 4,
                note: None,
            }
        );
    }

    #[test]
    fn a_selection_across_a_word_boundary_copies_hex_digits_and_says_so() {
        // the half-covered word, which is invariant 10.
        let mut v = open(b"\x01\x02\x03\x04\x05\x06\x07\x08");
        v.mode = ViewerMode::Hex;
        v.hex_cfg = HexConfig {
            group: HexGroup::Bits32,
            format: HexFormat::Hex,
            endian: Endian::Little,
        };
        v.side = HexSide::Bytes;
        select(&mut v, 1, 5, SelectKind::Linear);
        let got = v.copy(CopyRequest::Selection, 1024).expect("read");
        assert_eq!(
            got,
            Copied::Text {
                text: "02 03 04 05".to_string(),
                bytes: 4,
                note: Some("not aligned to the 32-bit columns - copied as hex digits".to_string()),
            },
            "no value can be printed for a word only half held"
        );
    }

    #[test]
    fn the_interpretation_refuses_a_length_that_has_none() {
        // it refuses with the length rather than
        // falling back to the selection, so the two keys never coincide.
        let mut v = open(b"abcdefgh");
        select(&mut v, 0, 3, SelectKind::Linear);
        assert_eq!(
            v.copy(CopyRequest::Interpretation, 1024).expect("no read"),
            Copied::Refused(no_interpretation(3))
        );
    }

    #[test]
    fn the_reading_that_is_copied_is_the_one_the_status_line_shows() {
        // Invariant 15, held by construction: `interpretations` is the one
        // function that produces the line, and this is the same call the status
        // field makes.
        let mut v = open(b"\x2a\x00\x00\x00");
        select(&mut v, 0, 4, SelectKind::Linear);
        let got = v.copy(CopyRequest::Interpretation, 1024).expect("read");
        let expected = interpretations(&[0x2A, 0, 0, 0], v.hex_cfg, v.ascii).expect("four bytes");
        assert_eq!(
            got,
            Copied::Text {
                text: expected,
                bytes: 4,
                note: None,
            }
        );
    }

    #[test]
    fn a_block_in_text_mode_takes_one_field_out_of_every_covered_row() {
        // "a column block, which is how you take one field out
        // of aligned output."
        let mut v = open(b"alpha  one\nbeta   two\n");
        select_block(&mut v, (0, 7), (11, 10));
        let got = v.copy(CopyRequest::Selection, 1024).expect("read");
        assert_eq!(
            got,
            Copied::Text {
                text: "one\ntwo".to_string(),
                bytes: 11,
                note: None,
            },
            "the head's own row is in the block"
        );
    }

    #[test]
    fn a_block_in_hex_takes_the_same_byte_column_of_every_row() {
        // The hex equivalent of the same activity: the third byte of every
        // record.
        let mut v = open(b"0123456789abcdefABCDEFGHIJKLMNOP");
        v.mode = ViewerMode::Hex;
        select_block(&mut v, (2, 2), (18, 4));

        v.side = HexSide::Bytes;
        let bytes_side = v.copy(CopyRequest::Selection, 1024).expect("read");
        assert_eq!(
            bytes_side,
            Copied::Text {
                text: "32 33\n43 44".to_string(),
                bytes: 16,
                note: None,
            }
        );

        // Tab moves the focus and nothing else: the same bytes, read the other
        // way.
        v.side = HexSide::Chars;
        let chars_side = v.copy(CopyRequest::Selection, 1024).expect("read");
        assert_eq!(
            chars_side,
            Copied::Text {
                text: "23\nCD".to_string(),
                bytes: 16,
                note: None,
            }
        );
    }

    #[test]
    fn a_copy_reports_the_bytes_it_read_rather_than_the_span_it_asked_for() {
        // `read_span`'s own rule: "End of file inside the selection: what is
        // there is what is copied, and the message says how many bytes that
        // was." `F2` keeps the selection, so a
        // file truncated under a live selection is an ordinary thing to meet -
        // the log being written in another window - and a message
        // that overstates what reached the clipboard is worse than none.
        let body = Arc::new(std::sync::Mutex::new(b"0123456789abcdefghij\n".to_vec()));
        let shared = Arc::clone(&body);
        let opener: source::Opener = Arc::new(move || {
            let bytes = shared.lock().expect("not poisoned").clone();
            Ok(source::Stream::Seekable(Box::new(std::io::Cursor::new(
                bytes,
            ))))
        });
        let cfg = ViewerConfig::default();
        let mut v =
            Viewer::open(ViewerId(1), "log", None, opener, Some(21), &cfg).expect("open in memory");
        v.cursor_enabled = true;
        select(&mut v, 0, 15, SelectKind::Linear);
        assert_eq!(
            v.copy(CopyRequest::Selection, 1 << 20).expect("copy"),
            Copied::Text {
                text: "0123456789abcde".to_string(),
                bytes: 15,
                note: None,
            }
        );

        // Truncated underneath, and `F2` keeps the selection pointing at bytes
        // the file no longer has.
        body.lock().expect("not poisoned").clear();
        body.lock()
            .expect("not poisoned")
            .extend_from_slice(b"012\n");
        v.reload().expect("reload");
        match v.copy(CopyRequest::Selection, 1 << 20).expect("copy") {
            Copied::Text { text, bytes, .. } => {
                assert_eq!(text, "012\n");
                assert_eq!(
                    bytes,
                    text.len() as u64,
                    "the message says what reached the clipboard"
                );
            }
            other => panic!("refused: {other:?}"),
        }
    }

    #[test]
    fn a_queued_copy_is_taken_once() {
        // `dispatch` queues and the event loop performs,
        // and `InputRoute` stops the drain on the queue so two copies cannot be
        // collapsed into one frame.
        let mut v = open(b"hello");
        assert_eq!(v.take_copy_request(), None);
        v.request_copy(CopyRequest::Selection);
        assert_eq!(v.take_copy_request(), Some(CopyRequest::Selection));
        assert_eq!(v.take_copy_request(), None);
    }
}
