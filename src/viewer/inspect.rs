//! The byte at the cursor, read every way at once.
//!
//! [`copy::interpretations`](super::copy::interpretations) answers a different
//! question: it reads a **selection** of exactly one, two, four or eight bytes
//! and puts one line in the status line. That is the right shape for "what is
//! this run of bytes", and the wrong shape for "what is *here*", which is the
//! question someone walking a header with the arrow keys is asking. Selecting
//! four bytes to find out that they are not the four you wanted is a slow way
//! to ask it.
//!
//! So this reads from the cursor forward and gives every width at once, with no
//! selection and nothing to set up. The two agree where they overlap, because
//! the arithmetic is the same and lives here.
//!
//! # What is offered, and what is not
//!
//! Widths one, two, four and eight, each unsigned and signed, and each in both
//! byte orders where the width has one. Then `f32`, `f64`, and the two
//! spellings of a timestamp that turn up in file formats. A width is offered
//! only where the file has that many bytes left; near the end the list gets
//! shorter rather than showing a number padded with imagination.
//!
//! Byte order is always given both ways, even above `group = 8` where the
//! configuration has declared one. The status line's reading omits the
//! rejected order deliberately - it is reporting the selection under the rules
//! in force - but this is the panel a person opens *because* they do not know
//! which way round the file is, and answering only in the declared order would
//! withhold the half they came for.

use chrono::{DateTime, Utc};

use crate::viewer::Row;

/// How many bytes the widest reading needs.
pub const SPAN: usize = 8;

/// One row of the panel: what it is, and what it reads as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// `u32 (LE)`, `f64 (BE)`, `time`.
    pub label: String,
    /// The value, already rendered.
    pub value: String,
}

impl Reading {
    fn new(label: &str, value: String) -> Self {
        Self {
            label: label.to_string(),
            value,
        }
    }
}

/// The bytes from `cursor` forward, at most [`SPAN`] of them.
///
/// Taken from the laid-out rows rather than by seeking, because the cursor is
/// always on screen and the rows are already in memory: a panel that re-read
/// the file on every arrow key would make the arrow keys the slow part of the
/// program. A run that crosses a row boundary is stitched, and one that runs
/// off the end of the file simply returns short.
#[must_use]
pub fn bytes_at(rows: &[Row], cursor: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(SPAN);
    for row in rows {
        let Row::Hex { offset, bytes, .. } = row else {
            continue;
        };
        let end = offset.saturating_add(bytes.len() as u64);
        if end <= cursor {
            continue;
        }
        let start = usize::try_from(cursor.saturating_sub(*offset)).unwrap_or(usize::MAX);
        let Some(tail) = bytes.get(start.min(bytes.len())..) else {
            continue;
        };
        for b in tail {
            if out.len() == SPAN {
                return out;
            }
            out.push(*b);
        }
    }
    out
}

/// Read `bytes` as a `width`-byte word in the given order.
pub(crate) fn word(bytes: &[u8], width: usize, big_endian: bool) -> Option<u64> {
    let take = bytes.get(..width)?;
    let mut v: u64 = 0;
    if big_endian {
        for b in take {
            v = (v << 8) | u64::from(*b);
        }
    } else {
        for (i, b) in take.iter().enumerate() {
            v |= u64::from(*b) << (i * 8);
        }
    }
    Some(v)
}

/// The signed reading of a `width`-byte word.
pub(crate) fn signed(v: u64, width: usize) -> i64 {
    let bits = width.saturating_mul(8);
    if bits >= 64 {
        return v as i64;
    }
    let shift = 64_u32.saturating_sub(bits as u32);
    ((v << shift) as i64) >> shift
}

/// A float, without an exponent for the ordinary magnitudes and without
/// trailing zeros.
pub(crate) fn float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v.is_sign_negative() { "-inf" } else { "inf" }.to_string();
    }
    let mag = v.abs();
    if v != 0.0 && (mag < 1e-4 || mag >= 1e11) {
        return format!("{v:e}");
    }
    let mut s = format!("{v:.6}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// A Unix timestamp, or `None` where the number is not a plausible one.
///
/// Every 32-bit number is a valid time in the arithmetic sense, and offering
/// "1970-01-01" for a byte that happens to be zero would be noise in the
/// panel rather than information. So a reading is shown only inside a window a
/// file's timestamp actually falls in: 1980 to 2100, which covers the DOS
/// epoch at one end and leaves room at the other.
fn unix_time(secs: i64) -> Option<String> {
    const FROM: i64 = 315_532_800; // 1980-01-01
    const TO: i64 = 4_102_444_800; // 2100-01-01
    if !(FROM..TO).contains(&secs) {
        return None;
    }
    DateTime::<Utc>::from_timestamp(secs, 0).map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// Every reading of the bytes at the cursor, in widening order.
///
/// Empty where there are no bytes, which is what an empty file and a cursor
/// past the end both look like.
#[must_use]
pub fn readings(bytes: &[u8]) -> Vec<Reading> {
    let mut out = Vec::new();
    if bytes.is_empty() {
        return out;
    }

    // A byte is a byte: no order, and the glyph the dump draws beside it.
    if let Some(b) = bytes.first() {
        out.push(Reading::new("u8", b.to_string()));
        // Only where it says something the line above did not, which is the
        // rule the wider widths already follow.
        if *b > 0x7F {
            out.push(Reading::new("i8", (*b as i8).to_string()));
        }
        out.push(Reading::new(
            "char",
            format!("'{}'", super::hex::ascii_glyph(*b)),
        ));
        out.push(Reading::new("bin", format!("{b:08b}")));
    }

    for width in [2_usize, 4, 8] {
        if bytes.len() < width {
            break;
        }
        for big in [false, true] {
            let order = if big { "BE" } else { "LE" };
            let Some(v) = word(bytes, width, big) else {
                continue;
            };
            let bits = width.saturating_mul(8);
            out.push(Reading::new(&format!("u{bits} ({order})"), v.to_string()));
            let s = signed(v, width);
            if s.to_string() != v.to_string() {
                out.push(Reading::new(&format!("i{bits} ({order})"), s.to_string()));
            }
        }
    }

    // Floats, both orders, where the width is there for them.
    if bytes.len() >= 4 {
        for big in [false, true] {
            let order = if big { "BE" } else { "LE" };
            if let Some(v) = word(bytes, 4, big) {
                let f = f32::from_bits(v as u32);
                out.push(Reading::new(&format!("f32 ({order})"), float(f64::from(f))));
            }
        }
    }
    if bytes.len() >= 8 {
        for big in [false, true] {
            let order = if big { "BE" } else { "LE" };
            if let Some(v) = word(bytes, 8, big) {
                out.push(Reading::new(
                    &format!("f64 ({order})"),
                    float(f64::from_bits(v)),
                ));
            }
        }
    }

    // A timestamp, where the number is one.
    if bytes.len() >= 4 {
        for big in [false, true] {
            let order = if big { "BE" } else { "LE" };
            if let Some(v) = word(bytes, 4, big)
                && let Some(t) = unix_time(v as i64)
            {
                out.push(Reading::new(&format!("time ({order})"), t));
            }
        }
    }
    out
}

/// The widest label in a set of readings, for laying the panel out.
#[must_use]
pub fn label_width(readings: &[Reading]) -> usize {
    readings
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod tests;
